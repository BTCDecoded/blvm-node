//! Security and isolation enforcement for modules
//!
//! Provides permission checking, request validation, and resource limits
//! to ensure modules cannot compromise node security or consensus.

pub mod permissions;
pub mod validator;

pub use permissions::{Permission, PermissionChecker, PermissionSet};
pub use validator::{RequestValidator, ValidationResult};
