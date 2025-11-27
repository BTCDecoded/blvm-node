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
    use bllvm_protocol::address::BitcoinAddress;

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
}

/// Unified payment processor (transport-agnostic)
pub struct PaymentProcessor {
    /// Payment request store (payment_id -> PaymentRequest)
    payment_store: Arc<Mutex<HashMap<String, PaymentRequest>>>,
    /// Module registry (optional, for module payments)
    module_registry: Option<Arc<ModuleRegistry>>,
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
            config,
        })
    }

    /// Set module registry for module payments
    pub fn with_module_registry(mut self, registry: Arc<ModuleRegistry>) -> Self {
        self.module_registry = Some(registry);
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
        merchant_key: Option<&secp256k1::SecretKey>,
    ) -> Result<PaymentRequest, PaymentError> {
        use std::time::{SystemTime, UNIX_EPOCH};

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Create payment request using existing BIP70 implementation
        let mut payment_request = PaymentRequest::new(
            "mainnet".to_string(), // TODO: Get from config
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

        Ok(ack)
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
    /// * `node_script` - Node operator's payment script (10% of payment)
    /// * `merchant_key` - Optional merchant key for signing payment request
    pub async fn create_module_payment_request(
        &self,
        manifest: &crate::module::registry::manifest::ModuleManifest,
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

        // Prefer payment codes (BIP47) over fixed addresses for privacy
        // Payment codes generate unique addresses per payment, avoiding address reuse
        let (author_address_str, using_payment_code_author) = if let Some(ref code) =
            payment.author_payment_code
        {
            // TODO: Implement BIP47 address derivation from payment code
            // For now, warn that payment codes are not yet implemented
            warn!("Payment codes (BIP47) not yet implemented, falling back to legacy address");
            // Fall back to legacy address if provided
            (payment.author_address.as_ref().ok_or_else(|| {
                PaymentError::ProcessingError(
                    "Payment code provided but BIP47 not implemented, and legacy address not provided".to_string(),
                )
            })?, false)
        } else if let Some(ref addr) = payment.author_address {
            warn!("Using legacy fixed address for author payment - consider migrating to payment codes (BIP47) for better privacy");
            (addr, false)
        } else {
            return Err(PaymentError::ProcessingError(
                "Module author payment address or payment code not specified in manifest"
                    .to_string(),
            ));
        };

        let (commons_address_str, using_payment_code_commons) = if let Some(ref code) =
            payment.commons_payment_code
        {
            // TODO: Implement BIP47 address derivation from payment code
            warn!("Payment codes (BIP47) not yet implemented, falling back to legacy address");
            (payment.commons_address.as_ref().ok_or_else(|| {
                PaymentError::ProcessingError(
                    "Payment code provided but BIP47 not implemented, and legacy address not provided".to_string(),
                )
            })?, false)
        } else if let Some(ref addr) = payment.commons_address {
            warn!("Using legacy fixed address for commons payment - consider migrating to payment codes (BIP47) for better privacy");
            (addr, false)
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

        // Include module info in merchant_data
        let merchant_data = Some(
            serde_json::to_vec(&json!({
                "module_name": manifest.name,
                "module_version": manifest.version,
                "payment_type": "module_purchase",
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
