//! Security and isolation enforcement for modules
//!
//! Provides permission checking, request validation, resource limits,
//! and cryptographic signature verification to ensure modules cannot
//! compromise node security or consensus.

pub mod permissions;
pub mod signing;
pub mod validator;

pub use permissions::{Permission, PermissionChecker, PermissionSet};
pub use signing::ModuleSigner;
pub use validator::{RequestValidator, ValidationResult};
