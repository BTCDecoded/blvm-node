//! Module encryption and decryption
//!
//! Handles encryption/decryption of module binaries with payment-gated access.
//! Uses AES-256-GCM with keys derived from payment_id + module_hash.

use crate::module::traits::ModuleError;
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng},
};
use hkdf::Hkdf;
use sha2::Sha256;
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
        secret: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>), ModuleError> {
        let key = Self::derive_key(secret, payment_id, module_hash)?;

        // Create cipher. `into` moves the key into the cipher.
        let cipher = Aes256Gcm::new(&key.into());

        // Generate random nonce (12 bytes for GCM)
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);

        // Encrypt
        let encrypted = cipher
            .encrypt(&nonce, module_binary)
            .map_err(|e| ModuleError::CryptoError(format!("Encryption failed: {e}")))?;

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
        secret: &[u8],
    ) -> Result<Vec<u8>, ModuleError> {
        let key = Self::derive_key(secret, payment_id, module_hash)?;

        // Create cipher. `into` moves the key into the cipher.
        let cipher = Aes256Gcm::new(&key.into());

        // GCM nonces are 12 bytes. generic-array 0.14's from_slice is deprecated.
        let nonce_bytes: [u8; 12] = nonce.try_into().map_err(|_| {
            ModuleError::CryptoError(format!("nonce must be 12 bytes, got {}", nonce.len()))
        })?;
        let nonce = Nonce::from(nonce_bytes);

        // Decrypt
        let decrypted = cipher
            .decrypt(&nonce, encrypted_data)
            .map_err(|e| ModuleError::CryptoError(format!("Decryption failed: {e}")))?;

        Ok(decrypted)
    }

    /// HKDF-SHA256. `secret` is the node master key. `payment_id` and
    /// `module_hash` are info, not the key. Both of those are stored beside
    /// the ciphertext, so they cannot be the IKM.
    fn derive_key(
        secret: &[u8],
        payment_id: &str,
        module_hash: &[u8; 32],
    ) -> Result<[u8; 32], ModuleError> {
        if secret.is_empty() {
            return Err(ModuleError::CryptoError(
                "module encryption requires a node master key".to_string(),
            ));
        }
        let mut info = Vec::with_capacity(24 + payment_id.len() + 32);
        info.extend_from_slice(b"module_encryption_key");
        info.extend_from_slice(payment_id.as_bytes());
        info.extend_from_slice(module_hash);
        let hk = Hkdf::<Sha256>::new(None, secret);
        let mut key = [0u8; 32];
        hk.expand(&info, &mut key)
            .map_err(|_| ModuleError::CryptoError("HKDF expansion failed".to_string()))?;
        Ok(key)
    }
}

/// Load or create the 32-byte module master key.
///
/// The file is `modules_dir/.master_key`, mode `0600`. It is not written into
/// an encrypted module directory.
pub fn load_or_create_master_key(modules_dir: &Path) -> Result<[u8; 32], ModuleError> {
    use std::io::Write;

    let path = modules_dir.join(".master_key");
    if path.is_file() {
        let bytes = std::fs::read(&path)
            .map_err(|e| ModuleError::op_err("Failed to read module master key", e))?;
        let key: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            ModuleError::CryptoError(format!(
                "module master key must be 32 bytes, got {}",
                bytes.len()
            ))
        })?;
        return Ok(key);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ModuleError::op_err("Failed to create module key directory", e))?;
    }
    let mut key = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut key);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|e| ModuleError::op_err("Failed to create module master key", e))?;
    file.write_all(&key)
        .map_err(|e| ModuleError::op_err("Failed to write module master key", e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| ModuleError::op_err("Failed to set module master key mode", e))?;
    }
    Ok(key)
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
    fs::create_dir_all(&encrypted_dir)
        .map_err(|e| ModuleError::op_err("Failed to create encrypted modules directory", e))?;

    // Create module directory
    let module_dir = encrypted_dir.join(module_name);
    fs::create_dir_all(&module_dir)
        .map_err(|e| ModuleError::op_err("Failed to create module directory", e))?;

    // Write encrypted binary
    let binary_path = module_dir.join("encrypted_binary");
    let mut file = tokio::fs::File::create(&binary_path)
        .await
        .map_err(|e| ModuleError::op_err("Failed to create encrypted binary file", e))?;
    file.write_all(encrypted_binary)
        .await
        .map_err(|e| ModuleError::op_err("Failed to write encrypted binary", e))?;
    file.flush()
        .await
        .map_err(|e| ModuleError::op_err("Failed to flush encrypted binary", e))?;
    // Ensure file handle is closed before returning the path for callers that read it immediately.
    drop(file);

    // Write metadata
    let metadata_path = module_dir.join("metadata.json");
    let metadata_json = serde_json::to_string_pretty(metadata)
        .map_err(|e| ModuleError::op_err("Failed to serialize metadata", e))?;
    fs::write(&metadata_path, metadata_json)
        .map_err(|e| ModuleError::op_err("Failed to write metadata", e))?;

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
        .map_err(|e| ModuleError::op_err("Failed to read metadata", e))?;
    let metadata: EncryptedModuleMetadata = serde_json::from_str(&metadata_json)
        .map_err(|e| ModuleError::op_err("Failed to parse metadata", e))?;

    // Load encrypted binary
    let binary_path = module_dir.join("encrypted_binary");
    let encrypted_binary = fs::read(&binary_path)
        .map_err(|e| ModuleError::op_err("Failed to read encrypted binary", e))?;

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
    fs::create_dir_all(&decrypted_dir)
        .map_err(|e| ModuleError::op_err("Failed to create decrypted modules directory", e))?;

    // Create module directory
    let module_dir = decrypted_dir.join(module_name);
    fs::create_dir_all(&module_dir)
        .map_err(|e| ModuleError::op_err("Failed to create module directory", e))?;

    // Write manifest
    let manifest_path = module_dir.join("module.toml");
    fs::write(&manifest_path, manifest)
        .map_err(|e| ModuleError::op_err("Failed to write manifest", e))?;

    // Write decrypted binary
    let binary_path = module_dir.join(module_name);
    let mut file = tokio::fs::File::create(&binary_path)
        .await
        .map_err(|e| ModuleError::op_err("Failed to create decrypted binary file", e))?;
    file.write_all(decrypted_binary)
        .await
        .map_err(|e| ModuleError::op_err("Failed to write decrypted binary", e))?;
    // tokio::fs::File may still have the write in flight; flush before returning.
    file.flush()
        .await
        .map_err(|e| ModuleError::op_err("Failed to flush decrypted binary", e))?;

    // Make binary executable (Unix)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = file
            .metadata()
            .await
            .map_err(|e| ModuleError::op_err("Failed to get file metadata", e))?
            .permissions();
        perms.set_mode(0o700);
        tokio::fs::set_permissions(&binary_path, perms)
            .await
            .map_err(|e| ModuleError::op_err("Failed to set file permissions", e))?;
    }

    Ok(binary_path)
}
