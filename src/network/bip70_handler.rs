//! BIP70 Payment Protocol P2P Message Handlers
//!
//! Handles incoming BIP70 messages from the P2P network.
//! Uses unified PaymentProcessor for transport-agnostic payment processing.

use crate::network::protocol::{
    GetPaymentRequestMessage, PaymentACKMessage, PaymentMessage, PaymentRequestMessage,
};
use crate::payment::processor::PaymentProcessor;
use anyhow::Result;
use blvm_protocol::payment::Bip70Error;
use hex;
use std::sync::Arc;

/// Handle GetPaymentRequest message
///
/// Merchant node responds with PaymentRequest signed with their Bitcoin key.
/// Uses unified PaymentProcessor for transport-agnostic processing.
pub async fn handle_get_payment_request(
    request: &GetPaymentRequestMessage,
    processor: Option<Arc<PaymentProcessor>>,
) -> Result<PaymentRequestMessage> {
    let processor = processor.ok_or_else(|| anyhow::anyhow!("Payment processor not available"))?;

    // Convert payment_id from bytes to string
    let payment_id = hex::encode(&request.payment_id);

    // Get payment request using unified processor
    let payment_request = processor
        .get_payment_request(&payment_id)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get payment request: {}", e))?;

    // Extract signature from embedded PaymentRequest signature field
    // The PaymentRequest is already signed when created via PaymentProcessor
    let merchant_signature = payment_request
        .signature
        .clone()
        .unwrap_or_default();

    // Extract merchant pubkey from PaymentRequest if available, otherwise use from request
    let merchant_pubkey = payment_request
        .merchant_pubkey
        .clone()
        .unwrap_or_else(|| request.merchant_pubkey.clone());

    // Convert to P2P message format
    Ok(PaymentRequestMessage {
        payment_request,
        payment_id: request.payment_id.clone(),
        merchant_pubkey,
        merchant_signature,
        #[cfg(feature = "ctv")]
        covenant_proof: None, // Can be set if CTV proof is created
    })
}

/// Handle Payment message
///
/// Merchant node processes payment and responds with PaymentACK.
/// Uses unified PaymentProcessor for transport-agnostic processing.
pub async fn handle_payment(
    payment_msg: &PaymentMessage,
    processor: Option<Arc<PaymentProcessor>>,
    merchant_private_key: Option<&secp256k1::SecretKey>,
) -> Result<PaymentACKMessage> {
    let processor = processor.ok_or_else(|| anyhow::anyhow!("Payment processor not available"))?;

    // Convert payment_id from bytes to string
    let payment_id = hex::encode(&payment_msg.payment_id);

    // Process payment using unified processor
    // PaymentProtocolServer::process_payment() signs the PaymentACK if merchant_key is provided
    let payment_ack = processor
        .process_payment(
            payment_msg.payment.clone(),
            payment_id,
            merchant_private_key,
        )
        .await
        .map_err(|e| anyhow::anyhow!("Payment processing failed: {}", e))?;

    // Extract signature from embedded PaymentACK signature field
    // The PaymentACK is already signed by PaymentProtocolServer::process_payment() if merchant_key was provided
    let merchant_signature = payment_ack
        .signature
        .clone()
        .unwrap_or_default();

    // Convert to P2P message format
    Ok(PaymentACKMessage {
        payment_ack,
        payment_id: payment_msg.payment_id.clone(),
        merchant_signature,
    })
}

/// Validate PaymentRequest message from P2P network
pub fn validate_payment_request_message(msg: &PaymentRequestMessage) -> Result<(), Bip70Error> {
    use blvm_protocol::payment::PaymentProtocolClient;
    PaymentProtocolClient::validate_payment_request(
        &msg.payment_request,
        Some(&msg.merchant_pubkey),
    )
}

/// Validate PaymentACK message from merchant
pub fn validate_payment_ack_message(
    ack: &PaymentACKMessage,
    merchant_pubkey: &[u8],
) -> Result<(), Bip70Error> {
    use blvm_protocol::payment::PaymentProtocolClient;
    PaymentProtocolClient::validate_payment_ack(
        &ack.payment_ack,
        &ack.merchant_signature,
        merchant_pubkey,
    )
}
