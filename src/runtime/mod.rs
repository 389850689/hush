mod flow;
pub mod value;
mod panic;
mod source;
mod lib;
mod mem;

use std::{
	collections::HashMap,
	ops::Deref,
	path::Path,
};

use crate::symbol;
use super::semantic::program;
use value::{
	Array,
	Dict,
	Float,
	Function,
	HushFun,
	RustFun,
	Value,
};
pub use panic::Panic;
use flow::Flow;
use mem::Stack;
use source::SourcePos;


/// A runtime instance to execute Hush programs.
pub struct Runtime<'a> {
	stack: Stack,
	arguments: Vec<(mem::SlotIx, Value)>,
	path: &'static Path,
	interner: &'a mut symbol::Interner,
}


impl<'a> Runtime<'a> {
	/// Execute the given program.
	pub fn eval(
		program: &'static program::Program,
		interner: &'a mut symbol::Interner
	) -> Result<Value, Panic> {
		let mut runtime = Self {
			stack: Stack::default(),
			arguments: Vec::new(),
			path: &program.source,
			interner,
		};

		// Global variables.
		let slots: mem::SlotIx = program.root_slots.into();

		runtime.stack.extend(slots.clone())
			.map_err(|_| Panic::stack_overflow(SourcePos::file(runtime.path)))?;

		// Stdlib.
		let std = lib::new();
		runtime.stack.store(mem::SlotIx(0), std);

		// Execute the program.
		let value = match runtime.eval_block(&program.statements)? {
			Flow::Regular(value) => value,
			flow => panic!("invalid flow in root state: {:#?}", flow)
		};

		// Drop global variables.
		runtime.stack.shrink(slots);

		debug_assert!(runtime.stack.is_empty());
		debug_assert!(runtime.arguments.is_empty());

		Ok(value)
	}


	/// Execute a block, returning the value of the last statement, or the corresponding
	/// control flow if returns or breaks are reached.
	fn eval_block(&mut self, block: &'static program::Block) -> Result<Flow, Panic> {
		let mut value = Value::default();

		for statement in block.0.iter() {
			match self.eval_statement(statement)? {
				Flow::Regular(val) => value = val,
				flow => return Ok(flow),
			}
		}

		Ok(Flow::Regular(value))
	}


	/// Execute a literal.
	/// For trivial types, this basically instatiates a corresponding value.
	/// For compound types, sub-expressions are evaluated.
	/// For function types, closed-over variables are captured, if any.
	/// For identifiers, their string is resolved.
	fn eval_literal(
		&mut self,
		literal: &'static program::Literal,
		pos: program::SourcePos
	) -> Result<Flow, Panic> {
		match literal {
			// Nil.
			program::Literal::Nil => Ok(Flow::Regular(Value::Nil)),

			// Bool.
			program::Literal::Bool(b) => Ok(Flow::Regular((*b).into())),

			// Int.
			program::Literal::Int(int) => Ok(Flow::Regular((*int).into())),

			// Float.
			program::Literal::Float(float) => Ok(Flow::Regular((*float).into())),

			// Byte.
			program::Literal::Byte(byte) => Ok(Flow::Regular((*byte).into())),

			// String.
			program::Literal::String(string) => Ok(Flow::Regular(string.as_ref().into())),

			// Array.
			program::Literal::Array(exprs) => {
				let mut array = Vec::new();

				for expr in exprs.iter() {
					match self.eval_expr(expr)?.0 {
						Flow::Regular(value) => array.push(value),
						flow => return Ok(flow),
					}
				}

				Ok(Flow::Regular(Array::new(array).into()))
			},

			// Dict.
			program::Literal::Dict(exprs) => {
				let mut dict = HashMap::new();

				for (symbol, expr) in exprs.iter() {
					let key: Value = self.interner
						.resolve(*symbol)
						.expect("unresolved symbol")
						.into();

					match self.eval_expr(expr)?.0 {
						Flow::Regular(value) => dict.insert(key, value),
						flow => return Ok(flow),
					};
				}

				Ok(Flow::Regular(Dict::new(dict).into()))
			}

			// Function.
			program::Literal::Function { params, frame_info, body } => {
				let context = frame_info
					.captures
					.iter()
					.map(
						|capture| (
							self.stack.capture(capture.from.into()),
							capture.to.into(),
						)
					)
					.collect();

				Ok(
					Flow::Regular(
						Function::Hush(
							HushFun {
								params: *params,
								frame_info,
								body,
								context,
								pos: self.pos(pos),
							}
						).into()
					)
				)
			},

			// Identifier.
			program::Literal::Identifier(symbol) => Ok(
				Flow::Regular(
					self.interner
						.resolve(*symbol)
						.expect("unresolved symbol")
						.into()
				)
			),
		}
	}


	/// Execute an expression.
	/// Returns a triple of (flow, expr pos, optional self value) or panic.
	fn eval_expr(
		&mut self, expr: &'static program::Expr
	) -> Result<(Flow, SourcePos, Option<Value>), Panic> {
		macro_rules! regular_expr {
			($expr: expr, $pos: expr) => {
				match self.eval_expr($expr)? {
					(Flow::Regular(value), pos, _) => (value, pos),
					(flow, _, _) => return Ok((flow, $pos, None))
				};
			}
		}

		match expr {
			// Identifier.
			program::Expr::Identifier { slot_ix, pos } => {
				let value = self.stack.fetch(slot_ix.into());
				Ok((Flow::Regular(value), self.pos(*pos), None))
			},

			// Literal.
			program::Expr::Literal { literal, pos } => {
				let flow = self.eval_literal(literal, *pos)?;
				Ok((flow, self.pos(*pos), None))
			},

			// UnaryOp.
			program::Expr::UnaryOp { op, operand, pos } => {
				use program::UnaryOp::{Minus, Not};

				let pos = self.pos(*pos);

				let (value, operand_pos) = regular_expr!(operand, pos);

				let value = match (op, value) {
					(Minus, Value::Float(ref f)) => Ok((-f).into()),
					(Minus, Value::Int(i)) => Ok((-i).into()),
					(Minus, value) => Err(Panic::invalid_operand(value, operand_pos)),

					(Not, Value::Bool(b)) => Ok((!b).into()),
					(Not, value) => Err(Panic::invalid_operand(value, operand_pos)),
				}?;

				Ok((Flow::Regular(value), pos, None))
			}

			// BinaryOp.
			program::Expr::BinaryOp { left, op, right, pos } => {
				use program::BinaryOp::*;
				use std::ops::{Add, Sub, Mul, Div, Rem};

				let pos = self.pos(*pos);

				let (left, left_pos) = regular_expr!(left, pos);

				let value = if matches!(op, And | Or) { // Short circuit operators.
					match (left, op) {
						(Value::Bool(false), And) => Value::Bool(false),
						(Value::Bool(true), Or) => Value::Bool(true),

						(Value::Bool(_), _) => {
							let (right, right_pos) = regular_expr!(right, pos);
							match right {
								right @ Value::Bool(_) => right,
								right => return Err(Panic::invalid_operand(right, right_pos)),
							}
						}

						(left, _) => return Err(Panic::invalid_operand(left, left_pos)),
					}
				} else {
					let (right, right_pos) = regular_expr!(right, pos);

					macro_rules! arith_operator {
						($left: expr, $right: expr, $op_float: expr, $op_int: ident, $err_int: expr) => {
							match ($left, $right) {
								// int + int
								(Value::Int(int1), Value::Int(int2)) => {
									let val = int1.$op_int(int2).ok_or($err_int)?;
									Value::Int(val)
								},

								// float + int, int + float
								(Value::Int(int), Value::Float(ref float))
									| (Value::Float(ref float), Value::Int(int)) => {
										let val = $op_float(float.clone(), int.into());
										Value::Float(val)
									},

								// ? + ?
								(left, right) => {
									return Err(
										if matches!(left, Value::Int(_) | Value::Float(_)) {
											Panic::invalid_operand(right, right_pos)
										} else {
											Panic::invalid_operand(left, left_pos)
										}
									)
								},
							}
						}
					}

					match (left, op, right) {
						(left, Plus, right) => arith_operator!(
							left, right,
							Add::add,
							checked_add,
							Panic::integer_overflow(pos.clone())
						),

						(left, Minus, right) => arith_operator!(
							left, right,
							Sub::sub,
							checked_sub,
							Panic::integer_overflow(pos.clone())
						),

						(left, Times, right) => arith_operator!(
							left, right,
							Mul::mul,
							checked_mul,
							Panic::integer_overflow(pos.clone())
						),

						(left, Div, right) => arith_operator!(
							left, right,
							Div::div,
							checked_div,
							Panic::division_by_zero(pos.clone()) // TODO: this can be caused by overflow too.
						),

						(left, Mod, right) => arith_operator!(
							left, right,
							Rem::rem,
							checked_rem,
							Panic::division_by_zero(pos.clone()) // TODO: this can be caused by overflow too.
						),

						(left, Equals, right) => Value::Bool(left == right),
						(left, NotEquals, right) => Value::Bool(left != right),

						(Value::String(ref str1), Concat, Value::String(ref str2)) => {
							let string: Vec<u8> =
								[
									str1.deref().as_ref(),
									str2.deref().as_ref()
								]
								.concat();

							string.into_boxed_slice().into()
						}

						// TODO: relational.

						(left, _, _) => return Err(Panic::invalid_operand(left, left_pos)),
					}
				};

				Ok((Flow::Regular(value), pos, None))
			}

			// If.
			program::Expr::If { condition, then, otherwise, pos } => {
				let pos = self.pos(*pos);

				let condition = match self.eval_expr(condition)? {
					(Flow::Regular(Value::Bool(b)), _, _) => b,
					(Flow::Regular(value), pos, _) => return Err(Panic::invalid_condition(value, pos)),
					(flow, _, _) => return Ok((flow, pos, None))
				};

				let value = if condition {
					self.eval_block(then)
				} else {
					self.eval_block(otherwise)
				}?;

				Ok((value, pos, None))
			}

			// Access.
			program::Expr::Access { object, field, pos } => {
				let pos = self.pos(*pos);

				let (obj, obj_pos) = regular_expr!(object, pos);
				let (field, field_pos) = regular_expr!(field, pos);

				let value = match (&obj, field) {
					(&Value::Dict(ref dict), field) => dict
						.get(&field)
						.map_err(|_| Panic::index_out_of_bounds(field, field_pos)),

					(&Value::Array(ref array), Value::Int(ix)) => array
						.index(ix)
						.map_err(|_| Panic::index_out_of_bounds(Value::Int(ix), field_pos)),

					(Value::Array(_), field) => Err(Panic::invalid_operand(field, field_pos)),

					(_, _) => return Err(Panic::invalid_operand(obj, obj_pos)),
				}?;

				Ok((Flow::Regular(value), pos, Some(obj)))
			}

			// Call.
			program::Expr::Call { function, args, pos } => {
				let pos = self.pos(*pos);

				// Eval function.
				let (function, obj) = match self.eval_expr(function)? {
					(Flow::Regular(Value::Function(ref fun)), _, obj) => (fun.clone(), obj),
					(Flow::Regular(value), pos, _) => return Err(Panic::invalid_call(value, pos)),
					(flow, _, _) => return Ok((flow, pos, None)),
				};

				// Eval arguments.
				for (ix, expr) in args.iter().enumerate() {
					let slot_ix = mem::SlotIx(ix as u32);

					match self.eval_expr(expr)? {
						(Flow::Regular(value), _, _) => self.arguments.push((slot_ix, value)),
						(flow, _, _) => {
							self.arguments.clear();
							return Ok((flow, pos, None));
						}
					}
				}

				let value = self.call(obj, function.deref(), pos.clone())?;

				Ok((Flow::Regular(value), pos, None))
			}

			// CommandBlock.
			program::Expr::CommandBlock { block, pos } => todo!(),
		}
	}


	/// Execute a statement.
	fn eval_statement(&mut self, statement: &'static program::Statement) -> Result<Flow, Panic> {
		match statement {
			// Assign.
			program::Statement::Assign { left, right } => {
				let value = match self.eval_expr(right)?.0 {
					Flow::Regular(value) => value,
					flow => return Ok(flow),
				};

				match left {
					program::Lvalue::Identifier { slot_ix, .. } => self.stack.store(slot_ix.into(), value),

					program::Lvalue::Access { object, field, pos } => {
						let (obj, obj_pos) = match self.eval_expr(object)? {
							(Flow::Regular(obj), pos, _) => (obj, pos),
							(flow, _, _) => return Ok(flow),
						};

						let (field, field_pos) = match self.eval_expr(field)? {
							(Flow::Regular(field), pos, _) => (field, pos),
							(flow, _, _) => return Ok(flow),
						};

						match (obj, field) {
							(Value::Dict(ref dict), field) => dict.insert(field, value),

							(Value::Array(ref array), Value::Int(ix)) if ix >= array.len() => return Err(
								Panic::index_out_of_bounds(Value::Int(ix), field_pos)
							),

							(Value::Array(ref array), Value::Int(ix)) => array
								.deref()
								.set(ix, value)
								.map_err(|_| Panic::index_out_of_bounds(Value::Int(ix), self.pos(*pos)))?,

							(Value::Array(_), field) => return Err(Panic::invalid_operand(field, field_pos)),

							(obj, _) => return Err(Panic::invalid_operand(obj, obj_pos)),
						};
					}
				}

				Ok(Flow::Regular(Value::default()))
			}

			// Return.
			program::Statement::Return { expr } => {
				match self.eval_expr(expr)?.0 {
					Flow::Regular(value) => Ok(Flow::Return(value)),
					flow => Ok(flow),
				}
			}

			// Break.
			program::Statement::Break => Ok(Flow::Break),

			// While.
			program::Statement::While { condition, block } => {
				loop {
					let condition = match self.eval_expr(condition)? {
						(Flow::Regular(Value::Bool(b)), _, _) => b,
						(Flow::Regular(value), pos, _) => return Err(Panic::invalid_condition(value, pos)),
						(flow, _, _) => return Ok(flow)
					};

					if !condition {
						break;
					}

					match self.eval_block(block)? {
						Flow::Regular(_) => (),
						flow @ Flow::Return(_) => return Ok(flow),
						Flow::Break => break,
					}
				}

				Ok(Flow::Regular(Value::default()))
			}

			// For.
			program::Statement::For { slot_ix, expr, block } => {
				thread_local! {
					static FINISHED: Value = "finished".into();
					static VALUE: Value = "value".into();
				}

				let slot_ix: mem::SlotIx = slot_ix.into();

				let (iter, pos) = match self.eval_expr(expr)? {
					(Flow::Regular(Value::Function(ref iter)), pos, _) => (iter.clone(), pos),
					(Flow::Regular(value), pos, _) => return Err(Panic::invalid_operand(value, pos)),
					(flow, _, _) => return Ok(flow)
				};

				loop {
					match self.call(None, &iter, pos.clone())? {
						Value::Dict(ref dict) => {
							let finished = FINISHED.with(
								|finished| dict
									.get(finished)
									.map_err(|_| Panic::index_out_of_bounds(finished.copy(), pos.clone()))
							)?;

							match finished {
								Value::Bool(false) => {
									let value = VALUE.with(
										|value| dict
											.get(value)
											.map_err(|_| Panic::index_out_of_bounds(value.copy(), pos.clone()))
									)?;

									self.stack.store(slot_ix.clone(), value);
								},

								Value::Bool(true) => break,

								other => return Err(Panic::invalid_operand(other, pos))
							}

							Value::Nil
						},

						other => return Err(Panic::invalid_operand(other, pos)),
					};

					match self.eval_block(block)? {
						Flow::Regular(_) => (),
						flow @ Flow::Return(_) => return Ok(flow),
						Flow::Break => break,
					}
				}

				Ok(Flow::Regular(Value::default()))
			}

			// Expr.
			program::Statement::Expr(expr) => self
				.eval_expr(expr)
				.map(|(flow, _, _)| flow)
		}
	}


	/// Call the given function.
	/// The arguments are expected to be on the self.arguments vector.
	fn call(
		&mut self,
		obj: Option<Value>,
		function: &Function,
		pos: SourcePos,
	) -> Result<Value, Panic> {
		let args_count = self.arguments.len() as u32;

		// Make sure we clean the arguments vector even when early returning.
		let arguments = self.arguments.drain(..);

		let value = match function {
			Function::Hush(HushFun { params, frame_info, body, context, .. }) => {
				if args_count != *params {
					return Err(Panic::missing_parameters(pos));
				}

				let slots: mem::SlotIx = frame_info.slots.into();
				self.stack.extend(slots.clone())
					.map_err(|_| Panic::stack_overflow(pos))?;

				// Place arguments
				for (slot_ix, value) in arguments {
					self.stack.store(slot_ix, value);
				}

				// Place captured variables.
				for (value, slot_ix) in context.iter().cloned() {
					self.stack.place(slot_ix, value);
				}

				match (obj, frame_info.self_slot) {
					(Some(obj), Some(slot_ix)) => self.stack.store(slot_ix.into(), obj),
					_ => ()
				};

				let value = match self.eval_block(body)? {
					Flow::Regular(value) => value,
					Flow::Return(value) => value,
					Flow::Break => panic!("break outside loop"),
				};

				self.stack.shrink(slots);

				value
			}

			Function::Rust(RustFun { fun, .. }) => {
				let slots = mem::SlotIx(args_count);
				self.stack.extend(slots.clone())
					.map_err(|_| Panic::stack_overflow(pos))?;

				// Place arguments
				for (slot_ix, value) in arguments {
					self.stack.store(slot_ix, value);
				}

				let value = fun(&mut self.stack, slots.clone())?;

				self.stack.shrink(slots);

				value
			}
		};

		Ok(value)
	}


	fn pos(&self, pos: program::SourcePos) -> SourcePos {
		SourcePos::new(pos, self.path)
	}
}