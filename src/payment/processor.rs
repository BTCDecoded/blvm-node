//! Unified Payment Processor
//!
//! Transport-agnostic core payment processing logic that works for both HTTP and P2P.
//! Reuses existing BIP70 protocol implementation.

use crate::config::PaymentConfig;
use crate::module::registry::client::ModuleRegistry;
use bllvm_protocol::payment::{
    Payment, PaymentACK, PaymentOutput, PaymentProtocolServer, PaymentRequest,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

/// Convert Bitcoin address to script pubkey
///
/// Handles SegWit (P2WPKH, P2WSH) and Taproot (P2TR) addresses.
fn address_to_script_pubkey(
    address: &bllvm_protocol::address::BitcoinAddress,
) -> Result<Vec<u8>, PaymentError> {
    match (address.witness_version, address.witness_program.len()) {
        // SegWit v0: P2WPKH (20 bytes) or P2WSH (32 bytes)
        (0, 20) => {
            // P2WPKH: OP_0 <20-byte-hash>
            let mut script = vec![0x00]; // OP_0
            script.extend_from_slice(&address.witness_program);
            Ok(script)
        }
        (0, 32) => {
            // P2WSH: OP_0 <32-byte-hash>
            let mut script = vec![0x00]; // OP_0
            script.extend_from_slice(&address.witness_program);
            Ok(script)
        }
        // Taproot v1: P2TR (32 bytes)
        (1, 32) => {
            // P2TR: OP_1 <32-byte-hash>
            let mut script = vec![0x51]; // OP_1
            script.extend_from_slice(&address.witness_program);
            Ok(script)
        }
        _ => Err(PaymentError::ProcessingError(format!(
            "Unsupported address type: witness_version={}, program_len={}",
            address.witness_version,
            address.witness_program.len()
        ))),
    }
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

        Ok(Self {
            payment_store: Arc::new(Mutex::new(HashMap::new())),
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

    /// Get module registry (for internal use)
    #[allow(dead_code)]
    fn get_module_registry(&self) -> Option<&Arc<ModuleRegistry>> {
        self.module_registry.as_ref()
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
        merchant_key: Option<&secp256k1::SecretKey>,
    ) -> Result<PaymentRequest, PaymentError> {
        use std::time::{SystemTime, UNIX_EPOCH};

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Determine network from config (defaults to mainnet if not specified)
        let network_str = match self.config.network.as_deref() {
            Some("testnet") | Some("testnet3") => "testnet",
            Some("regtest") => "regtest",
            Some("signet") => "signet",
            _ => "mainnet", // Default to mainnet
        };

        // Create payment request using existing BIP70 implementation
        let mut payment_request = PaymentRequest::new(
            network_str.to_string(),
            outputs,
            timestamp,
        );

        // Set merchant data if provided
        if let Some(data) = merchant_data {
            payment_request.payment_details.merchant_data = Some(data);
        }

        // Sign if merchant key provided
        if let Some(key) = merchant_key {
            payment_request.sign(key).map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to sign payment request: {}", e))
            })?;
        }

        // Generate payment ID and store
        let payment_id = Self::generate_payment_id(&payment_request);
        self.payment_store
            .lock()
            .unwrap()
            .insert(payment_id.clone(), payment_request.clone());

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
        merchant_key: Option<&secp256k1::SecretKey>,
    ) -> Result<PaymentACK, PaymentError> {
        // Look up original request
        let request = self.get_payment_request(&payment_id).await?;

        // Reuse existing process_payment from BIP70
        let ack = PaymentProtocolServer::process_payment(&payment, &request, merchant_key)
            .map_err(|e| PaymentError::ValidationFailed(format!("{:?}", e)))?;

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
            .map_err(|e| PaymentError::ProcessingError(format!("Invalid module hash: {}", e)))?
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
            .map_err(|e| PaymentError::ProcessingError(format!("Failed to fetch module: {}", e)))?;

        let binary = entry.binary.ok_or_else(|| {
            PaymentError::ProcessingError("Module binary not available".to_string())
        })?;

        // Encrypt module
        let (encrypted_binary, nonce) = encryption
            .encrypt_module(&binary, payment_id, &module_hash)
            .map_err(|e| PaymentError::ProcessingError(format!("Encryption failed: {}", e)))?;

        // Create metadata
        use crate::module::encryption::EncryptedModuleMetadata;
        use std::time::{SystemTime, UNIX_EPOCH};
        let encrypted_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

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
            PaymentError::ProcessingError(format!("Failed to store encrypted module: {}", e))
        })?;

        info!(
            "Module {} encrypted at rest (payment_id: {}, method: {})",
            module_name, payment_id, payment_method
        );

        Ok(())
    }

    /// Create module payment request (75/15/10 split)
    ///
    /// # Security
    ///
    /// This method verifies that the author and commons addresses are cryptographically
    /// signed in the module manifest. The node operator's address (10%) is provided by
    /// the node and is not signed (node can choose their own address).
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
        merchant_key: Option<&secp256k1::SecretKey>,
    ) -> Result<PaymentRequest, PaymentError> {
        use crate::module::security::signing::ModuleSigner;
        use bllvm_protocol::address::BitcoinAddress;

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

        // BIP47 payment code address derivation
        // Derives a unique payment address from a payment code for privacy
        fn derive_bip47_address(
            payment_code: &str,
            notification_index: u32,
        ) -> Result<String, PaymentError> {
            // BIP47 implementation requires:
            // 1. Decode payment code (base58)
            // 2. Extract public key from payment code
            // 3. Derive notification address using ECDH
            // 4. Derive payment address from notification address + index
            //
            // For now, return error indicating BIP47 requires full cryptographic implementation
            // This is a placeholder that can be extended with proper BIP47 library integration
            Err(PaymentError::ProcessingError(format!(
                "BIP47 payment code derivation requires full cryptographic implementation. \
                 Payment code: {}, index: {}. \
                 Please provide a legacy address as fallback or implement BIP47 library integration.",
                payment_code, notification_index
            )))
        }

        // Prefer payment codes (BIP47) over fixed addresses for privacy
        // Payment codes generate unique addresses per payment, avoiding address reuse
        let author_address_str = if let Some(ref payment_code) = payment.author_payment_code {
            // Attempt BIP47 derivation (notification index 0 for first payment)
            match derive_bip47_address(payment_code, 0) {
                Ok(addr) => {
                    info!("Derived BIP47 address for author payment: {}", addr);
                    addr
                }
                Err(_) => {
                    // Fall back to legacy address if BIP47 not fully implemented
                    warn!("BIP47 derivation failed, falling back to legacy address");
                    payment.author_address.as_ref().ok_or_else(|| {
                        PaymentError::ProcessingError(
                            "BIP47 payment code provided but derivation failed, and legacy address not provided".to_string(),
                        )
                    })?
                }
            }
        } else if let Some(ref addr) = payment.author_address {
            addr
        } else {
            return Err(PaymentError::ProcessingError(
                "Module author payment address or payment code not specified in manifest"
                    .to_string(),
            ));
        };

        let commons_address_str = if let Some(ref payment_code) = payment.commons_payment_code {
            // Attempt BIP47 derivation (notification index 0 for first payment)
            match derive_bip47_address(payment_code, 0) {
                Ok(addr) => {
                    info!("Derived BIP47 address for commons payment: {}", addr);
                    addr
                }
                Err(_) => {
                    // Fall back to legacy address if BIP47 not fully implemented
                    warn!("BIP47 derivation failed, falling back to legacy address");
                    payment.commons_address.as_ref().ok_or_else(|| {
                        PaymentError::ProcessingError(
                            "BIP47 payment code provided but derivation failed, and legacy address not provided".to_string(),
                        )
                    })?
                }
            }
        } else if let Some(ref addr) = payment.commons_address {
            addr
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
                    author_address_str,
                    commons_address_str,
                    price_sats,
                    payment_sig,
                    &public_keys,
                    threshold,
                )
                .map_err(|e| {
                    PaymentError::ValidationFailed(format!(
                        "Payment address signature verification failed: {}",
                        e
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

        // Decode addresses to script pubkeys
        let author_address = BitcoinAddress::decode(author_address_str).map_err(|e| {
            PaymentError::ProcessingError(format!("Invalid author address format: {}", e))
        })?;

        let commons_address = BitcoinAddress::decode(commons_address_str).map_err(|e| {
            PaymentError::ProcessingError(format!("Invalid commons address format: {}", e))
        })?;

        // Convert addresses to script pubkeys
        // For SegWit (v0) and Taproot (v1), we need to create the appropriate script
        let author_script = address_to_script_pubkey(&author_address)?;
        let commons_script = address_to_script_pubkey(&commons_address)?;
        // Calculate split: 75% author, 15% commons, 10% node
        let author_amount = (price_sats * 75) / 100;
        let commons_amount = (price_sats * 15) / 100;
        let node_amount = (price_sats * 10) / 100;

        // Verify total (should equal price_sats, accounting for rounding)
        let total = author_amount + commons_amount + node_amount;
        if total > price_sats {
            warn!(
                "Payment split exceeds price: {} > {} (rounding error)",
                total, price_sats
            );
        }

        // Create outputs
        let outputs = vec![
            PaymentOutput {
                script: author_script,
                amount: Some(author_amount),
            },
            PaymentOutput {
                script: commons_script,
                amount: Some(commons_amount),
            },
            PaymentOutput {
                script: node_script,
                amount: Some(node_amount),
            },
        ];

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
                    "commons": commons_amount,
                    "node": node_amount,
                },
                "author_address": author_address_str,
                "commons_address": commons_address_str,
            }))
            .map_err(|e| {
                PaymentError::ProcessingError(format!("Failed to serialize merchant_data: {}", e))
            })?,
        );

        self.create_payment_request(outputs, merchant_data, merchant_key)
            .await
    }
}
