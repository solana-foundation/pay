use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Keystore error: {0}")]
    Backend(String),

    #[error("Invalid keypair: {0}")]
    InvalidKeypair(String),

    #[error("Authentication denied: {0}")]
    AuthDenied(String),

    #[error("Not available on this platform")]
    Unavailable,

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
