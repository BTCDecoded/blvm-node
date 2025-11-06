//! Stratum V2 Protocol Error Types

use thiserror::Error;

/// Stratum V2 protocol errors
#[derive(Error, Debug)]
pub enum StratumV2Error {
    /// TLV encoding/decoding error
    #[error("TLV encoding error: {0}")]
    TlvEncoding(String),

    /// Message serialization error
    #[error("Message serialization error: {0}")]
    Serialization(String),

    /// Message deserialization error
    #[error("Message deserialization error: {0}")]
    Deserialization(String),

    /// Protocol version mismatch
    #[error("Protocol version mismatch: expected {expected}, got {got}")]
    VersionMismatch { expected: u16, got: u16 },

    /// Invalid message type
    #[error("Invalid message type: {0}")]
    InvalidMessageType(u16),

    /// Connection error
    #[error("Connection error: {0}")]
    Connection(#[from] anyhow::Error),

    /// Network error
    #[error("Network error: {0}")]
    Network(String),

    /// Mining job error
    #[error("Mining job error: {0}")]
    MiningJob(String),

    /// Share validation error
    #[error("Share validation error: {0}")]
    ShareValidation(String),

    /// Configuration error
    #[error("Configuration error: {0}")]
    Configuration(String),
}

/// Result type for Stratum V2 operations
pub type StratumV2Result<T> = Result<T, StratumV2Error>;
