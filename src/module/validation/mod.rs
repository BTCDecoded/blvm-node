//! Module validation framework
//!
//! Provides manifest validation, security checks, dependency validation,
//! and capability declaration validation.

pub mod manifest_validator;
pub mod signature_verifier;

pub use manifest_validator::{ManifestValidator, ValidationResult, validate_module_signature};
pub use signature_verifier::{manifest_content_without_signatures, verify_module_signatures};
