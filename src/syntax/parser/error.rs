use std::fmt::{self, Display};

use super::{Token, TokenKind};


/// What kind of token the parser was expecting.
#[derive(Debug)]
pub enum Expected {
	Token(TokenKind),
	Message(&'static str),
}


impl Display for Expected {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self {
			Self::Token(token) => write!(f, "'{:?}'", token),
			Self::Message(msg) => write!(f, "{}", msg),
		}
	}
}


#[derive(Debug)]
pub enum Error {
	UnexpectedEof,
	Unexpected {
		token: Token,
		expected: Expected,
	},
}


impl Error {
	pub fn unexpected_eof() -> Self {
		Self::UnexpectedEof
	}


	pub fn unexpected(token: Token, expected: TokenKind) -> Self {
		Self::Unexpected {
			token,
			expected: Expected::Token(expected)
		}
	}


	pub fn unexpected_msg(token: Token, message: &'static str) -> Self {
		Self::Unexpected {
			token,
			expected: Expected::Message(message)
		}
	}
}


impl Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self {
			Self::UnexpectedEof => write!(f, "Error: unexpected end of file"),
			Self::Unexpected { token: Token { token, pos }, expected } => {
				write!(f, "Error at {}: unexpected '{:?}', expected {}", pos, token, expected)
			},
		}
	}
}


impl std::error::Error for Error { }