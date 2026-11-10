//! Registry-specific error types

use crate::module::traits::ModuleError;
use thiserror::Error;

/// Registry operation errors
#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("Module not found: {0}")]
    NotFound(String),

    #[error("Hash mismatch: {0}")]
    HashMismatch(String),

    #[error("Insufficient signatures: required {0}, got {1}")]
    InsufficientSignatures(usize, usize),

    #[error("Invalid signature: {0}")]
    InvalidSignature(String),

    #[error("Network error: {0}")]
    NetworkError(String),

    #[error("Cache error: {0}")]
    CacheError(String),

    #[error("Storage error: {0}")]
    StorageError(String),

    #[error("Verification failed: {0}")]
    VerificationFailed(String),
}

impl From<RegistryError> for crate::module::traits::ModuleError {
    fn from(e: RegistryError) -> Self {
        match e {
            RegistryError::NotFound(msg) => ModuleError::ModuleNotFound(msg),
            RegistryError::HashMismatch(msg) => {
                ModuleError::CryptoError(format!("Hash mismatch: {msg}"))
            }
            RegistryError::InsufficientSignatures(required, got) => ModuleError::CryptoError(
                format!("Insufficient signatures: required {required}, got {got}"),
            ),
            RegistryError::InvalidSignature(msg) => {
                ModuleError::CryptoError(format!("Invalid signature: {msg}"))
            }
            RegistryError::NetworkError(msg) => {
                ModuleError::OperationError(format!("Network error: {msg}"))
            }
            RegistryError::CacheError(msg) => {
                ModuleError::OperationError(format!("Cache error: {msg}"))
            }
            RegistryError::StorageError(msg) => {
                ModuleError::OperationError(format!("Storage error: {msg}"))
            }
            RegistryError::VerificationFailed(msg) => {
                ModuleError::CryptoError(format!("Verification failed: {msg}"))
            }
        }
    }
}
