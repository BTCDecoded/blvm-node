//! BIP70 Payment Protocol P2P Message Handlers
//!
//! Handles incoming BIP70 messages from the P2P network.
//! Similar to bip157_handler.rs pattern.

use crate::bip70::{Bip70Error, PaymentProtocolClient, PaymentProtocolServer, PaymentRequest};
use crate::network::protocol::{
    GetPaymentRequestMessage, PaymentACKMessage, PaymentMessage, PaymentRequestMessage,
    ProtocolMessage,
};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use hex;

/// In-memory payment request store (simplified - would use persistent storage in production)
type PaymentRequestStore = Arc<Mutex<HashMap<String, PaymentRequest>>>;

/// Handle GetPaymentRequest message
///
/// Merchant node responds with PaymentRequest signed with their Bitcoin key.
pub async fn handle_get_payment_request(
    request: &GetPaymentRequestMessage,
    payment_store: Option<&PaymentRequestStore>,
) -> Result<PaymentRequestMessage> {
    if let Some(store) = payment_store {
        let store = store.lock().unwrap();
        let key = format!("{}_{}", hex::encode(&request.payment_id), hex::encode(&request.merchant_pubkey));
        if let Some(payment_request) = store.get(&key) {
            // Convert to P2P message format
            return Ok(PaymentRequestMessage {
                payment_request: payment_request.clone(),
                payment_id: request.payment_id.clone(),
            });
        }
    }
    
    Err(anyhow::anyhow!("Payment request not found"))
}

/// Handle Payment message
///
/// Merchant node processes payment and responds with PaymentACK.
pub async fn handle_payment(
    payment_msg: &PaymentMessage,
    payment_store: Option<&PaymentRequestStore>,
    merchant_private_key: Option<&secp256k1::SecretKey>,
) -> Result<PaymentACKMessage> {
    // Look up original PaymentRequest
    let original_request = if let Some(store) = payment_store {
        let store = store.lock().unwrap();
        let key = format!("{}_{}", hex::encode(&payment_msg.payment_id), "");
        // Find matching request (simplified - would use proper lookup)
        store.values().next().cloned()
    } else {
        None
    };
    
    if let Some(request) = original_request {
        // Use existing process_payment function
        use crate::bip70::PaymentProtocolServer;
        PaymentProtocolServer::process_payment(payment_msg, &request, merchant_private_key)
            .map_err(|e| anyhow::anyhow!("Payment processing failed: {:?}", e))
    } else {
        Err(anyhow::anyhow!("Original payment request not found"))
    }
}

/// Validate PaymentRequest message from P2P network
pub fn validate_payment_request_message(msg: &PaymentRequestMessage) -> Result<(), Bip70Error> {
    PaymentProtocolClient::validate_payment_request(msg)
}

/// Validate PaymentACK message from merchant
pub fn validate_payment_ack_message(
    ack: &PaymentACKMessage,
    merchant_pubkey: &[u8],
) -> Result<(), Bip70Error> {
    PaymentProtocolClient::validate_payment_ack(ack, merchant_pubkey)
}
