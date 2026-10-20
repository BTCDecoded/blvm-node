//! Unified Payment Processor
//!
//! Transport-agnostic core payment processing logic that works for both HTTP and P2P.
//! Reuses existing BIP70 protocol implementation.

use crate::config::PaymentConfig;
use crate::module::registry::client::ModuleRegistry;
use blvm_protocol::payment::{
    Payment, PaymentACK, PaymentOutput, PaymentProtocolServer, PaymentRequest,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

/// Convert a Bitcoin address string to a script pubkey.
///
/// Uses the RPC writer so v0 and v1 programs include the push-length byte.
fn address_to_script_pubkey(address: &str) -> Result<Vec<u8>, PaymentError> {
    crate::rpc::rawtx::address_string_to_script_pubkey(address)
        .map_err(|e| PaymentError::ProcessingError(e.to_string()))
}

/// Payment error types
#[derive(Debug, thiserror::Error)]
pub enum PaymentError {
    #[error("Payment request not found: {0}")]
    RequestNotFound(String),

    #[error("Payment validation failed: {0}")]
    ValidationFailed(String),

    #[error("Feature not enabled: {0}")]
    FeatureNotEnabled(String),

    #[error("No transport enabled")]
    NoTransportEnabled,

    #[error("Payment processing error: {0}")]
    ProcessingError(String),

    #[error("Covenant verification failed: {0}")]
    CovenantVerificationFailed(String),
}

/// Unified payment processor (transport-agnostic)
pub struct PaymentProcessor {
    /// Payment request store (payment_id -> PaymentRequest)
    payment_store: Arc<Mutex<HashMap<String, PaymentRequest>>>,
    /// Module registry (optional, for module payments)
    module_registry: Option<Arc<ModuleRegistry>>,
    /// Module encryption (optional, for module payment encryption)
    module_encryption: Option<Arc<crate::module::encryption::ModuleEncryption>>,
    /// Modules directory path (for storing encrypted/decrypted modules)
    modules_dir: Option<std::path::PathBuf>,
    /// Payment configuration
    config: PaymentConfig,
}

fn load_payment_store(path: &str) -> HashMap<String, PaymentRequest> {
    let mut store = HashMap::new();
    let dir = std::path::Path::new(path);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return store;
    };
    for entry in entries.flatten() {
        let file_path = entry.path();
        if file_path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&file_path) else {
            continue;
        };
        let Ok(request) = serde_json::from_slice::<PaymentRequest>(&bytes) else {
            continue;
        };
        let Some(id) = file_path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if !id.is_empty() {
            store.insert(id.to_string(), request);
        }
    }
    store
}

fn compressed_point(point: &blvm_secp256k1::group::Ge) -> Option<[u8; 33]> {
    if point.infinity {
        return None;
    }
    let mut affine = *point;
    affine.x.normalize();
    affine.y.normalize();
    let mut x = [0u8; 32];
    let mut y = [0u8; 32];
    affine.x.get_b32(&mut x);
    affine.y.get_b32(&mut y);
    let mut out = [0u8; 33];
    out[0] = 0x02 | (y[31] & 1);
    out[1..].copy_from_slice(&x);
    Some(out)
}

fn hmac_sha512(key: &[u8], message: &[u8]) -> [u8; 64] {
    use sha2::{Digest, Sha512};
    let mut key_block = [0u8; 128];
    if key.len() > 128 {
        key_block[..64].copy_from_slice(&Sha512::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; 128];
    let mut outer_pad = [0x5cu8; 128];
    for i in 0..128 {
        inner_pad[i] ^= key_block[i];
        outer_pad[i] ^= key_block[i];
    }
    let mut inner = Sha512::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha512::new();
    outer.update(outer_pad);
    outer.update(inner);
    outer.finalize().into()
}

fn persist_payment_request(dir: &str, payment_id: &str, request: &PaymentRequest) {
    let directory = std::path::Path::new(dir);
    if let Err(e) = std::fs::create_dir_all(directory) {
        warn!("Failed to create payment store {}: {}", dir, e);
        return;
    }
    match serde_json::to_vec(request) {
        Ok(bytes) => {
            let path = directory.join(format!("{payment_id}.json"));
            if let Err(e) = std::fs::write(path, bytes) {
                warn!("Failed to write payment {}: {}", payment_id, e);
            }
        }
        Err(e) => warn!("Failed to encode payment {}: {}", payment_id, e),
    }
}

impl PaymentProcessor {
    /// Create a new payment processor
    pub fn new(config: PaymentConfig) -> Result<Self, PaymentError> {
        // Validate HTTP BIP70 configuration
        #[cfg(not(feature = "bip70-http"))]
        if config.http_enabled {
            return Err(PaymentError::FeatureNotEnabled(
                "HTTP BIP70 requires --features bip70-http".to_string(),
            ));
        }

        // Validate REST API configuration
        #[cfg(not(feature = "rest-api"))]
        if config.http_enabled {
            return Err(PaymentError::FeatureNotEnabled(
                "HTTP BIP70 requires --features rest-api".to_string(),
            ));
        }

        // At least one transport must be enabled
        if !config.p2p_enabled && !config.http_enabled {
            return Err(PaymentError::NoTransportEnabled);
        }

        info!(
            "Payment processor initialized: P2P={}, HTTP={}",
            config.p2p_enabled, config.http_enabled
        );

        let payment_store = load_payment_store(&config.payment_store_path);
        Ok(Self {
            payment_store: Arc::new(Mutex::new(payment_store)),
            module_registry: None,
            module_encryption: None,
            modules_dir: None,
            config,
        })
    }

    /// Set module registry for module payments
    pub fn with_module_registry(mut self, registry: Arc<ModuleRegistry>) -> Self {
        self.module_registry = Some(registry);
        self
    }

    /// Set module encryption for module payment encryption
    pub fn with_module_encryption(
        mut self,
        encryption: Arc<crate::module::encryption::ModuleEncryption>,
    ) -> Self {
        self.module_encryption = Some(encryption);
        self
    }

    /// Set modules directory for storing encrypted/decrypted modules
    pub fn with_modules_dir(mut self, modules_dir: std::path::PathBuf) -> Self {
        self.modules_dir = Some(modules_dir);
        self
    }

    /// Generate payment ID from payment request
    fn generate_payment_id(request: &PaymentRequest) -> String {
        use sha2::{Digest, Sha256};
        let serialized = bincode::serialize(request).unwrap_or_default();
        let hash = Sha256::digest(&serialized);
        hex::encode(&hash[..16]) // Use first 16 bytes for ID
    }

    /// Create payment request (works for both HTTP and P2P)
    pub async fn create_payment_request(
        &self,
        outputs: Vec<PaymentOutput>,
        merchant_data: Option<Vec<u8>>,
        merchant_key: Option<&[u8; 32]>,
    ) -> Result<PaymentRequest, PaymentError> {
        let timestamp = crate::utils::current_timestamp();

        // Determine network from config (defaults to mainnet if not specified)
        let network_str = match self.config.network.as_deref() {
            Some("testnet") | Some("testnet3") => "testnet",
            Some("regtest") => "regtest",
            Some("signet") => "signet",
            _ => "mainnet", // Default to mainnet
        };

        // Create payment request using existing BIP70 implementation
        let mut payment_request = PaymentRequest::new(network_str.to_string(), outputs, timestamp);

        // Set merchant data if provided
        if let Some(data) = merchant_data {
            payment_request.payment_details.merchant_data = Some(data);
        }

        // Sign if merchant key provided
        if let Some(key) = merchant_key {
            payment_request.sign(key).map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to sign payment request: {e}"))
            })?;
        }

        // Generate payment ID and store
        let payment_id = Self::generate_payment_id(&payment_request);
        self.payment_store
            .lock()
            .unwrap()
            .insert(payment_id.clone(), payment_request.clone());
        persist_payment_request(
            &self.config.payment_store_path,
            &payment_id,
            &payment_request,
        );

        debug!("Created payment request: {}", payment_id);

        Ok(payment_request)
    }

    /// Get payment request by ID
    pub async fn get_payment_request(
        &self,
        payment_id: &str,
    ) -> Result<PaymentRequest, PaymentError> {
        let store = self.payment_store.lock().unwrap();
        store
            .get(payment_id)
            .cloned()
            .ok_or_else(|| PaymentError::RequestNotFound(payment_id.to_string()))
    }

    /// Process payment (works for both HTTP and P2P)
    pub async fn process_payment(
        &self,
        payment: Payment,
        payment_id: String,
        merchant_key: Option<&[u8; 32]>,
    ) -> Result<PaymentACK, PaymentError> {
        // Look up original request
        let request = self.get_payment_request(&payment_id).await?;

        // Process payment
        let ack = PaymentProtocolServer::process_payment(&payment, &request, merchant_key)
            .map_err(|e| PaymentError::ValidationFailed(format!("{e:?}")))?;

        info!("Processed payment: {}", payment_id);

        // Check if this is a module payment and encrypt module
        if let Some(merchant_data) = &request.payment_details.merchant_data {
            if let Ok(metadata) = serde_json::from_slice::<serde_json::Value>(merchant_data) {
                if metadata
                    .get("payment_type")
                    .and_then(|v| v.as_str())
                    .map(|s| s == "module_payment")
                    .unwrap_or(false)
                {
                    // This is a module payment - encrypt the module
                    if let Err(e) = self
                        .encrypt_module_for_payment(&payment_id, &metadata)
                        .await
                    {
                        warn!("Failed to encrypt module for payment {}: {}", payment_id, e);
                        // Don't fail the payment if encryption fails - log and continue
                    }
                }
            }
        }

        Ok(ack)
    }

    /// Encrypt module for a payment
    async fn encrypt_module_for_payment(
        &self,
        payment_id: &str,
        metadata: &serde_json::Value,
    ) -> Result<(), PaymentError> {
        // Get required components
        let module_registry = self.module_registry.as_ref().ok_or_else(|| {
            PaymentError::ProcessingError("Module registry not available".to_string())
        })?;

        let encryption = self.module_encryption.as_ref().ok_or_else(|| {
            PaymentError::ProcessingError("Module encryption not available".to_string())
        })?;

        let modules_dir = self.modules_dir.as_ref().ok_or_else(|| {
            PaymentError::ProcessingError("Modules directory not configured".to_string())
        })?;

        // Extract module info from metadata
        let module_name = metadata
            .get("module_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                PaymentError::ProcessingError(
                    "Module name not found in payment metadata".to_string(),
                )
            })?;

        let module_hash_hex = metadata
            .get("module_hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                PaymentError::ProcessingError(
                    "Module hash not found in payment metadata".to_string(),
                )
            })?;

        let module_hash = hex::decode(module_hash_hex)
            .map_err(|e| PaymentError::ProcessingError(format!("Invalid module hash: {e}")))?
            .try_into()
            .map_err(|_| {
                PaymentError::ProcessingError("Module hash must be 32 bytes".to_string())
            })?;

        // Determine payment method from payment
        let payment_method = if metadata
            .get("payment_method")
            .and_then(|v| v.as_str())
            .is_some()
        {
            metadata
                .get("payment_method")
                .and_then(|v| v.as_str())
                .unwrap_or("on-chain")
        } else {
            "on-chain" // Default
        };

        // Fetch module from registry
        let entry = module_registry
            .fetch_module(module_name)
            .await
            .map_err(|e| PaymentError::ProcessingError(format!("Failed to fetch module: {e}")))?;

        let binary = entry.binary.ok_or_else(|| {
            PaymentError::ProcessingError("Module binary not available".to_string())
        })?;

        // Encrypt module
        let (encrypted_binary, nonce) = encryption
            .encrypt_module(&binary, payment_id, &module_hash)
            .map_err(|e| PaymentError::ProcessingError(format!("Encryption failed: {e}")))?;

        // Create metadata
        use crate::module::encryption::EncryptedModuleMetadata;
        let encrypted_at = crate::utils::current_timestamp();

        let encryption_metadata = EncryptedModuleMetadata {
            payment_id: payment_id.to_string(),
            module_hash: module_hash.to_vec(),
            nonce,
            encrypted_at,
            payment_method: payment_method.to_string(),
        };

        // Store encrypted module
        crate::module::encryption::store_encrypted_module(
            modules_dir,
            module_name,
            &encrypted_binary,
            &encryption_metadata,
        )
        .await
        .map_err(|e| {
            PaymentError::ProcessingError(format!("Failed to store encrypted module: {e}"))
        })?;

        info!(
            "Module {} encrypted at rest (payment_id: {}, method: {})",
            module_name, payment_id, payment_method
        );

        Ok(())
    }

    /// BIP-47 payment address at `notification_index`. Index 0 is the first payment address.
    pub(crate) fn derive_bip47_address(
        &self,
        payment_code: &str,
        notification_index: u32,
    ) -> Result<String, PaymentError> {
        let Some(scan_hex) = self.config.bip47_scan_key.as_deref() else {
            return Err(PaymentError::ProcessingError(
                "BIP-47 scan key is not configured".to_string(),
            ));
        };
        let scan_vec = hex::decode(scan_hex.trim())
            .map_err(|_| PaymentError::ProcessingError("invalid BIP-47 scan key".to_string()))?;
        let scan_bytes: [u8; 32] = scan_vec
            .try_into()
            .map_err(|_| PaymentError::ProcessingError("invalid BIP-47 scan key".to_string()))?;
        let payload = crate::rpc::rawtx::base58check_decode(payment_code).ok_or_else(|| {
            PaymentError::ProcessingError("invalid BIP-47 payment code".to_string())
        })?;
        if payload.len() != 81 || payload[0] != 0x47 || payload[1] != 0x01 {
            return Err(PaymentError::ProcessingError(
                "invalid BIP-47 payment code".to_string(),
            ));
        }
        let pubkey_bytes: [u8; 33] = payload[3..36].try_into().expect("33-byte pubkey");
        let chain_code: [u8; 32] = payload[36..68].try_into().expect("32-byte chain code");
        let payment_point =
            blvm_secp256k1::ecdsa::ge_from_compressed(&pubkey_bytes).ok_or_else(|| {
                PaymentError::ProcessingError("invalid BIP-47 payment code".to_string())
            })?;
        let mut scan = blvm_secp256k1::scalar::Scalar::zero();
        if scan.set_b32(&scan_bytes) || scan.is_zero() {
            return Err(PaymentError::ProcessingError(
                "invalid BIP-47 scan key".to_string(),
            ));
        }
        let mut shared_j = blvm_secp256k1::group::Gej::default();
        blvm_secp256k1::ecmult_const(&mut shared_j, &payment_point, &scan);
        let mut shared = blvm_secp256k1::group::Ge::default();
        shared.set_gej_var(&shared_j);
        let shared_bytes = compressed_point(&shared).ok_or_else(|| {
            PaymentError::ProcessingError("invalid BIP-47 payment code".to_string())
        })?;
        let mut mac_message = Vec::with_capacity(37);
        mac_message.extend_from_slice(&shared_bytes);
        mac_message.extend_from_slice(&notification_index.to_be_bytes());
        let mac = hmac_sha512(&chain_code, &mac_message);
        let mut blind_bytes = [0u8; 32];
        blind_bytes.copy_from_slice(&mac[..32]);
        let mut blind = blvm_secp256k1::scalar::Scalar::zero();
        if blind.set_b32(&blind_bytes) || blind.is_zero() {
            return Err(PaymentError::ProcessingError(
                "invalid BIP-47 payment code".to_string(),
            ));
        }
        let mut blind_g = blvm_secp256k1::group::Gej::default();
        blvm_secp256k1::ecmult_gen_const(&mut blind_g, &blind);
        let mut sum = blvm_secp256k1::group::Gej::default();
        sum.add_ge_var(&blind_g, &payment_point);
        let mut payment = blvm_secp256k1::group::Ge::default();
        payment.set_gej_var(&sum);
        let payment_pub = compressed_point(&payment).ok_or_else(|| {
            PaymentError::ProcessingError("invalid BIP-47 payment code".to_string())
        })?;
        let hash = crate::storage::hashing::hash160(&payment_pub);
        let version = match self.config.network.as_deref() {
            Some("testnet") | Some("regtest") | Some("signet") => 0x6f,
            _ => 0x00,
        };
        let mut address_payload = Vec::with_capacity(21);
        address_payload.push(version);
        address_payload.extend_from_slice(&hash);
        Ok(crate::rpc::rawtx::base58check_encode(&address_payload))
    }

    /// Create module payment request (75/15/10 split)
    ///
    /// Payment split:
    /// - 75% to module author
    /// - 15% to marketplace module developer (Commons developers)
    /// - 10% to node operator
    ///
    /// # Security
    ///
    /// This method verifies that the author and marketplace addresses are cryptographically
    /// signed in the module manifest. The node operator's address (10%) is provided by
    /// the node and is not signed (node can choose their own address).
    ///
    /// # Note
    ///
    /// The 15% goes to the marketplace module developer (who developed the marketplace module),
    /// not to "Commons governance" as a separate entity. The marketplace module handles
    /// registry, discovery, and payment processing.
    ///
    /// # Arguments
    ///
    /// * `manifest` - Module manifest containing signed payment addresses
    /// * `module_hash` - Module hash (for encryption key derivation)
    /// * `node_script` - Node operator's payment script (10% of payment)
    /// * `merchant_key` - Optional merchant key for signing payment request
    pub async fn create_module_payment_request(
        &self,
        manifest: &crate::module::registry::manifest::ModuleManifest,
        module_hash: &[u8; 32],
        node_script: Vec<u8>,
        merchant_key: Option<&[u8; 32]>,
    ) -> Result<PaymentRequest, PaymentError> {
        use crate::module::security::signing::ModuleSigner;

        if !self.config.module_payments_enabled {
            return Err(PaymentError::ProcessingError(
                "Module payments are disabled".to_string(),
            ));
        }

        // Extract payment configuration from manifest
        let payment = manifest.payment.as_ref().ok_or_else(|| {
            PaymentError::ProcessingError("Module manifest missing payment section".to_string())
        })?;

        if !payment.required {
            return Err(PaymentError::ProcessingError(
                "Module does not require payment".to_string(),
            ));
        }

        let price_sats = payment.price_sats.ok_or_else(|| {
            PaymentError::ProcessingError("Payment required but price not specified".to_string())
        })?;

        let resolve_payment_code = |code: &str,
                                    legacy: &Option<String>|
         -> Result<String, PaymentError> {
            match self.derive_bip47_address(code, 0) {
                Ok(addr) => Ok(addr),
                Err(PaymentError::ProcessingError(msg)) if msg.starts_with("invalid BIP-47") => {
                    Err(PaymentError::ProcessingError(msg))
                }
                Err(_) => legacy.clone().ok_or_else(|| {
                    PaymentError::ProcessingError(
                        "BIP47 payment code provided but derivation failed, and legacy address not provided"
                            .to_string(),
                    )
                }),
            }
        };

        // Prefer payment codes (BIP47) over fixed addresses for privacy
        // Payment codes generate unique addresses per payment, avoiding address reuse
        let author_address_str = if let Some(ref payment_code) = payment.author_payment_code {
            match resolve_payment_code(payment_code, &payment.author_address) {
                Ok(addr) => {
                    info!("Derived BIP47 address for author payment: {}", addr);
                    addr
                }
                Err(err) => return Err(err),
            }
        } else if let Some(ref addr) = payment.author_address {
            addr.to_string()
        } else {
            return Err(PaymentError::ProcessingError(
                "Module author payment address or payment code not specified in manifest"
                    .to_string(),
            ));
        };

        let commons_address_str = if let Some(ref payment_code) = payment.commons_payment_code {
            match resolve_payment_code(payment_code, &payment.commons_address) {
                Ok(addr) => {
                    info!("Derived BIP47 address for commons payment: {}", addr);
                    addr
                }
                Err(err) => return Err(err),
            }
        } else if let Some(ref addr) = payment.commons_address {
            addr.to_string()
        } else {
            return Err(PaymentError::ProcessingError(
                "Commons governance payment address or payment code not specified in manifest"
                    .to_string(),
            ));
        };

        // Verify payment address signatures (CRITICAL: prevents node tampering)
        if let Some(ref payment_sig) = payment.payment_signature {
            let signer = ModuleSigner::new();
            let public_keys = manifest.get_public_keys();
            let threshold = manifest.get_threshold().ok_or_else(|| {
                PaymentError::ProcessingError(
                    "Manifest signature threshold not specified".to_string(),
                )
            })?;

            let valid = signer
                .verify_payment_addresses(
                    &author_address_str,
                    &commons_address_str,
                    price_sats,
                    payment_sig,
                    &public_keys,
                    threshold,
                )
                .map_err(|e| {
                    PaymentError::ValidationFailed(format!(
                        "Payment address signature verification failed: {e}"
                    ))
                })?;

            if !valid {
                return Err(PaymentError::ValidationFailed(
                    "Payment address signature verification failed: insufficient signatures"
                        .to_string(),
                ));
            }

            debug!(
                "Payment address signatures verified for module {}",
                manifest.name
            );
        } else {
            return Err(PaymentError::ValidationFailed(
                "Payment addresses not cryptographically signed in manifest".to_string(),
            ));
        }

        let author_script = address_to_script_pubkey(&author_address_str)?;
        let marketplace_script = address_to_script_pubkey(&commons_address_str)?;
        // 75% author, 15% marketplace, remainder to the node so the outputs sum to price_sats.
        let author_amount = (price_sats * 75) / 100;
        let marketplace_amount = (price_sats * 15) / 100;
        let node_amount = price_sats
            .saturating_sub(author_amount)
            .saturating_sub(marketplace_amount);

        let mut outputs = Vec::new();
        if author_amount > 0 {
            outputs.push(PaymentOutput {
                script: author_script,
                amount: Some(author_amount),
            });
        }
        if marketplace_amount > 0 {
            outputs.push(PaymentOutput {
                script: marketplace_script,
                amount: Some(marketplace_amount),
            });
        }
        if node_amount > 0 {
            outputs.push(PaymentOutput {
                script: node_script,
                amount: Some(node_amount),
            });
        }
        if outputs.is_empty() {
            return Err(PaymentError::ProcessingError(
                "Payment price produced no outputs".to_string(),
            ));
        }

        // Include module info in merchant_data (for encryption/decryption)
        let merchant_data = Some(
            serde_json::to_vec(&json!({
                "module_name": manifest.name,
                "module_version": manifest.version,
                "module_hash": hex::encode(module_hash),
                "payment_type": "module_payment",
                "price_sats": price_sats,
                "split": {
                    "author": author_amount,
                    "marketplace": marketplace_amount,
                    "node": node_amount,
                },
                "author_address": author_address_str,
                "commons_address": commons_address_str,
            }))
            .map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to serialize merchant_data: {e}"))
            })?,
        );

        self.create_payment_request(outputs, merchant_data, merchant_key)
            .await
    }
}

#[cfg(test)]
mod bip47_address {
    use super::compressed_point;
    use crate::config::PaymentConfig;
    use crate::payment::processor::PaymentProcessor;

    fn scalar_point(secret: [u8; 32]) -> [u8; 33] {
        let mut scalar = blvm_secp256k1::scalar::Scalar::zero();
        assert!(!scalar.set_b32(&secret));
        let mut jacobian = blvm_secp256k1::group::Gej::default();
        blvm_secp256k1::ecmult_gen_const(&mut jacobian, &scalar);
        let mut point = blvm_secp256k1::group::Ge::default();
        point.set_gej_var(&jacobian);
        compressed_point(&point).unwrap()
    }

    fn payment_code(pubkey: &[u8; 33], chain: &[u8; 32]) -> String {
        let mut body = vec![0x47, 0x01, 0x00];
        body.extend_from_slice(pubkey);
        body.extend_from_slice(chain);
        body.extend_from_slice(&[0u8; 13]);
        crate::rpc::rawtx::base58check_encode(&body)
    }

    #[test]
    fn index_zero_address_follows_the_payment_code_and_scan_key() {
        let payment_secret = {
            let mut bytes = [0u8; 32];
            bytes[31] = 2;
            bytes
        };
        let scan_secret = {
            let mut bytes = [0u8; 32];
            bytes[31] = 3;
            bytes
        };
        let chain = [0x11u8; 32];
        let pubkey = scalar_point(payment_secret);
        let code = payment_code(&pubkey, &chain);
        let mut config = PaymentConfig::default();
        config.bip47_scan_key = Some(hex::encode(scan_secret));
        let processor = PaymentProcessor::new(config).unwrap();
        let address = processor.derive_bip47_address(&code, 0).unwrap();
        let again = processor.derive_bip47_address(&code, 0).unwrap();
        assert_eq!(address, again);
        let other_index = processor.derive_bip47_address(&code, 1).unwrap();
        assert_ne!(address, other_index);

        let decoded = crate::rpc::rawtx::base58check_decode(&address).unwrap();
        assert_eq!(decoded.len(), 21);
        assert_eq!(decoded[0], 0x00);
        let hashed_code = crate::storage::hashing::hash160(code.as_bytes());
        assert_ne!(&decoded[1..], hashed_code.as_slice());

        let payment_point = blvm_secp256k1::ecdsa::ge_from_compressed(&pubkey).unwrap();
        let mut scan = blvm_secp256k1::scalar::Scalar::zero();
        assert!(!scan.set_b32(&scan_secret));
        let mut shared_j = blvm_secp256k1::group::Gej::default();
        blvm_secp256k1::ecmult_const(&mut shared_j, &payment_point, &scan);
        let mut shared = blvm_secp256k1::group::Ge::default();
        shared.set_gej_var(&shared_j);
        let shared_bytes = compressed_point(&shared).unwrap();
        let mut message = shared_bytes.to_vec();
        message.extend_from_slice(&0u32.to_be_bytes());
        let mac = super::hmac_sha512(&chain, &message);
        let mut blind_bytes = [0u8; 32];
        blind_bytes.copy_from_slice(&mac[..32]);
        let mut blind = blvm_secp256k1::scalar::Scalar::zero();
        assert!(!blind.set_b32(&blind_bytes));
        let mut blind_g = blvm_secp256k1::group::Gej::default();
        blvm_secp256k1::ecmult_gen_const(&mut blind_g, &blind);
        let mut sum = blvm_secp256k1::group::Gej::default();
        sum.add_ge_var(&blind_g, &payment_point);
        let mut derived = blvm_secp256k1::group::Ge::default();
        derived.set_gej_var(&sum);
        let derived_pub = compressed_point(&derived).unwrap();
        let expect = crate::storage::hashing::hash160(&derived_pub);
        assert_eq!(&decoded[1..], expect.as_slice());

        assert!(processor.derive_bip47_address("not-a-code", 0).is_err());
        let mut bare = PaymentConfig::default();
        bare.bip47_scan_key = None;
        let closed = PaymentProcessor::new(bare).unwrap();
        assert!(closed.derive_bip47_address(&code, 0).is_err());
    }
}
