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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::protocol::GetPaymentRequestMessage;
    use std::net::SocketAddr;

    #[tokio::test(flavor = "multi_thread")]
    async fn handle_get_payment_request_without_processor_errors() {
        let listen: SocketAddr = "127.0.0.1:18470".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:18471".parse().unwrap();
        let nm = NetworkManager::new(listen);
        let request = GetPaymentRequestMessage {
            merchant_pubkey: vec![0x02; 33],
            payment_id: vec![0x01; 32],
            network: "regtest".into(),
        };
        assert!(nm.handle_get_payment_request(peer, request).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn handle_payment_without_processor_errors() {
        use crate::network::protocol::PaymentMessage;

        let listen: SocketAddr = "127.0.0.1:18472".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:18473".parse().unwrap();
        let nm = NetworkManager::new(listen);
        let payment = PaymentMessage {
            payment_id: vec![0x02; 32],
            payment: blvm_protocol::payment::Payment {
                merchant_data: None,
                transactions: vec![],
                refund_to: None,
                memo: None,
            },
            customer_signature: None,
        };
        assert!(nm.handle_payment(peer, payment).await.is_err());
    }
}
