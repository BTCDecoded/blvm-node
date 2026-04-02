//! Payment protocol handlers (GetPaymentRequest, Payment) - P2P BIP70.

use crate::network::bip70_handler::{handle_get_payment_request, handle_payment};
use crate::network::network_manager::NetworkManager;
use crate::network::protocol::{ProtocolMessage, ProtocolParser};
use anyhow::Result;
use std::net::SocketAddr;

impl NetworkManager {
    /// Handle GetPaymentRequest message (P2P BIP70)
    pub(crate) async fn handle_get_payment_request(
        &self,
        peer_addr: SocketAddr,
        message: crate::network::protocol::GetPaymentRequestMessage,
    ) -> Result<()> {
        let processor = self
            .payment_processor()
            .lock()
            .await
            .as_ref()
            .map(std::sync::Arc::clone);
        let response_msg = handle_get_payment_request(&message, processor).await?;
        let response = ProtocolMessage::PaymentRequest(response_msg);
        let wire_msg = ProtocolParser::serialize_message(&response)?;
        self.send_to_peer(peer_addr, wire_msg).await?;
        Ok(())
    }

    /// Handle Payment message (P2P BIP70)
    pub(crate) async fn handle_payment(
        &self,
        peer_addr: SocketAddr,
        message: crate::network::protocol::PaymentMessage,
    ) -> Result<()> {
        let processor = self
            .payment_processor()
            .lock()
            .await
            .as_ref()
            .map(std::sync::Arc::clone);
        let merchant_key_guard = self.merchant_key().lock().await;
        let merchant_key = merchant_key_guard.as_ref();
        let response_msg = handle_payment(&message, processor, merchant_key).await?;
        let response = ProtocolMessage::PaymentACK(response_msg);
        let wire_msg = ProtocolParser::serialize_message(&response)?;
        self.send_to_peer(peer_addr, wire_msg).await?;
        Ok(())
    }
}
