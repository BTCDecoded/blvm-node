//! Module validation framework
//! 
//! Provides manifest validation, security checks, dependency validation,
//! and capability declaration validation.

pub mod manifest_validator;

pub use manifest_validator::{ManifestValidator, ValidationResult, validate_module_signature};

