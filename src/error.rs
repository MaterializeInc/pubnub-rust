use error_iter::ErrorIter;
use std::error::Error as StdError;
use thiserror::Error;

/// # Error variants
#[derive(Debug, Error)]
pub enum Error<E>
where
    E: StdError + 'static,
{
    /// Transport error.
    #[error("Transport error")]
    Transport(E),

    /// Invalid UTF-8.
    #[error("Invalid UTF-8")]
    Utf8(#[from] std::str::Utf8Error),

    /// Invalid JSON.
    #[error("Invalid JSON")]
    Json(#[from] json::Error),
}

impl<E> ErrorIter for Error<E> where E: StdError + 'static {}
