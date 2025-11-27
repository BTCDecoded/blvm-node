//! Module loader implementation
//!
//! Handles dynamic module loading, initialization, and configuration.
//! Includes cryptographic signature verification for signed modules.

use std::collections::HashMap;
use std::path::Path;
use tracing::{debug, info, warn};

use crate::module::manager::ModuleManager;
use crate::module::registry::discovery::DiscoveredModule;
use crate::module::security::signing::ModuleSigner;
use crate::module::traits::ModuleError;

/// Module loader for loading and initializing modules
pub struct ModuleLoader;

impl ModuleLoader {
    /// Load a discovered module
    pub async fn load_discovered_module(
        manager: &mut ModuleManager,
        discovered: &DiscoveredModule,
        config: HashMap<String, String>,
    ) -> Result<(), ModuleError> {
        info!("Loading module: {}", discovered.manifest.name);

        // Verify signatures if present
        if discovered.manifest.has_signatures() {
            debug!(
                "Module {} has signatures, verifying...",
                discovered.manifest.name
            );
            Self::verify_module_signatures(&discovered.manifest, &discovered.binary_path)?;
            info!("Module {} signatures verified", discovered.manifest.name);
        } else {
            warn!(
                "Module {} has no signatures - loading unsigned module (not recommended)",
                discovered.manifest.name
            );
        }

        let metadata = discovered.manifest.to_metadata();

        manager
            .load_module(
                &discovered.manifest.name,
                &discovered.binary_path,
                metadata,
                config,
            )
            .await
    }

    /// Verify module signatures (manifest and binary)
    fn verify_module_signatures(
        manifest: &crate::module::registry::manifest::ModuleManifest,
        binary_path: &Path,
    ) -> Result<(), ModuleError> {
        let signer = ModuleSigner::new();

        // Verify manifest signatures
        if let Some(_sig_section) = &manifest.signatures {
            // Find manifest file (should be in the same directory as binary)
            let manifest_path = binary_path
                .parent()
                .ok_or_else(|| ModuleError::CryptoError("Binary path has no parent".to_string()))?
                .join("module.toml");

            // Read raw manifest content for signature verification
            // Note: We need to read the file before signatures are parsed out
            // For now, we'll read it and verify - in a full implementation,
            // we'd need to handle the signature section specially
            let manifest_content = std::fs::read_to_string(&manifest_path).map_err(|e| {
                ModuleError::CryptoError(format!("Failed to read manifest file: {e}"))
            })?;

            // Remove signature section from content for verification
            // (signatures are over the content without the signature section)
            // This is a simplified approach - in production, we'd need proper TOML manipulation
            let content_for_verification = Self::remove_signature_section(&manifest_content);

            let signatures = manifest.get_signatures();
            let public_keys = manifest.get_public_keys();
            let threshold = manifest.get_threshold().ok_or_else(|| {
                ModuleError::CryptoError("Signature threshold not specified".to_string())
            })?;

            let valid = signer.verify_manifest(
                content_for_verification.as_bytes(),
                &signatures,
                &public_keys,
                threshold,
            )?;

            if !valid {
                return Err(ModuleError::CryptoError(format!(
                    "Manifest signature verification failed for module {} (required {}-of-{})",
                    manifest.name, threshold.0, threshold.1
                )));
            }

            debug!("Manifest signatures verified for module {}", manifest.name);
        }

        // Verify binary hash and signatures if present
        if let Some(binary_section) = &manifest.binary {
            if binary_path.exists() {
                let binary_content = std::fs::read(binary_path)
                    .map_err(|e| ModuleError::CryptoError(format!("Failed to read binary: {e}")))?;

                // Verify binary hash if specified
                if let Some(expected_hash) = &binary_section.hash {
                    use sha2::{Digest, Sha256};
                    let actual_hash = hex::encode(Sha256::digest(&binary_content));
                    if actual_hash != expected_hash.trim_start_matches("sha256:") {
                        return Err(ModuleError::CryptoError(format!(
                            "Binary hash mismatch for module {}: expected {}, got {}",
                            manifest.name, expected_hash, actual_hash
                        )));
                    }
                    debug!("Binary hash verified for module {}", manifest.name);
                }

                // Verify binary signatures if present
                if let Some(_sig_section) = &manifest.signatures {
                    let signatures = manifest.get_signatures();
                    let public_keys = manifest.get_public_keys();
                    let threshold = manifest.get_threshold().ok_or_else(|| {
                        ModuleError::CryptoError("Signature threshold not specified".to_string())
                    })?;

                    let valid = signer.verify_binary(
                        &binary_content,
                        &signatures,
                        &public_keys,
                        threshold,
                    )?;

                    if !valid {
                        return Err(ModuleError::CryptoError(format!(
                            "Binary signature verification failed for module {} (required {}-of-{})",
                            manifest.name, threshold.0, threshold.1
                        )));
                    }

                    debug!("Binary signatures verified for module {}", manifest.name);
                }
            }
        }

        Ok(())
    }

    /// Load all modules in dependency order
    pub async fn load_modules_in_order(
        manager: &mut ModuleManager,
        discovered_modules: &[DiscoveredModule],
        load_order: &[String],
        module_configs: &HashMap<String, HashMap<String, String>>,
    ) -> Result<(), ModuleError> {
        for module_name in load_order {
            // Find the discovered module
            let discovered = discovered_modules
                .iter()
                .find(|m| m.manifest.name == *module_name)
                .ok_or_else(|| ModuleError::ModuleNotFound(module_name.clone()))?;

            // Get module config (or empty default)
            let config = module_configs.get(module_name).cloned().unwrap_or_default();

            // Load the module
            Self::load_discovered_module(manager, discovered, config).await?;
        }

        Ok(())
    }

    /// Load module configuration from file
    pub fn load_module_config<P: AsRef<Path>>(
        module_name: &str,
        config_path: P,
    ) -> Result<HashMap<String, String>, ModuleError> {
        if !config_path.as_ref().exists() {
            debug!("No config file for module {}, using defaults", module_name);
            return Ok(HashMap::new());
        }

        // Try TOML first
        if let Ok(contents) = std::fs::read_to_string(&config_path) {
            if let Ok(config) = toml::from_str::<HashMap<String, toml::Value>>(&contents) {
                // Convert TOML values to strings
                let mut string_config = HashMap::new();
                for (key, value) in config {
                    let value_str = match value {
                        toml::Value::String(s) => s,
                        toml::Value::Integer(i) => i.to_string(),
                        toml::Value::Float(f) => f.to_string(),
                        toml::Value::Boolean(b) => b.to_string(),
                        toml::Value::Array(arr) => arr
                            .iter()
                            .map(|v| v.to_string())
                            .collect::<Vec<_>>()
                            .join(","),
                        toml::Value::Table(map) => {
                            // Nested tables become dot-notation keys
                            let mut result = Vec::new();
                            for (subkey, subvalue) in map {
                                result.push(format!("{key}.{subkey}"));
                                result.push(subvalue.to_string());
                            }
                            result.join(",")
                        }
                        toml::Value::Datetime(dt) => dt.to_string(),
                    };
                    string_config.insert(key, value_str);
                }
                return Ok(string_config);
            }
        }

        // If TOML parsing failed, try simple key=value format
        let contents = std::fs::read_to_string(&config_path)
            .map_err(|e| ModuleError::OperationError(format!("Failed to read config file: {e}")))?;

        let mut config = HashMap::new();
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if let Some((key, value)) = line.split_once('=') {
                config.insert(key.trim().to_string(), value.trim().to_string());
            }
        }

        Ok(config)
    }

    /// Flatten TOML value to string hashmap
    fn flatten_toml_value(
        prefix: String,
        value: &toml::Value,
        result: &mut HashMap<String, String>,
    ) {
        use toml::Value;

        match value {
            Value::String(s) => {
                if !prefix.is_empty() {
                    result.insert(prefix, s.clone());
                }
            }
            Value::Integer(i) => {
                result.insert(prefix, i.to_string());
            }
            Value::Float(f) => {
                result.insert(prefix, f.to_string());
            }
            Value::Boolean(b) => {
                result.insert(prefix, b.to_string());
            }
            Value::Array(arr) => {
                let values: Vec<String> = arr
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        _ => v.to_string(),
                    })
                    .collect();
                result.insert(prefix, values.join(","));
            }
            Value::Table(table) => {
                for (key, val) in table {
                    let new_prefix = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    Self::flatten_toml_value(new_prefix, val, result);
                }
            }
            Value::Datetime(dt) => {
                result.insert(prefix, dt.to_string());
            }
        }
    }

    /// Remove signature section from TOML content for verification
    ///
    /// Signatures are computed over the manifest content without the signature section itself.
    /// This is a simplified implementation - in production, proper TOML parsing/manipulation would be used.
    fn remove_signature_section(content: &str) -> String {
        // Simple approach: remove [signatures] section
        // This is a placeholder - proper implementation would use TOML manipulation
        let lines: Vec<&str> = content.lines().collect();
        let mut in_signatures = false;
        let mut result = Vec::new();

        for line in lines.iter() {
            let trimmed = line.trim();
            if trimmed == "[signatures]" {
                in_signatures = true;
                continue;
            }
            if in_signatures {
                if trimmed.starts_with('[') && trimmed.ends_with(']') {
                    // New section started
                    in_signatures = false;
                    result.push(*line);
                } else if trimmed.is_empty()
                    && result
                        .last()
                        .map(|l: &&str| l.trim().is_empty())
                        .unwrap_or(false)
                {
                    // Skip empty lines in signatures section
                    continue;
                } else if !trimmed.starts_with('#') && trimmed.contains('=') {
                    // Skip signature entries (key=value lines)
                    continue;
                } else {
                    // Keep other content
                    result.push(*line);
                }
            } else {
                result.push(*line);
            }
        }

        result.join("\n")
    }
}
