//! Decentralized module registry client
//!
//! Fetches modules from multiple sources (mirrors) with cryptographic verification.
//! Supports content-addressable storage, local caching, and multi-source fetching.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::debug;

use crate::module::registry::cache::{CachedModule, LocalCache};
use crate::module::registry::cas::{ContentAddressableStorage, ModuleHash};
use crate::module::registry::manifest::ModuleManifest;
use crate::module::security::signing::ModuleSigner;
use crate::module::traits::ModuleError;

/// Registry peer configuration (P2P-based)
#[derive(Debug, Clone)]
pub struct RegistryMirror {
    /// Peer address (TCP/QUIC/Iroh transport address)
    pub peer_addr: crate::network::transport::TransportAddr,
    /// Peer's public key for verification (optional)
    pub public_key: Option<String>,
    /// Last successful verification timestamp
    pub last_verified: Option<u64>,
    /// Reputation score (0.0-1.0)
    pub reputation: f64,
}

/// Module entry from registry
#[derive(Debug, Clone)]
pub struct ModuleEntry {
    /// Module hash (content-addressable identifier)
    pub hash: ModuleHash,
    /// Module name
    pub name: String,
    /// Module version
    pub version: String,
    /// Manifest hash
    pub manifest_hash: ModuleHash,
    /// Binary hash
    pub binary_hash: ModuleHash,
    /// Manifest content
    pub manifest: ModuleManifest,
    /// Binary content (optional - may be fetched separately)
    pub binary: Option<Vec<u8>>,
}

/// Decentralized module registry client
pub struct ModuleRegistry {
    mirrors: Vec<RegistryMirror>,
    local_cache: Arc<RwLock<LocalCache>>,
    cas: Arc<RwLock<ContentAddressableStorage>>,
    signer: ModuleSigner,
    cache_dir: PathBuf,
    /// Optional network manager for P2P fetching
    network_manager: Option<Arc<crate::network::NetworkManager>>,
}

// Expose CAS and local_cache for external access
impl ModuleRegistry {
    /// Get reference to CAS (for module registry extensions)
    pub fn cas(&self) -> Arc<RwLock<ContentAddressableStorage>> {
        Arc::clone(&self.cas)
    }

    /// Get reference to local cache (for module registry extensions)
    pub fn local_cache(&self) -> Arc<RwLock<LocalCache>> {
        Arc::clone(&self.local_cache)
    }
}

impl ModuleRegistry {
    /// Create a new module registry client
    pub fn new<P: AsRef<Path>>(
        cache_dir: P,
        cas_dir: P,
        mirrors: Vec<RegistryMirror>,
    ) -> Result<Self, ModuleError> {
        let cache_dir = cache_dir.as_ref().to_path_buf();
        let cas_dir = cas_dir.as_ref().to_path_buf();

        // Load or create local cache
        let local_cache = Arc::new(RwLock::new(
            LocalCache::load(&cache_dir).unwrap_or_else(|_| LocalCache::new()),
        ));

        // Create CAS
        let cas = Arc::new(RwLock::new(ContentAddressableStorage::new(cas_dir)?));

        Ok(Self {
            mirrors,
            local_cache,
            cas,
            signer: ModuleSigner::new(),
            cache_dir,
            network_manager: None,
        })
    }

    /// Set network manager for P2P fetching
    pub fn with_network_manager(
        mut self,
        network_manager: Arc<crate::network::NetworkManager>,
    ) -> Self {
        self.network_manager = Some(network_manager);
        self
    }

    /// Set network manager for P2P fetching (mutable reference version)
    pub fn set_network_manager(&mut self, network_manager: Arc<crate::network::NetworkManager>) {
        self.network_manager = Some(network_manager);
    }

    /// Fetch module from registry
    pub async fn fetch_module(&self, name: &str) -> Result<ModuleEntry, ModuleError> {
        // 1. Check local cache first
        {
            let cache = self.local_cache.read().await;
            if let Some(cached) = cache.get(name) {
                if cache.is_valid(name) {
                    debug!("Module {} found in cache", name);
                    // Try to load from cache
                    if let Ok(entry) = self.load_from_cache(cached).await {
                        return Ok(entry);
                    }
                }
            }
        }

        // 2. Fetch from mirrors (if any configured)
        if !self.mirrors.is_empty() {
            if let Ok(entry) = self.fetch_from_mirrors(name).await {
                // Cache the verified entry
                self.cache_entry(&entry).await?;
                return Ok(entry);
            }
        }

        // 3. Not found
        Err(ModuleError::ModuleNotFound(format!(
            "Module {name} not found in cache or mirrors"
        )))
    }

    /// Fetch from multiple peers in parallel (via P2P)
    async fn fetch_from_mirrors(&self, name: &str) -> Result<ModuleEntry, ModuleError> {
        // Try each peer sequentially (can be parallelized later with Arc<Self>)
        for mirror in &self.mirrors {
            match self.fetch_from_peer(&mirror.peer_addr, name).await {
                Ok(entry) => {
                    // Verify entry
                    if self.verify_entry(&entry).await.is_ok() {
                        return Ok(entry);
                    }
                }
                Err(_) => {
                    // Try next peer
                    continue;
                }
            }
        }

        Err(ModuleError::ModuleNotFound(format!(
            "Module {name} not found in any peer"
        )))
    }

    /// Fetch module data from a single peer (via P2P protocol)
    async fn fetch_from_peer(
        &self,
        peer_addr: &crate::network::transport::TransportAddr,
        name: &str,
    ) -> Result<ModuleEntry, ModuleError> {
        let network_manager = self.network_manager.as_ref().ok_or_else(|| {
            ModuleError::OperationError("Network manager not set for P2P fetching".to_string())
        })?;

        // Convert TransportAddr to SocketAddr for network manager
        let socket_addr = match peer_addr {
            crate::network::transport::TransportAddr::Tcp(addr) => *addr,
            #[cfg(feature = "quinn")]
            crate::network::transport::TransportAddr::Quinn(addr) => *addr,
            #[cfg(feature = "iroh")]
            crate::network::transport::TransportAddr::Iroh(_) => {
                return Err(ModuleError::OperationError(
                    "Iroh transport not yet supported for module fetching".to_string(),
                ));
            }
        };

        // Register pending request
        let (request_id, response_rx) = network_manager.register_request(socket_addr);

        // Create GetModule message
        use crate::network::protocol::{GetModuleMessage, ProtocolMessage, ProtocolParser};
        let get_module_msg = GetModuleMessage {
            request_id,
            name: name.to_string(),
            version: None,    // Get latest version
            payment_id: None, // Payment ID would be provided if payment was already made
        };

        // Serialize and send message
        let message_wire =
            ProtocolParser::serialize_message(&ProtocolMessage::GetModule(get_module_msg))
                .map_err(|e| {
                    ModuleError::OperationError(format!("Failed to serialize GetModule: {e}"))
                })?;

        network_manager
            .send_to_peer(socket_addr, message_wire)
            .await
            .map_err(|e| {
                ModuleError::OperationError(format!("Failed to send GetModule to peer: {e}"))
            })?;

        // Wait for response with timeout
        let response_data = tokio::time::timeout(tokio::time::Duration::from_secs(30), response_rx)
            .await
            .map_err(|_| ModuleError::Timeout)?
            .map_err(|_| ModuleError::OperationError("Response channel closed".to_string()))?;

        // Parse Module response
        let parsed = ProtocolParser::parse_message(&response_data).map_err(|e| {
            ModuleError::OperationError(format!("Failed to parse Module response: {e}"))
        })?;

        let module_msg = match parsed {
            ProtocolMessage::Module(msg) => msg,
            _ => {
                return Err(ModuleError::OperationError(
                    "Expected Module message".to_string(),
                ))
            }
        };

        // Verify request_id matches
        if module_msg.request_id != request_id {
            return Err(ModuleError::OperationError(
                "Request ID mismatch".to_string(),
            ));
        }

        // Parse manifest
        let manifest_str = String::from_utf8(module_msg.manifest.clone())
            .map_err(|e| ModuleError::InvalidManifest(format!("Failed to decode manifest: {e}")))?;
        let manifest: ModuleManifest = toml::from_str(&manifest_str)
            .map_err(|e| ModuleError::InvalidManifest(format!("Failed to parse manifest: {e}")))?;

        // Create ModuleEntry
        Ok(ModuleEntry {
            hash: module_msg.hash,
            name: module_msg.name,
            version: module_msg.version,
            manifest_hash: module_msg.manifest_hash,
            binary_hash: module_msg.binary_hash,
            manifest,
            binary: module_msg.binary,
        })
    }

    /// Verify and create module entry from fetched data
    async fn verify_and_create_entry(
        &self,
        data: HashMap<String, serde_json::Value>,
    ) -> Result<ModuleEntry, ModuleError> {
        // Parse required fields from fetched data
        let name = data
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModuleError::OperationError("Missing 'name' field".to_string()))?
            .to_string();

        let version = data
            .get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModuleError::OperationError("Missing 'version' field".to_string()))?
            .to_string();

        // Parse hashes (hex-encoded)
        let hash_str = data
            .get("hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModuleError::OperationError("Missing 'hash' field".to_string()))?;

        let manifest_hash_str = data
            .get("manifest_hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModuleError::OperationError("Missing 'manifest_hash' field".to_string()))?;

        let binary_hash_str = data
            .get("binary_hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModuleError::OperationError("Missing 'binary_hash' field".to_string()))?;

        // Decode hashes from hex
        let hash = hex::decode(hash_str)
            .map_err(|e| ModuleError::OperationError(format!("Invalid hash hex: {}", e)))?
            .try_into()
            .map_err(|_| ModuleError::OperationError("Hash must be 32 bytes".to_string()))?;

        let manifest_hash = hex::decode(manifest_hash_str)
            .map_err(|e| ModuleError::OperationError(format!("Invalid manifest_hash hex: {}", e)))?
            .try_into()
            .map_err(|_| ModuleError::OperationError("Manifest hash must be 32 bytes".to_string()))?;

        let binary_hash = hex::decode(binary_hash_str)
            .map_err(|e| ModuleError::OperationError(format!("Invalid binary_hash hex: {}", e)))?
            .try_into()
            .map_err(|_| ModuleError::OperationError("Binary hash must be 32 bytes".to_string()))?;

        // Parse manifest (TOML string or JSON object)
        let manifest = if let Some(manifest_str) = data.get("manifest").and_then(|v| v.as_str()) {
            toml::from_str::<ModuleManifest>(manifest_str)
                .map_err(|e| ModuleError::OperationError(format!("Invalid manifest TOML: {}", e)))?
        } else if let Some(manifest_obj) = data.get("manifest") {
            // Try parsing as JSON and converting to TOML format
            serde_json::from_value::<ModuleManifest>(manifest_obj.clone())
                .map_err(|e| ModuleError::OperationError(format!("Invalid manifest JSON: {}", e)))?
        } else {
            return Err(ModuleError::OperationError("Missing 'manifest' field".to_string()));
        };

        // Verify manifest hash matches
        let cas = self.cas.read().await;
        let manifest_bytes = toml::to_string(&manifest)
            .map_err(|e| ModuleError::OperationError(format!("Failed to serialize manifest: {}", e)))?
            .into_bytes();
        if !cas.verify(&manifest_bytes, &manifest_hash) {
            return Err(ModuleError::CryptoError(
                "Manifest hash verification failed".to_string(),
            ));
        }

        // Parse binary if provided
        let binary = data
            .get("binary")
            .and_then(|v| {
                v.as_str()
                    .and_then(|s| hex::decode(s).ok())
                    .or_else(|| v.as_array().map(|arr| {
                        arr.iter()
                            .filter_map(|item| item.as_u64().map(|n| n as u8))
                            .collect()
                    }))
            })
            .and_then(|bin_data| {
                // Verify binary hash if binary is provided
                if cas.verify(&bin_data, &binary_hash) {
                    Some(bin_data)
                } else {
                    None
                }
            });

        Ok(ModuleEntry {
            hash,
            name,
            version,
            manifest_hash,
            binary_hash,
            manifest,
            binary,
        })
    }

    /// Load module entry from cache
    async fn load_from_cache(&self, cached: &CachedModule) -> Result<ModuleEntry, ModuleError> {
        // Load manifest from CAS
        let manifest_data = self.cas.read().await.get(&cached.manifest_hash)?;

        // Verify manifest hash first
        let cas = self.cas.read().await;
        if !cas.verify(&manifest_data, &cached.manifest_hash) {
            return Err(ModuleError::CryptoError(
                "Cached manifest hash mismatch".to_string(),
            ));
        }
        drop(cas);

        // Parse manifest
        let manifest_str = String::from_utf8(manifest_data).map_err(|e| {
            ModuleError::InvalidManifest(format!("Failed to decode cached manifest: {e}"))
        })?;
        let manifest: ModuleManifest = toml::from_str(&manifest_str).map_err(|e| {
            ModuleError::InvalidManifest(format!("Failed to parse cached manifest: {e}"))
        })?;

        // Load binary if path exists
        let binary = if cached.local_path.exists() {
            Some(std::fs::read(&cached.local_path).map_err(|e| {
                ModuleError::OperationError(format!("Failed to read cached binary: {e}"))
            })?)
        } else {
            None
        };

        // Verify binary hash if present
        if let Some(ref bin_data) = binary {
            if !self.cas.read().await.verify(bin_data, &cached.binary_hash) {
                return Err(ModuleError::CryptoError(
                    "Cached binary hash mismatch".to_string(),
                ));
            }
        }

        Ok(ModuleEntry {
            hash: cached.hash,
            name: cached.name.clone(),
            version: cached.version.clone(),
            manifest_hash: cached.manifest_hash,
            binary_hash: cached.binary_hash,
            manifest,
            binary,
        })
    }

    /// Cache a verified module entry
    async fn cache_entry(&self, entry: &ModuleEntry) -> Result<(), ModuleError> {
        // Store manifest in CAS if not already present
        let manifest_data = toml::to_string(&entry.manifest).map_err(|e| {
            ModuleError::OperationError(format!("Failed to serialize manifest: {e}"))
        })?;

        let mut cas = self.cas.write().await;
        if !cas.has(&entry.manifest_hash) {
            cas.store(manifest_data.as_bytes())?;
        }

        // Store binary in CAS if present
        if let Some(ref binary_data) = entry.binary {
            if !cas.has(&entry.binary_hash) {
                cas.store(binary_data)?;
            }
        }
        drop(cas);

        // Update cache
        let mut cache = self.local_cache.write().await;
        let cached = CachedModule {
            name: entry.name.clone(),
            version: entry.version.clone(),
            hash: entry.hash,
            manifest_hash: entry.manifest_hash,
            binary_hash: entry.binary_hash,
            verified_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            verified_by: vec![],
            local_path: self.cache_dir.join(format!("{}.bin", entry.name)),
            expires_at: None, // No expiration by default
        };

        cache.cache(cached);
        cache.update_sync_time();

        // Save cache to disk
        cache.save(&self.cache_dir)?;

        Ok(())
    }

    /// Verify module entry (signatures, hashes, etc.)
    pub async fn verify_entry(&self, entry: &ModuleEntry) -> Result<(), ModuleError> {
        // 1. Verify manifest signatures if present
        if entry.manifest.has_signatures() {
            let manifest_data = toml::to_string(&entry.manifest).map_err(|e| {
                ModuleError::OperationError(format!("Failed to serialize manifest: {e}"))
            })?;

            let signatures = entry.manifest.get_signatures();
            let public_keys = entry.manifest.get_public_keys();
            let threshold = entry.manifest.get_threshold().ok_or_else(|| {
                ModuleError::CryptoError("Signature threshold not specified".to_string())
            })?;

            let valid = self.signer.verify_manifest(
                manifest_data.as_bytes(),
                &signatures,
                &public_keys,
                threshold,
            )?;

            if !valid {
                return Err(ModuleError::CryptoError(format!(
                    "Manifest signature verification failed for module {}",
                    entry.name
                )));
            }
        }

        // 2. Verify manifest hash
        let manifest_data = toml::to_string(&entry.manifest).map_err(|e| {
            ModuleError::OperationError(format!("Failed to serialize manifest: {e}"))
        })?;

        let cas = self.cas.read().await;
        if !cas.verify(manifest_data.as_bytes(), &entry.manifest_hash) {
            return Err(ModuleError::CryptoError(
                "Manifest hash mismatch".to_string(),
            ));
        }

        // 3. Verify binary hash if present
        if let Some(ref binary_data) = entry.binary {
            if !cas.verify(binary_data, &entry.binary_hash) {
                return Err(ModuleError::CryptoError("Binary hash mismatch".to_string()));
            }
        }

        Ok(())
    }
}
