//! Module manifest parsing and validation
//!
//! Handles parsing module.toml manifests and validating module metadata.

use crate::module::traits::{ModuleError, ModuleMetadata};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Module manifest (module.toml structure)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleManifest {
    /// Module name
    pub name: String,
    /// Module version (semantic versioning)
    pub version: String,
    /// Human-readable description
    pub description: Option<String>,
    /// Module author
    pub author: Option<String>,
    /// Capabilities this module declares it can use
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Required dependencies (module names with versions)
    #[serde(default)]
    pub dependencies: HashMap<String, String>,
    /// Module entry point (binary name or path)
    pub entry_point: String,
    /// Module configuration schema (optional)
    #[serde(default)]
    pub config_schema: HashMap<String, String>,
}

impl ModuleManifest {
    /// Load manifest from file
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, ModuleError> {
        let contents = std::fs::read_to_string(path.as_ref()).map_err(|e| {
            ModuleError::InvalidManifest(format!("Failed to read manifest file: {}", e))
        })?;

        let manifest: ModuleManifest = toml::from_str(&contents).map_err(|e| {
            ModuleError::InvalidManifest(format!("Failed to parse manifest TOML: {}", e))
        })?;

        // Validate required fields
        if manifest.name.is_empty() {
            return Err(ModuleError::InvalidManifest(
                "Module name cannot be empty".to_string(),
            ));
        }
        if manifest.entry_point.is_empty() {
            return Err(ModuleError::InvalidManifest(
                "Entry point cannot be empty".to_string(),
            ));
        }

        Ok(manifest)
    }

    /// Convert to ModuleMetadata
    pub fn to_metadata(&self) -> ModuleMetadata {
        ModuleMetadata {
            name: self.name.clone(),
            version: self.version.clone(),
            description: self.description.clone().unwrap_or_default(),
            author: self.author.clone().unwrap_or_default(),
            capabilities: self.capabilities.clone(),
            dependencies: self.dependencies.clone(),
            entry_point: self.entry_point.clone(),
        }
    }
}

impl TryFrom<ModuleManifest> for ModuleMetadata {
    type Error = ModuleError;

    fn try_from(manifest: ModuleManifest) -> Result<Self, Self::Error> {
        Ok(manifest.to_metadata())
    }
}
