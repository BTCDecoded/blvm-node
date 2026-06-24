//! Module loader implementation
//!
//! Handles dynamic module loading, initialization, and configuration.
//! Includes cryptographic signature verification for signed modules.

use std::collections::HashMap;
use std::path::Path;
use tracing::{debug, info, warn};

use crate::module::manager::ModuleManager;
use crate::module::registry::discovery::DiscoveredModule;
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

        #[cfg(feature = "governance")]
        {
            let is_wasm =
                discovered.binary_path.extension().and_then(|s| s.to_str()) == Some("wasm");

            if !is_wasm {
                if let Some(registry_url) = manager.registry_url() {
                    let client = reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(60))
                        .user_agent(concat!("blvm-node/", env!("CARGO_PKG_VERSION")))
                        .build()
                        .map_err(|e| ModuleError::op_err("Failed to build HTTP client", e))?;

                    match crate::module::github_release_install::try_fetch_expected_sha_for_native_module(
                        &client,
                        registry_url,
                        &discovered.manifest,
                    )
                    .await
                    {
                        Ok(expected_sha) => {
                            let binary_content = tokio::fs::read(&discovered.binary_path)
                                .await
                                .map_err(|e| {
                                    ModuleError::CryptoError(format!("Failed to read binary: {e}"))
                                })?;
                            use sha2::{Digest, Sha256};
                            let actual = hex::encode(Sha256::digest(&binary_content));
                            if actual != expected_sha {
                                return Err(ModuleError::OperationError(format!(
                                    "SHA256 mismatch for module '{}' (GitHub release checksums): expected {} got {}",
                                    discovered.manifest.name, expected_sha, actual
                                )));
                            }
                            info!(
                                "Module {} binary verified against GitHub release checksums",
                                discovered.manifest.name
                            );
                        }
                        Err(e) => {
                            if let Some(h) = discovered
                                .manifest
                                .binary
                                .as_ref()
                                .and_then(|b| b.hash.as_ref())
                            {
                                crate::module::validation::signature_verifier::verify_stored_binary_hash(
                                    &discovered.manifest,
                                    &discovered.binary_path,
                                    h,
                                )?;
                                warn!(
                                    "Module {}: could not fetch GitHub release checksums ({}); verified against module.toml [binary].hash",
                                    discovered.manifest.name, e
                                );
                            } else {
                                return Err(e);
                            }
                        }
                    }
                } else {
                    Self::verify_manifest_binary_hash_if_present(
                        &discovered.manifest,
                        &discovered.binary_path,
                    )?;
                }
            } else {
                Self::verify_manifest_binary_hash_if_present(
                    &discovered.manifest,
                    &discovered.binary_path,
                )?;
            }
        }

        #[cfg(not(feature = "governance"))]
        {
            Self::verify_manifest_binary_hash_if_present(
                &discovered.manifest,
                &discovered.binary_path,
            )?;
        }

        // Verify signatures if present
        if discovered.manifest.has_signatures() {
            debug!(
                "Module {} has signatures, verifying...",
                discovered.manifest.name
            );
            crate::module::validation::verify_module_signatures(
                &discovered.manifest,
                &discovered.binary_path,
            )?;
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

    /// If the manifest declares `[binary].hash`, ensure the on-disk file matches.
    fn verify_manifest_binary_hash_if_present(
        manifest: &crate::module::registry::manifest::ModuleManifest,
        binary_path: &Path,
    ) -> Result<(), ModuleError> {
        if let Some(bin) = &manifest.binary {
            if let Some(h) = &bin.hash {
                crate::module::validation::signature_verifier::verify_stored_binary_hash(
                    manifest,
                    binary_path,
                    h,
                )?;
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
            .map_err(|e| ModuleError::op_err("Failed to read config file", e))?;

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
}
