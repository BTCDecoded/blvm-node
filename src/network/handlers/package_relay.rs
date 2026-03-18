//! BIP331 Package Relay handler (PkgTxn).

use crate::network::bandwidth_protection::ServiceType;
use crate::network::network_manager::NetworkManager;
use crate::network::package_relay::PackageRelay;
use crate::network::package_relay_handler::handle_pkgtxn;
use crate::network::protocol::{ProtocolMessage, ProtocolParser};
use anyhow::Result;
use blvm_protocol::Transaction;
use std::net::SocketAddr;
use tracing::warn;

impl NetworkManager {
    /// Handle PkgTxn message from a peer
    pub(crate) async fn handle_pkgtxn_request(
        &self,
        data: Vec<u8>,
        peer_addr: SocketAddr,
    ) -> Result<()> {
        match self
            .bandwidth_protection()
            .check_service_request(ServiceType::PackageRelay, peer_addr)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return Err(anyhow::anyhow!(
                    "Package relay bandwidth limit exceeded for peer {}",
                    peer_addr
                ));
            }
            Err(e) => {
                warn!("Bandwidth check error for package relay: {}", e);
                return Err(anyhow::anyhow!("Bandwidth check failed: {}", e));
            }
        }

        self.bandwidth_protection()
            .record_service_request(ServiceType::PackageRelay, peer_addr)
            .await;

        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let request = match protocol_msg {
            ProtocolMessage::PkgTxn(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected PkgTxn message")),
        };

        let mut relay = PackageRelay::new();
        let mut response_bytes = 0u64;
        if let Some(reject) = handle_pkgtxn(&mut relay, &request)? {
            let response_wire =
                ProtocolParser::serialize_message(&ProtocolMessage::PkgTxnReject(reject))?;
            response_bytes = response_wire.len() as u64;
            self.send_to_peer(peer_addr, response_wire).await?;
        }

        let mut txs: Vec<Transaction> = Vec::with_capacity(request.transactions.len());
        for raw in &request.transactions {
            if let Ok(tx) = bincode::deserialize::<Transaction>(raw) {
                txs.push(tx);
            }
        }
        let _ = self.submit_transactions_to_mempool(&txs).await;

        if response_bytes > 0 {
            self.bandwidth_protection()
                .record_service_bandwidth(ServiceType::PackageRelay, peer_addr, response_bytes)
                .await;
        }
        Ok(())
    }
}
