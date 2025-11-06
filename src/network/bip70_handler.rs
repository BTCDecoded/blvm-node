//! BIP70 Payment Protocol P2P Message Handlers
//!
//! Handles incoming BIP70 messages from the P2P network.
//! Similar to bip157_handler.rs pattern.

use crate::bip70::{Bip70Error, PaymentProtocolClient, PaymentProtocolServer};
use crate::network::protocol::{
    GetPaymentRequestMessage, PaymentACKMessage, PaymentMessage, PaymentRequestMessage,
    ProtocolMessage,
};
use anyhow::Result;

/// Handle GetPaymentRequest message
///
/// Merchant node responds with PaymentRequest signed with their Bitcoin key.
pub async fn handle_get_payment_request(
    request: &GetPaymentRequestMessage,
    // In real implementation: merchant_payment_store: &MerchantPaymentStore
) -> Result<PaymentRequestMessage> {
    // TODO: Look up payment request by payment_id and merchant_pubkey
    // For now, return error indicating not implemented

    Err(anyhow::anyhow!(
        "GetPaymentRequest handler not yet implemented - requires merchant payment store"
    ))
}

/// Handle Payment message
///
/// Merchant node processes payment and responds with PaymentACK.
pub async fn handle_payment(
    payment_msg: &PaymentMessage,
    // In real implementation: payment_store: &PaymentStore, original_request: &PaymentRequest
) -> Result<PaymentACKMessage> {
    // TODO: Look up original PaymentRequest by payment_id
    // TODO: Validate payment against original request
    // TODO: Process payment and generate PaymentACK

    Err(anyhow::anyhow!(
        "Payment handler not yet implemented - requires payment processing store"
    ))
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
