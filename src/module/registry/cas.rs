//! Content-Addressable Storage (CAS) for modules
//!
//! Stores modules by their cryptographic hash rather than by name,
//! ensuring content integrity and enabling decentralized distribution.

use crate::module::traits::ModuleError;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Module hash type (SHA256)
pub type ModuleHash = [u8; 32];

/// Content-addressable storage for modules
pub struct ContentAddressableStorage {
    storage_dir: PathBuf,
    index: HashMap<ModuleHash, PathBuf>, // hash → file path
}

impl ContentAddressableStorage {
    /// Create a new CAS instance
    pub fn new<P: AsRef<Path>>(storage_dir: P) -> Result<Self, ModuleError> {
        let storage_dir = storage_dir.as_ref().to_path_buf();

        // Create storage directory if it doesn't exist
        std::fs::create_dir_all(&storage_dir).map_err(|e| {
            ModuleError::OperationError(format!("Failed to create CAS directory: {e}"))
        })?;

        // Load existing index
        let index = Self::load_index(&storage_dir)?;

        Ok(Self { storage_dir, index })
    }

    /// Store content and return its hash
    pub fn store(&mut self, content: &[u8]) -> Result<ModuleHash, ModuleError> {
        let hash = Self::hash_content(content);
        let path = self.storage_dir.join(hex::encode(hash));

        // Write content to file
        std::fs::write(&path, content)
            .map_err(|e| ModuleError::OperationError(format!("Failed to write CAS file: {e}")))?;

        // Update index
        self.index.insert(hash, path);

        // Save index
        self.save_index()?;

        Ok(hash)
    }

    /// Retrieve content by hash
    pub fn get(&self, hash: &ModuleHash) -> Result<Vec<u8>, ModuleError> {
        // Check index first
        if let Some(path) = self.index.get(hash) {
            return std::fs::read(path)
                .map_err(|e| ModuleError::OperationError(format!("Failed to read CAS file: {e}")));
        }

        // Try to read from storage directory (for files not in index)
        let path = self.storage_dir.join(hex::encode(hash));
        if path.exists() {
            return std::fs::read(&path)
                .map_err(|e| ModuleError::OperationError(format!("Failed to read CAS file: {e}")));
        }

        Err(ModuleError::ModuleNotFound(format!(
            "Content not found for hash: {}",
            hex::encode(hash)
        )))
    }

    /// Hash content (SHA256)
    pub fn hash_content(content: &[u8]) -> ModuleHash {
        let mut hasher = Sha256::new();
        hasher.update(content);
        hasher.finalize().into()
    }

    /// Verify content matches hash
    pub fn verify(&self, content: &[u8], expected_hash: &ModuleHash) -> bool {
        let actual_hash = Self::hash_content(content);
        actual_hash == *expected_hash
    }

    /// Check if content exists for hash
    pub fn has(&self, hash: &ModuleHash) -> bool {
        self.index.contains_key(hash) || {
            let path = self.storage_dir.join(hex::encode(hash));
            path.exists()
        }
    }

    /// Load index from disk
    fn load_index(storage_dir: &Path) -> Result<HashMap<ModuleHash, PathBuf>, ModuleError> {
        let index_file = storage_dir.join("index.json");

        if !index_file.exists() {
            return Ok(HashMap::new());
        }

        let contents = std::fs::read_to_string(&index_file)
            .map_err(|e| ModuleError::OperationError(format!("Failed to read index file: {e}")))?;

        let index_data: HashMap<String, String> = serde_json::from_str(&contents)
            .map_err(|e| ModuleError::OperationError(format!("Failed to parse index file: {e}")))?;

        let mut index = HashMap::new();
        for (hash_hex, path_str) in index_data {
            let hash_bytes = hex::decode(&hash_hex)
                .map_err(|e| ModuleError::OperationError(format!("Invalid hash in index: {e}")))?;

            if hash_bytes.len() != 32 {
                continue;
            }

            let mut hash = [0u8; 32];
            hash.copy_from_slice(&hash_bytes);
            index.insert(hash, PathBuf::from(path_str));
        }

        Ok(index)
    }

    /// Save index to disk
    fn save_index(&self) -> Result<(), ModuleError> {
        let index_file = self.storage_dir.join("index.json");

        let index_data: HashMap<String, String> = self
            .index
            .iter()
            .map(|(hash, path)| (hex::encode(hash), path.to_string_lossy().to_string()))
            .collect();

        let contents = serde_json::to_string_pretty(&index_data)
            .map_err(|e| ModuleError::OperationError(format!("Failed to serialize index: {e}")))?;

        std::fs::write(&index_file, contents)
            .map_err(|e| ModuleError::OperationError(format!("Failed to write index file: {e}")))?;

        Ok(())
    }
}
