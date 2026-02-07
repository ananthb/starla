//! Error types for Starla

use thiserror::Error;

/// Main error type for Atlas probe operations
#[derive(Error, Debug)]
pub enum Error {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Network error: {0}")]
    Network(String),

    #[error("Measurement error: {0}")]
    Measurement(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("SSH error: {0}")]
    Ssh(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Unknown error: {0}")]
    Unknown(String),
}

/// Convenient Result type alias
pub type Result<T> = std::result::Result<T, Error>;
