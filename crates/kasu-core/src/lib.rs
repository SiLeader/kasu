mod codec;
mod git;
pub use git::*;
mod model;
mod repository;
pub use repository::*;

pub use bstr::BString;
pub use codec::{Canonical, Decoder, Encoder};
pub use model::*;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("permission denied: {0}")]
    Denied(String),
    #[error("invalid data: {0}")]
    Invalid(String),
    #[error("object not found: {0}")]
    NotFound(String),
    #[error("immutable key already contains different bytes: {0}")]
    Collision(String),
    #[error("failed precondition: {0}")]
    Precondition(String),
    #[error("storage: {0}")]
    Storage(String),
}
pub type Result<T> = std::result::Result<T, Error>;

pub fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}
