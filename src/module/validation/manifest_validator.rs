//! Manifest validation framework
//!
//! Validates module manifests for structure, security, and compatibility.

use std::collections::HashMap;
use tracing::{debug, warn};

use crate::module::registry::manifest::ModuleManifest;
use crate::module::traits::ModuleError;

/// Validation result
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationResult {
    /// Manifest is valid
    Valid,
    /// Manifest is invalid with specific errors
    Invalid(Vec<String>),
}

/// Manifest validator
pub struct ManifestValidator {
    /// Required fields for a valid manifest
    required_fields: Vec<&'static str>,
    /// Maximum manifest size (bytes)
    max_manifest_size: usize,
}

impl ManifestValidator {
    /// Create a new manifest validator
    pub fn new() -> Self {
        Self {
            required_fields: vec!["name", "version", "entry_point"],
            max_manifest_size: 64 * 1024, // 64 KB max
        }
    }

    /// Validate a module manifest
    pub fn validate(&self, manifest: &ModuleManifest) -> ValidationResult {
        let mut errors = Vec::new();

        // Validate required fields
        if manifest.name.is_empty() {
            errors.push("Module name cannot be empty".to_string());
        }

        if manifest.version.is_empty() {
            errors.push("Module version cannot be empty".to_string());
        } else if !self.is_valid_version(&manifest.version) {
            errors.push(format!(
                "Invalid version format: {} (expected semantic versioning)",
                manifest.version
            ));
        }

        if manifest.entry_point.is_empty() {
            errors.push("Entry point cannot be empty".to_string());
        }

        // Validate name format (alphanumeric, dashes, underscores)
        if !self.is_valid_name(&manifest.name) {
            errors.push(format!(
                "Invalid module name: {} (must be alphanumeric with dashes/underscores)",
                manifest.name
            ));
        }

        // Validate capabilities/permissions
        if let Err(cap_errors) = self.validate_capabilities(&manifest.capabilities) {
            errors.extend(cap_errors);
        }

        // Validate dependencies
        if let Err(dep_errors) = self.validate_dependencies(&manifest.dependencies) {
            errors.extend(dep_errors);
        }

        if errors.is_empty() {
            debug!("Manifest validation passed for module: {}", manifest.name);
            ValidationResult::Valid
        } else {
            warn!(
                "Manifest validation failed for module {}: {:?}",
                manifest.name, errors
            );
            ValidationResult::Invalid(errors)
        }
    }

    /// Validate module name format
    #[inline]
    fn is_valid_name(&self, name: &str) -> bool {
        // Fast checks first
        if name.is_empty() || name.len() > 64 {
            return false;
        }

        // Must start with alphanumeric
        if !name.chars().next().map_or(false, |c| c.is_alphanumeric()) {
            return false;
        }

        // All characters must be alphanumeric, dash, or underscore
        name.chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    }

    /// Validate version format (semantic versioning)
    ///
    /// Accepts: major.minor[.patch][-prerelease][+build]
    #[inline]
    fn is_valid_version(&self, version: &str) -> bool {
        if version.is_empty() {
            return false;
        }

        // Split on '+' to separate build metadata
        let (base, _build) = if let Some(pos) = version.find('+') {
            (&version[..pos], &version[pos + 1..])
        } else {
            (version, "")
        };

        // Split on '-' to separate prerelease
        let (version_part, _prerelease) = if let Some(pos) = base.find('-') {
            (&base[..pos], &base[pos + 1..])
        } else {
            (base, "")
        };

        // Split version into parts (major.minor.patch)
        let nums: Vec<&str> = version_part.split('.').collect();

        // Must have 2-3 parts (major.minor or major.minor.patch)
        if nums.len() < 2 || nums.len() > 3 {
            return false;
        }

        // Each part must be numeric
        nums.iter().all(|n| {
            !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && n.parse::<u32>().is_ok()
        })
    }

    /// Validate capabilities/permissions
    fn validate_capabilities(&self, capabilities: &[String]) -> Result<(), Vec<String>> {
        use crate::module::security::permissions::parse_permission_string;

        let mut errors = Vec::new();

        for cap in capabilities {
            if parse_permission_string(cap).is_none() {
                errors.push(format!("Unknown capability: {cap}"));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Validate dependencies
    fn validate_dependencies(
        &self,
        dependencies: &HashMap<String, String>,
    ) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        for (dep_name, dep_version) in dependencies {
            // Validate dependency name format
            if !self.is_valid_name(dep_name) {
                errors.push(format!("Invalid dependency name: {dep_name}"));
            }

            // Validate dependency version format (can be version range)
            if !self.is_valid_version_or_range(dep_version) {
                errors.push(format!(
                    "Invalid dependency version format: {dep_version} (for dependency: {dep_name})"
                ));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Validate version or version range
    fn is_valid_version_or_range(&self, version: &str) -> bool {
        // Check if it's a version range (>=, <=, =, ^, ~)
        if version.starts_with(">=")
            || version.starts_with("<=")
            || version.starts_with("==")
            || version.starts_with("^")
            || version.starts_with("~")
        {
            let version_part = version.trim_start_matches(|c: char| {
                c == '>' || c == '=' || c == '^' || c == '~' || c == '<'
            });
            return self.is_valid_version(version_part);
        }

        // Otherwise, just check if it's a valid version
        self.is_valid_version(version)
    }
}

impl Default for ManifestValidator {
    fn default() -> Self {
        Self::new()
    }
}

/// Validate module signature (Phase 2+)
pub fn validate_module_signature(
    _manifest: &ModuleManifest,
    _binary_path: &std::path::Path,
) -> Result<(), ModuleError> {
    // Phase 1: Signature validation disabled (test keys only)
    // Phase 2+: Implement signature verification
    // - Load module public key from manifest
    // - Verify binary signature
    // - Check signature against module registry

    debug!("Module signature validation skipped (Phase 1 - test keys only)");
    Ok(())
}
