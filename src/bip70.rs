//! BIP70: Payment Protocol (P2P Variant)
//!
//! Specification: https://github.com/bitcoin/bips/blob/master/bip-0070.mediawiki
//!
//! This is a P2P-based variant of BIP70 that addresses security concerns:
//! - Uses Bitcoin P2P network instead of HTTP/HTTPS (privacy-preserving)
//! - Uses Bitcoin public key signatures instead of X.509 certificates (decentralized)
//! - Supports signed refund addresses (prevents refund attacks)
//! - Works with TCP, Iroh, and QUIC transports
//!
//! Core messages:
//! - PaymentRequest: Merchant payment details signed with Bitcoin key
//! - Payment: Customer payment transaction(s)
//! - PaymentACK: Merchant confirmation of payment
//!
//! Security enhancements:
//! - Merchant authentication via Bitcoin public keys (on-chain verifiable)
//! - Signed refund addresses prevent refund attacks
//! - P2P routing preserves customer privacy (no direct merchant connection)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use secp256k1::{Secp256k1, Message};
use secp256k1::ecdsa::Signature;
use sha2::{Sha256, Digest};

/// BIP70 Payment Protocol version
pub const PAYMENT_PROTOCOL_VERSION: u32 = 1;

/// Payment Details - Core payment information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentDetails {
    /// Network identifier (mainnet, testnet, regtest)
    pub network: String,
    /// Payment outputs (address, amount)
    pub outputs: Vec<PaymentOutput>,
    /// Payment expiration time (Unix timestamp)
    pub time: u64,
    /// Payment expiration time
    pub expires: Option<u64>,
    /// Memo for merchant
    pub memo: Option<String>,
    /// Memo for customer
    pub payment_url: Option<String>,
    /// Merchant data (opaque to customer)
    pub merchant_data: Option<Vec<u8>>,
}

/// Payment Output - Address and amount
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentOutput {
    /// Bitcoin address or script
    pub script: Vec<u8>,
    /// Amount in satoshis (None = all available)
    pub amount: Option<u64>,
}

/// Signed refund address - Pre-authorized refund address with merchant signature
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedRefundAddress {
    /// Refund address/script
    pub address: PaymentOutput,
    /// Merchant signature over address (prevents refund attacks)
    pub signature: Vec<u8>,
}

/// Payment Request - Main payment protocol message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentRequest {
    /// Payment details
    pub payment_details: PaymentDetails,
    /// Merchant's Bitcoin public key (compressed, 33 bytes)
    /// Replaces X.509 certificates with on-chain verifiable keys
    pub merchant_pubkey: Option<Vec<u8>>,
    /// Signature over payment_details by merchant's Bitcoin key
    pub signature: Option<Vec<u8>>,
    /// Pre-authorized refund addresses (signed by merchant)
    /// Prevents refund address attacks by requiring merchant signature
    pub authorized_refund_addresses: Option<Vec<SignedRefundAddress>>,
}

/// Payment - Customer payment transaction(s)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payment {
    /// Serialized transaction(s)
    pub transactions: Vec<Vec<u8>>,
    /// Refund addresses (if change needed)
    pub refund_to: Option<Vec<PaymentOutput>>,
    /// Merchant data (echo back from PaymentRequest)
    pub merchant_data: Option<Vec<u8>>,
    /// Memo from customer
    pub memo: Option<String>,
}

/// Payment ACK - Merchant confirmation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentACK {
    /// Original payment message
    pub payment: Payment,
    /// Confirmation memo from merchant
    pub memo: Option<String>,
}

impl PaymentRequest {
    /// Create a new payment request
    pub fn new(
        network: String,
        outputs: Vec<PaymentOutput>,
        time: u64,
    ) -> Self {
        Self {
            payment_details: PaymentDetails {
                network,
                outputs,
                time,
                expires: None,
                memo: None,
                payment_url: None,
                merchant_data: None,
            },
            merchant_pubkey: None,
            signature: None,
            authorized_refund_addresses: None,
        }
    }
    
    /// Set merchant public key
    pub fn with_merchant_key(mut self, pubkey: [u8; 33]) -> Self {
        self.merchant_pubkey = Some(pubkey.to_vec());
        self
    }
    
    /// Add authorized refund address (signed by merchant)
    pub fn with_authorized_refund(mut self, signed_refund: SignedRefundAddress) -> Self {
        if self.authorized_refund_addresses.is_none() {
            self.authorized_refund_addresses = Some(Vec::new());
        }
        self.authorized_refund_addresses.as_mut().unwrap().push(signed_refund);
        self
    }

    /// Set expiration time
    pub fn with_expires(mut self, expires: u64) -> Self {
        self.payment_details.expires = Some(expires);
        self
    }

    /// Set memo for merchant
    pub fn with_memo(mut self, memo: String) -> Self {
        self.payment_details.memo = Some(memo);
        self
    }

    /// Set payment URL (where to send Payment message)
    pub fn with_payment_url(mut self, url: String) -> Self {
        self.payment_details.payment_url = Some(url);
        self
    }

    /// Set merchant data (opaque customer data)
    pub fn with_merchant_data(mut self, data: Vec<u8>) -> Self {
        self.payment_details.merchant_data = Some(data);
        self
    }

    /// Sign payment request with merchant's private key
    pub fn sign(&mut self, private_key: &secp256k1::SecretKey) -> Result<(), Bip70Error> {
        use secp256k1::Message as SecpMessage;
        
        // Serialize payment_details for signing
        let serialized = bincode::serialize(&self.payment_details)
            .map_err(|e| Bip70Error::SerializationError(e.to_string()))?;
        
        // Hash payment_details
        let mut hasher = Sha256::new();
        hasher.update(&serialized);
        let hash = hasher.finalize();
        
        // Create message for signing
        let message = SecpMessage::from_digest_slice(&hash)
            .map_err(|e| Bip70Error::SignatureError(format!("Invalid message: {}", e)))?;
        
        // Sign with secp256k1
        let secp = Secp256k1::new();
        let signature = secp.sign_ecdsa(&message, private_key);
        
        // Get public key
        let pubkey = secp256k1::PublicKey::from_secret_key(&secp, private_key);
        let pubkey_serialized = pubkey.serialize();
        
        // Store signature and public key
        self.signature = Some(signature.serialize_compact().to_vec());
        self.merchant_pubkey = Some(pubkey_serialized.to_vec());
        
        Ok(())
    }
    
    /// Verify payment request signature
    pub fn verify_signature(&self) -> Result<(), Bip70Error> {
        let pubkey = self.merchant_pubkey
            .as_ref()
            .ok_or_else(|| Bip70Error::SignatureError("No merchant public key".to_string()))?;
        let signature_bytes = self.signature
            .as_ref()
            .ok_or_else(|| Bip70Error::SignatureError("No signature".to_string()))?;
        
        // Parse public key
        let pubkey = secp256k1::PublicKey::from_slice(&pubkey)
            .map_err(|e| Bip70Error::SignatureError(format!("Invalid public key: {}", e)))?;
        
        // Parse signature
        let signature = Signature::from_compact(signature_bytes)
            .map_err(|e| Bip70Error::SignatureError(format!("Invalid signature: {}", e)))?;
        
        // Serialize payment_details for verification
        let serialized = bincode::serialize(&self.payment_details)
            .map_err(|e| Bip70Error::SerializationError(e.to_string()))?;
        
        // Hash payment_details
        let mut hasher = Sha256::new();
        hasher.update(&serialized);
        let hash = hasher.finalize();
        
        // Create message for verification
        let message = Message::from_digest_slice(&hash)
            .map_err(|e| Bip70Error::SignatureError(format!("Invalid message: {}", e)))?;
        
        // Verify signature
        let secp = Secp256k1::new();
        secp.verify_ecdsa(&message, &signature, &pubkey)
            .map_err(|_| Bip70Error::SignatureError("Signature verification failed".to_string()))?;
        
        Ok(())
    }
    
    /// Validate payment request
    pub fn validate(&self) -> Result<(), Bip70Error> {
        // Check expiration
        if let Some(expires) = self.payment_details.expires {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            if now > expires {
                return Err(Bip70Error::Expired);
            }
        }

        // Validate outputs
        if self.payment_details.outputs.is_empty() {
            return Err(Bip70Error::InvalidRequest("No payment outputs".to_string()));
        }

        // Validate network
        let valid_networks = ["main", "test", "regtest"];
        if !valid_networks.contains(&self.payment_details.network.as_str()) {
            return Err(Bip70Error::InvalidRequest(
                format!("Invalid network: {}", self.payment_details.network)
            ));
        }

        Ok(())
    }
}

impl Payment {
    /// Create a new payment
    pub fn new(transactions: Vec<Vec<u8>>) -> Self {
        Self {
            transactions,
            refund_to: None,
            merchant_data: None,
            memo: None,
        }
    }

    /// Add refund address (must be pre-authorized in PaymentRequest)
    pub fn with_refund_to(mut self, outputs: Vec<PaymentOutput>) -> Self {
        self.refund_to = Some(outputs);
        self
    }
    
    /// Validate refund addresses against PaymentRequest authorized list
    pub fn validate_refund_addresses(
        &self,
        authorized_refunds: &[SignedRefundAddress],
    ) -> Result<(), Bip70Error> {
        if let Some(ref refund_to) = self.refund_to {
            for refund_addr in refund_to {
                // Check if refund address is in authorized list
                let is_authorized = authorized_refunds.iter().any(|auth| {
                    auth.address.script == refund_addr.script &&
                    auth.address.amount == refund_addr.amount
                });
                
                if !is_authorized {
                    return Err(Bip70Error::InvalidPayment(
                        format!("Refund address not authorized: {:?}", refund_addr.script)
                    ));
                }
            }
        }
        Ok(())
    }

    /// Set merchant data (echo from PaymentRequest)
    pub fn with_merchant_data(mut self, data: Vec<u8>) -> Self {
        self.merchant_data = Some(data);
        self
    }

    /// Set customer memo
    pub fn with_memo(mut self, memo: String) -> Self {
        self.memo = Some(memo);
        self
    }

    /// Validate payment
    pub fn validate(&self) -> Result<(), Bip70Error> {
        if self.transactions.is_empty() {
            return Err(Bip70Error::InvalidPayment("No transactions".to_string()));
        }

        Ok(())
    }
}

/// BIP70 Error types
#[derive(Debug, thiserror::Error)]
pub enum Bip70Error {
    #[error("Payment request expired")]
    Expired,
    
    #[error("Invalid payment request: {0}")]
    InvalidRequest(String),
    
    #[error("Invalid payment: {0}")]
    InvalidPayment(String),
    
    #[error("Certificate validation failed: {0}")]
    CertificateError(String),
    
    #[error("Signature verification failed: {0}")]
    SignatureError(String),
    
    #[error("HTTP error: {0}")]
    HttpError(String),
    
    #[error("Serialization error: {0}")]
    SerializationError(String),
}

/// BIP70 Payment Protocol client (for making payments via P2P)
pub struct PaymentProtocolClient;

impl PaymentProtocolClient {
    /// Create GetPaymentRequest message for P2P network
    pub fn create_get_payment_request(
        merchant_pubkey: [u8; 33],
        payment_id: [u8; 32],
        network: String,
    ) -> crate::network::protocol::GetPaymentRequestMessage {
        crate::network::protocol::GetPaymentRequestMessage {
            merchant_pubkey: merchant_pubkey.to_vec(),
            payment_id: payment_id.to_vec(),
            network,
        }
    }
    
    /// Create Payment message for P2P network
    pub fn create_payment_message(
        payment: Payment,
        payment_id: [u8; 32],
    ) -> crate::network::protocol::PaymentMessage {
        crate::network::protocol::PaymentMessage {
            payment,
            payment_id: payment_id.to_vec(),
            customer_signature: None,
        }
    }
    
    /// Validate received PaymentRequest from P2P network
    pub fn validate_payment_request(
        msg: &crate::network::protocol::PaymentRequestMessage,
    ) -> Result<(), Bip70Error> {
        // Verify signature
        msg.payment_request.verify_signature()?;
        
        // Validate payment request
        msg.payment_request.validate()?;
        
        // Verify merchant pubkey matches
        if let Some(ref req_pubkey) = msg.payment_request.merchant_pubkey {
            if req_pubkey.as_slice() != msg.merchant_pubkey.as_slice() {
                return Err(Bip70Error::SignatureError(
                    "PaymentRequest pubkey mismatch".to_string()
                ));
            }
        }
        
        Ok(())
    }
    
    /// Validate PaymentACK from merchant
    pub fn validate_payment_ack(
        ack: &crate::network::protocol::PaymentACKMessage,
        merchant_pubkey: &[u8],
    ) -> Result<(), Bip70Error> {
        // Verify merchant signature
        let pubkey = secp256k1::PublicKey::from_slice(merchant_pubkey)
            .map_err(|e| Bip70Error::SignatureError(format!("Invalid pubkey: {}", e)))?;
        
        // Serialize payment_ack for verification
        let serialized = bincode::serialize(&ack.payment_ack)
            .map_err(|e| Bip70Error::SerializationError(e.to_string()))?;
        
        let mut hasher = Sha256::new();
        hasher.update(&serialized);
        let hash = hasher.finalize();
        
        let message = Message::from_digest_slice(&hash)
            .map_err(|e| Bip70Error::SignatureError(format!("Invalid message: {}", e)))?;
        
        let signature = Signature::from_compact(&ack.merchant_signature)
            .map_err(|e| Bip70Error::SignatureError(format!("Invalid signature: {}", e)))?;
        
        let secp = Secp256k1::new();
        secp.verify_ecdsa(&message, &signature, &pubkey)
            .map_err(|_| Bip70Error::SignatureError("PaymentACK signature verification failed".to_string()))?;
        
        Ok(())
    }
}

/// BIP70 Payment Protocol server (for receiving payments via P2P)
pub struct PaymentProtocolServer;

impl PaymentProtocolServer {
    /// Create signed payment request for P2P network
    pub fn create_signed_payment_request(
        details: PaymentDetails,
        merchant_private_key: &secp256k1::SecretKey,
        authorized_refunds: Option<Vec<SignedRefundAddress>>,
    ) -> Result<(PaymentRequest, crate::network::protocol::PaymentRequestMessage), Bip70Error> {
        let mut payment_request = PaymentRequest {
            payment_details: details,
            merchant_pubkey: None,
            signature: None,
            authorized_refund_addresses: authorized_refunds,
        };
        
        // Sign payment request
        payment_request.sign(merchant_private_key)?;
        
        // Get merchant public key
        let merchant_pubkey = payment_request.merchant_pubkey
            .as_ref()
            .cloned()
            .ok_or_else(|| Bip70Error::SignatureError("Failed to get merchant pubkey".to_string()))?;
        
        // Create payment ID from payment details hash
        let serialized = bincode::serialize(&payment_request.payment_details)
            .map_err(|e| Bip70Error::SerializationError(e.to_string()))?;
        let mut hasher = Sha256::new();
        hasher.update(&serialized);
        let payment_id: [u8; 32] = hasher.finalize().into();
        
        // Create P2P message
        let p2p_message = crate::network::protocol::PaymentRequestMessage {
            payment_request: payment_request.clone(),
            merchant_signature: payment_request.signature.clone()
                .ok_or_else(|| Bip70Error::SignatureError("No signature".to_string()))?,
            merchant_pubkey: merchant_pubkey,
            payment_id: payment_id.to_vec(),
        };
        
        Ok((payment_request, p2p_message))
    }

    /// Process incoming payment from P2P network
    pub fn process_payment(
        payment_msg: &crate::network::protocol::PaymentMessage,
        original_request: &PaymentRequest,
    ) -> Result<crate::network::protocol::PaymentACKMessage, Bip70Error> {
        // Validate payment
        payment_msg.payment.validate()?;

        // Validate refund addresses if provided
        if let Some(ref authorized_refunds) = original_request.authorized_refund_addresses {
            payment_msg.payment.validate_refund_addresses(authorized_refunds)?;
        }

        // TODO: Verify transactions match PaymentRequest outputs
        // TODO: Validate merchant_data matches original request
        
        let payment_ack = PaymentACK {
            payment: payment_msg.payment.clone(),
            memo: Some("Payment received".to_string()),
        };
        
        // Sign payment ACK
        let _merchant_pubkey = original_request.merchant_pubkey
            .as_ref()
            .ok_or_else(|| Bip70Error::SignatureError("No merchant pubkey in request".to_string()))?;
        
        // For signing, we'd need the merchant's private key - this should be passed in
        // For now, return unsigned ACK (real implementation would sign it)
        Ok(crate::network::protocol::PaymentACKMessage {
            payment_ack,
            payment_id: payment_msg.payment_id.clone(),
            merchant_signature: Vec::new(), // TODO: Sign with merchant key
        })
    }
    
    /// Sign a refund address for inclusion in PaymentRequest
    pub fn sign_refund_address(
        address: PaymentOutput,
        merchant_private_key: &secp256k1::SecretKey,
    ) -> Result<SignedRefundAddress, Bip70Error> {
        // Serialize address for signing
        let serialized = bincode::serialize(&address)
            .map_err(|e| Bip70Error::SerializationError(e.to_string()))?;
        
        // Hash address
        let mut hasher = Sha256::new();
        hasher.update(&serialized);
        let hash = hasher.finalize();
        
        // Sign
        let message = Message::from_digest_slice(&hash)
            .map_err(|e| Bip70Error::SignatureError(format!("Invalid message: {}", e)))?;
        
        let secp = Secp256k1::new();
        let signature = secp.sign_ecdsa(&message, merchant_private_key);
        
        Ok(SignedRefundAddress {
            address,
            signature: signature.serialize_compact().to_vec(),
        })
    }
    
    /// Verify signed refund address
    pub fn verify_refund_address(
        signed_refund: &SignedRefundAddress,
        merchant_pubkey: &[u8],
    ) -> Result<(), Bip70Error> {
        let pubkey = secp256k1::PublicKey::from_slice(merchant_pubkey)
            .map_err(|e| Bip70Error::SignatureError(format!("Invalid pubkey: {}", e)))?;
        
        let serialized = bincode::serialize(&signed_refund.address)
            .map_err(|e| Bip70Error::SerializationError(e.to_string()))?;
        
        let mut hasher = Sha256::new();
        hasher.update(&serialized);
        let hash = hasher.finalize();
        
        let message = Message::from_digest_slice(&hash)
            .map_err(|e| Bip70Error::SignatureError(format!("Invalid message: {}", e)))?;
        
        let signature = Signature::from_compact(&signed_refund.signature)
            .map_err(|e| Bip70Error::SignatureError(format!("Invalid signature: {}", e)))?;
        
        let secp = Secp256k1::new();
        secp.verify_ecdsa(&message, &signature, &pubkey)
            .map_err(|_| Bip70Error::SignatureError("Refund address signature verification failed".to_string()))?;
        
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_payment_request_creation() {
        let output = PaymentOutput {
            script: vec![0x51], // OP_1 (placeholder)
            amount: Some(100000), // 0.001 BTC
        };

        let request = PaymentRequest::new(
            "main".to_string(),
            vec![output],
            1234567890,
        );

        assert_eq!(request.payment_details.network, "main");
        assert_eq!(request.payment_details.outputs.len(), 1);
        assert_eq!(request.payment_details.time, 1234567890);
    }

    #[test]
    fn test_payment_request_validation() {
        let request = PaymentRequest::new(
            "main".to_string(),
            vec![PaymentOutput {
                script: vec![0x51],
                amount: Some(100000),
            }],
            1234567890,
        );

        assert!(request.validate().is_ok());
    }

    #[test]
    fn test_payment_request_expired() {
        let expired_time = 1000;
        let request = PaymentRequest::new(
            "main".to_string(),
            vec![PaymentOutput {
                script: vec![0x51],
                amount: Some(100000),
            }],
            expired_time,
        ).with_expires(1001);

        // Should fail validation (expired)
        let result = request.validate();
        assert!(result.is_err());
    }

    #[test]
    fn test_payment_creation() {
        let tx = vec![0x01, 0x00, 0x00, 0x00]; // Placeholder transaction
        let payment = Payment::new(vec![tx.clone()]);

        assert_eq!(payment.transactions.len(), 1);
        assert_eq!(payment.transactions[0], tx);
    }

    #[test]
    fn test_payment_validation() {
        let payment = Payment::new(vec![vec![0x01, 0x02, 0x03]]);
        assert!(payment.validate().is_ok());

        let empty_payment = Payment::new(vec![]);
        assert!(empty_payment.validate().is_err());
    }
}

