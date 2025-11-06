//! Module validation framework
//!
//! Provides manifest validation, security checks, dependency validation,
//! and capability declaration validation.

pub mod manifest_validator;

pub use manifest_validator::{validate_module_signature, ManifestValidator, ValidationResult};
