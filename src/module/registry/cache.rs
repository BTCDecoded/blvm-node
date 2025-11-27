//! Local cache for verified modules
//!
//! Provides local-first caching of verified modules for offline operation
//! and faster subsequent loads.

use crate::module::traits::ModuleError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Cached module entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedModule {
    /// Module name
    pub name: String,
    /// Module version
    pub version: String,
    /// Module hash
    pub hash: [u8; 32],
    /// Manifest hash
    pub manifest_hash: [u8; 32],
    /// Binary hash
    pub binary_hash: [u8; 32],
    /// Timestamp when verified
    pub verified_at: u64,
    /// Mirror URLs that verified this module
    pub verified_by: Vec<String>,
    /// Local path to cached module
    pub local_path: PathBuf,
    /// Cache expiration timestamp (optional)
    pub expires_at: Option<u64>,
}

/// Local cache for verified modules
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalCache {
    modules: HashMap<String, CachedModule>,
    registry_state: Option<CachedRegistryState>,
    last_sync: u64,
}

/// Cached registry state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedRegistryState {
    /// Merkle root hash
    pub root_hash: [u8; 32],
    /// Timestamp
    pub timestamp: u64,
    /// Signature (if available)
    pub signature: Option<String>,
}

impl LocalCache {
    /// Create a new empty cache
    pub fn new() -> Self {
        Self {
            modules: HashMap::new(),
            registry_state: None,
            last_sync: 0,
        }
    }

    /// Load cache from disk
    pub fn load<P: AsRef<Path>>(cache_dir: P) -> Result<Self, ModuleError> {
        let cache_dir = cache_dir.as_ref();
        let cache_file = cache_dir.join("registry_cache.json");

        if !cache_file.exists() {
            return Ok(Self::new());
        }

        let contents = std::fs::read_to_string(&cache_file)
            .map_err(|e| ModuleError::OperationError(format!("Failed to read cache file: {e}")))?;

        let cache: LocalCache = serde_json::from_str(&contents)
            .map_err(|e| ModuleError::OperationError(format!("Failed to parse cache file: {e}")))?;

        Ok(cache)
    }

    /// Save cache to disk
    pub fn save<P: AsRef<Path>>(&self, cache_dir: P) -> Result<(), ModuleError> {
        let cache_dir = cache_dir.as_ref();

        // Create cache directory if it doesn't exist
        std::fs::create_dir_all(cache_dir).map_err(|e| {
            ModuleError::OperationError(format!("Failed to create cache directory: {e}"))
        })?;

        let cache_file = cache_dir.join("registry_cache.json");
        let contents = serde_json::to_string_pretty(self)
            .map_err(|e| ModuleError::OperationError(format!("Failed to serialize cache: {e}")))?;

        std::fs::write(&cache_file, contents)
            .map_err(|e| ModuleError::OperationError(format!("Failed to write cache file: {e}")))?;

        Ok(())
    }

    /// Get cached module
    pub fn get(&self, name: &str) -> Option<&CachedModule> {
        self.modules.get(name)
    }

    /// Cache verified module
    pub fn cache(&mut self, module: CachedModule) {
        self.modules.insert(module.name.clone(), module);
    }

    /// Remove cached module
    pub fn remove(&mut self, name: &str) {
        self.modules.remove(name);
    }

    /// Check if module is cached and not expired
    pub fn is_valid(&self, name: &str) -> bool {
        if let Some(cached) = self.modules.get(name) {
            if let Some(expires_at) = cached.expires_at {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                return now < expires_at;
            }
            return true;
        }
        false
    }

    /// Get all cached module names
    pub fn list_modules(&self) -> Vec<String> {
        self.modules.keys().cloned().collect()
    }

    /// Clear expired entries
    pub fn clear_expired(&mut self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        self.modules.retain(|_, cached| {
            if let Some(expires_at) = cached.expires_at {
                now < expires_at
            } else {
                true
            }
        });
    }

    /// Update last sync timestamp
    pub fn update_sync_time(&mut self) {
        self.last_sync = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
    }

    /// Get last sync timestamp
    pub fn last_sync(&self) -> u64 {
        self.last_sync
    }
}

impl Default for LocalCache {
    fn default() -> Self {
        Self::new()
    }
}
