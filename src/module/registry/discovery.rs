//! Module discovery
//!
//! Scans module directories and discovers available modules.

use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

use crate::module::registry::manifest::ModuleManifest;
use crate::module::traits::ModuleError;
use crate::module::validation::ManifestValidator;

/// Discovered module information
#[derive(Debug, Clone)]
pub struct DiscoveredModule {
    /// Module directory path
    pub directory: PathBuf,
    /// Module manifest
    pub manifest: ModuleManifest,
    /// Path to module binary
    pub binary_path: PathBuf,
}

/// Module discovery scanner
pub struct ModuleDiscovery {
    /// Base directory to scan for modules
    modules_dir: PathBuf,
}

impl ModuleDiscovery {
    /// Create a new module discovery scanner
    pub fn new<P: AsRef<Path>>(modules_dir: P) -> Self {
        Self {
            modules_dir: modules_dir.as_ref().to_path_buf(),
        }
    }

    /// Discover all modules in the modules directory
    pub fn discover_modules(&self) -> Result<Vec<DiscoveredModule>, ModuleError> {
        info!("Discovering modules in {:?}", self.modules_dir);

        if !self.modules_dir.exists() {
            debug!(
                "Modules directory does not exist, creating: {:?}",
                self.modules_dir
            );
            fs::create_dir_all(&self.modules_dir).map_err(|e| {
                ModuleError::OperationError(format!("Failed to create modules directory: {e}"))
            })?;
            return Ok(Vec::new());
        }

        let mut modules = Vec::new();

        // Scan directory for module subdirectories
        let entries = fs::read_dir(&self.modules_dir).map_err(|e| {
            ModuleError::OperationError(format!("Failed to read modules directory: {e}"))
        })?;

        for entry in entries {
            let entry = entry.map_err(|e| {
                ModuleError::OperationError(format!("Failed to read directory entry: {e}"))
            })?;

            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            // Look for module.toml in this directory
            let manifest_path = path.join("module.toml");
            if !manifest_path.exists() {
                debug!("No module.toml found in {:?}, skipping", path);
                continue;
            }

            // Parse manifest
            match ModuleManifest::from_file(&manifest_path) {
                Ok(manifest) => {
                    // Validate manifest
                    let validator = ManifestValidator::new();
                    match validator.validate(&manifest) {
                        crate::module::validation::ValidationResult::Valid => {
                            debug!("Manifest validated for module: {}", manifest.name);
                        }
                        crate::module::validation::ValidationResult::Invalid(errors) => {
                            warn!(
                                "Manifest validation failed for module {}: {:?}",
                                manifest.name, errors
                            );
                            // Continue anyway in Phase 1, but log warning
                        }
                    }

                    // Find module binary
                    let binary_path = self.find_module_binary(&path, &manifest.entry_point)?;

                    modules.push(DiscoveredModule {
                        directory: path,
                        manifest,
                        binary_path,
                    });
                }
                Err(e) => {
                    warn!("Failed to parse manifest in {:?}: {}", path, e);
                    continue;
                }
            }
        }

        info!("Discovered {} modules", modules.len());
        Ok(modules)
    }

    /// Find module binary path
    fn find_module_binary(
        &self,
        module_dir: &Path,
        entry_point: &str,
    ) -> Result<PathBuf, ModuleError> {
        // Try different possible locations
        let candidates = vec![
            module_dir.join(entry_point),
            module_dir.join("target").join("release").join(entry_point),
            module_dir.join("target").join("debug").join(entry_point),
            self.modules_dir.join(entry_point),
        ];

        for candidate in candidates {
            if candidate.exists() && candidate.is_file() {
                // Check if executable (on Unix)
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(metadata) = candidate.metadata() {
                        let perms = metadata.permissions();
                        if perms.mode() & 0o111 != 0 {
                            return Ok(candidate);
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    // On Windows, just check if file exists
                    return Ok(candidate);
                }
            }
        }

        Err(ModuleError::ModuleNotFound(format!(
            "Module binary not found for entry_point: {entry_point} in {module_dir:?}"
        )))
    }

    /// Discover a specific module by name
    pub fn discover_module(&self, module_name: &str) -> Result<DiscoveredModule, ModuleError> {
        let module_dir = self.modules_dir.join(module_name);
        let manifest_path = module_dir.join("module.toml");

        if !manifest_path.exists() {
            return Err(ModuleError::ModuleNotFound(format!(
                "Module {module_name} not found (no module.toml in {module_dir:?})"
            )));
        }

        let manifest = ModuleManifest::from_file(&manifest_path)?;

        // Validate manifest
        let validator = ManifestValidator::new();
        match validator.validate(&manifest) {
            crate::module::validation::ValidationResult::Valid => {
                debug!("Manifest validated: {}", module_name);
            }
            crate::module::validation::ValidationResult::Invalid(errors) => {
                warn!(
                    "Manifest validation failed for module {}: {:?}",
                    module_name, errors
                );
                // Continue anyway in Phase 1, but log warning
                // In Phase 2+, we would reject invalid manifests
            }
        }

        let binary_path = self.find_module_binary(&module_dir, &manifest.entry_point)?;

        Ok(DiscoveredModule {
            directory: module_dir,
            manifest,
            binary_path,
        })
    }
}
