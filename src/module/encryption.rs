//! Module encryption and decryption
//!
//! Handles encryption/decryption of module binaries with payment-gated access.
//! Uses AES-256-GCM with keys derived from payment_id + module_hash.

use crate::module::traits::ModuleError;
use aes_gcm::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Module encryption handler
pub struct ModuleEncryption;

impl ModuleEncryption {
    /// Create a new module encryption instance
    pub fn new() -> Self {
        Self
    }

    /// Encrypt module binary
    ///
    /// # Arguments
    ///
    /// * `module_binary` - The module binary to encrypt
    /// * `payment_id` - Payment ID (used for key derivation)
    /// * `module_hash` - Module hash (used for key derivation)
    ///
    /// # Returns
    ///
    /// Tuple of (encrypted_data, nonce)
    pub fn encrypt_module(
        &self,
        module_binary: &[u8],
        payment_id: &str,
        module_hash: &[u8; 32],
    ) -> Result<(Vec<u8>, Vec<u8>), ModuleError> {
        // Derive encryption key from payment_id + module_hash
        let key = Self::derive_key(payment_id, module_hash);

        // Create cipher
        let cipher = Aes256Gcm::new(&key.into());

        // Generate random nonce (12 bytes for GCM)
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);

        // Encrypt
        let encrypted = cipher
            .encrypt(&nonce, module_binary)
            .map_err(|e| ModuleError::CryptoError(format!("Encryption failed: {}", e)))?;

        Ok((encrypted, nonce.to_vec()))
    }

    /// Decrypt module binary
    ///
    /// # Arguments
    ///
    /// * `encrypted_data` - The encrypted module binary
    /// * `nonce` - The nonce used for encryption
    /// * `payment_id` - Payment ID (used for key derivation)
    /// * `module_hash` - Module hash (used for key derivation)
    ///
    /// # Returns
    ///
    /// Decrypted module binary
    pub fn decrypt_module(
        &self,
        encrypted_data: &[u8],
        nonce: &[u8],
        payment_id: &str,
        module_hash: &[u8; 32],
    ) -> Result<Vec<u8>, ModuleError> {
        // Derive decryption key from payment_id + module_hash
        let key = Self::derive_key(payment_id, module_hash);

        // Create cipher
        let cipher = Aes256Gcm::new(&key.into());

        // Convert nonce to Nonce type
        let nonce = Nonce::from_slice(nonce);

        // Decrypt
        let decrypted = cipher
            .decrypt(nonce, encrypted_data)
            .map_err(|e| ModuleError::CryptoError(format!("Decryption failed: {}", e)))?;

        Ok(decrypted)
    }

    /// Derive encryption key from payment_id + module_hash using HKDF
    fn derive_key(payment_id: &str, module_hash: &[u8; 32]) -> [u8; 32] {
        // Create input: payment_id || module_hash
        let mut input = Vec::with_capacity(payment_id.len() + 32);
        input.extend_from_slice(payment_id.as_bytes());
        input.extend_from_slice(module_hash);

        // Use HKDF-SHA256 to derive 32-byte key
        let hk = Hkdf::<Sha256>::new(None, &input);
        let mut key = [0u8; 32];
        hk.expand(b"module_encryption_key", &mut key)
            .expect("HKDF expansion failed");

        key
    }
}

impl Default for ModuleEncryption {
    fn default() -> Self {
        Self::new()
    }
}

/// Encrypted module metadata
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EncryptedModuleMetadata {
    /// Payment ID
    pub payment_id: String,
    /// Module hash
    #[serde(with = "serde_bytes")]
    pub module_hash: Vec<u8>,
    /// Encryption nonce
    #[serde(with = "serde_bytes")]
    pub nonce: Vec<u8>,
    /// Timestamp when encrypted
    pub encrypted_at: u64,
    /// Payment method used
    pub payment_method: String, // "on-chain" | "lightning" | "ctv"
}

/// Store encrypted module to disk
pub async fn store_encrypted_module(
    modules_dir: &Path,
    module_name: &str,
    encrypted_binary: &[u8],
    metadata: &EncryptedModuleMetadata,
) -> Result<PathBuf, ModuleError> {
    use std::fs;
    use tokio::io::AsyncWriteExt;

    // Create encrypted modules directory
    let encrypted_dir = modules_dir.join("encrypted");
    fs::create_dir_all(&encrypted_dir).map_err(|e| {
        ModuleError::OperationError(format!(
            "Failed to create encrypted modules directory: {}",
            e
        ))
    })?;

    // Create module directory
    let module_dir = encrypted_dir.join(module_name);
    fs::create_dir_all(&module_dir).map_err(|e| {
        ModuleError::OperationError(format!("Failed to create module directory: {}", e))
    })?;

    // Write encrypted binary
    let binary_path = module_dir.join("encrypted_binary");
    let mut file = tokio::fs::File::create(&binary_path).await.map_err(|e| {
        ModuleError::OperationError(format!("Failed to create encrypted binary file: {}", e))
    })?;
    file.write_all(encrypted_binary).await.map_err(|e| {
        ModuleError::OperationError(format!("Failed to write encrypted binary: {}", e))
    })?;

    // Write metadata
    let metadata_path = module_dir.join("metadata.json");
    let metadata_json = serde_json::to_string_pretty(metadata)
        .map_err(|e| ModuleError::OperationError(format!("Failed to serialize metadata: {}", e)))?;
    fs::write(&metadata_path, metadata_json)
        .map_err(|e| ModuleError::OperationError(format!("Failed to write metadata: {}", e)))?;

    Ok(binary_path)
}

/// Load encrypted module from disk
pub async fn load_encrypted_module(
    modules_dir: &Path,
    module_name: &str,
) -> Result<(Vec<u8>, EncryptedModuleMetadata), ModuleError> {
    use std::fs;

    let module_dir = modules_dir.join("encrypted").join(module_name);

    // Load metadata
    let metadata_path = module_dir.join("metadata.json");
    let metadata_json = fs::read_to_string(&metadata_path)
        .map_err(|e| ModuleError::OperationError(format!("Failed to read metadata: {}", e)))?;
    let metadata: EncryptedModuleMetadata = serde_json::from_str(&metadata_json)
        .map_err(|e| ModuleError::OperationError(format!("Failed to parse metadata: {}", e)))?;

    // Load encrypted binary
    let binary_path = module_dir.join("encrypted_binary");
    let encrypted_binary = fs::read(&binary_path).map_err(|e| {
        ModuleError::OperationError(format!("Failed to read encrypted binary: {}", e))
    })?;

    Ok((encrypted_binary, metadata))
}

/// Store decrypted module to disk
pub async fn store_decrypted_module(
    modules_dir: &Path,
    module_name: &str,
    decrypted_binary: &[u8],
    manifest: &[u8],
) -> Result<PathBuf, ModuleError> {
    use std::fs;
    use tokio::io::AsyncWriteExt;

    // Create decrypted modules directory
    let decrypted_dir = modules_dir.join("decrypted");
    fs::create_dir_all(&decrypted_dir).map_err(|e| {
        ModuleError::OperationError(format!(
            "Failed to create decrypted modules directory: {}",
            e
        ))
    })?;

    // Create module directory
    let module_dir = decrypted_dir.join(module_name);
    fs::create_dir_all(&module_dir).map_err(|e| {
        ModuleError::OperationError(format!("Failed to create module directory: {}", e))
    })?;

    // Write manifest
    let manifest_path = module_dir.join("module.toml");
    fs::write(&manifest_path, manifest)
        .map_err(|e| ModuleError::OperationError(format!("Failed to write manifest: {}", e)))?;

    // Write decrypted binary
    let binary_path = module_dir.join(module_name);
    let mut file = tokio::fs::File::create(&binary_path).await.map_err(|e| {
        ModuleError::OperationError(format!("Failed to create decrypted binary file: {}", e))
    })?;
    file.write_all(decrypted_binary).await.map_err(|e| {
        ModuleError::OperationError(format!("Failed to write decrypted binary: {}", e))
    })?;

    // Make binary executable (Unix)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = file
            .metadata()
            .await
            .map_err(|e| {
                ModuleError::OperationError(format!("Failed to get file metadata: {}", e))
            })?
            .permissions();
        perms.set_mode(0o755);
        tokio::fs::set_permissions(&binary_path, perms)
            .await
            .map_err(|e| {
                ModuleError::OperationError(format!("Failed to set file permissions: {}", e))
            })?;
    }

    Ok(binary_path)
}
