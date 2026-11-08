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

        if let Err(host_errors) = self.validate_hosted_declarations(manifest) {
            errors.extend(host_errors);
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
        if !name.chars().next().is_some_and(|c| c.is_alphanumeric()) {
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

    /// `[[services]]` and `http_wants` require `min_node_version`, checked with
    /// `matches_version_constraint` against this crate's version.
    fn validate_hosted_declarations(
        &self,
        manifest: &ModuleManifest,
    ) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        let declares_host =
            !manifest.services.is_empty() || !manifest.network.http_wants.is_empty();
        if declares_host {
            match manifest
                .min_node_version
                .as_deref()
                .map(str::trim)
                .filter(|constraint| !constraint.is_empty())
            {
                None => errors.push(
                    "min_node_version is required when [[services]] or [network].http_wants is set"
                        .to_string(),
                ),
                Some(constraint) => {
                    let node = env!("CARGO_PKG_VERSION");
                    if !crate::module::version_constraint::matches_version_constraint(
                        node, constraint,
                    ) {
                        errors.push(format!(
                            "min_node_version constraint '{constraint}' is not satisfied by node {node}"
                        ));
                    }
                }
            }
        }
        for svc in &manifest.services {
            if !self.is_valid_name(&svc.name) {
                errors.push(format!("Invalid service name: {}", svc.name));
            }
            if svc.kind != "http" && svc.kind != "stream" {
                errors.push(format!(
                    "Invalid service kind '{}' for {} (expected http or stream)",
                    svc.kind, svc.name
                ));
            }
        }
        for old in &manifest.replaces {
            if !self.is_valid_name(old) {
                errors.push(format!("Invalid replaces name: {old}"));
            }
        }
        for api in manifest
            .provides_apis
            .iter()
            .chain(manifest.requires_apis.iter())
        {
            if api.is_empty() || api.contains(['/', '\\']) {
                errors.push(format!("Invalid api contract: {api}"));
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

/// Validate module signatures when declared; unsigned manifests skip verification.
pub fn validate_module_signature(
    manifest: &ModuleManifest,
    binary_path: &std::path::Path,
) -> Result<(), ModuleError> {
    if !manifest.has_signatures() {
        debug!(
            "Module {} has no signatures; skipping signature validation",
            manifest.name
        );
        return Ok(());
    }
    super::signature_verifier::verify_module_signatures(manifest, binary_path)
}
