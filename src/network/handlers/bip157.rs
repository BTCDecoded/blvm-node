//! BIP157/158 block filter handlers (GetCfilters, GetCfheaders, GetCfcheckpt).

use crate::network::bandwidth_protection::ServiceType;
use crate::network::bip157_handler::{handle_getcfilters, handle_getcfheaders, handle_getcfcheckpt};
use crate::network::network_manager::NetworkManager;
use crate::network::protocol::{ProtocolMessage, ProtocolParser};
use crate::network::transport::TransportAddr;
use crate::network::NetworkMessage;
use anyhow::Result;
use std::net::SocketAddr;
use std::time::Instant;
use tracing::warn;

impl NetworkManager {
    /// Handle GetCfilters request from a peer
    pub(crate) async fn handle_getcfilters_request(
        &self,
        data: Vec<u8>,
        peer_addr: SocketAddr,
    ) -> Result<()> {
        match self
            .bandwidth_protection()
            .check_service_request(ServiceType::Filters, peer_addr)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return Err(anyhow::anyhow!(
                    "Filter service bandwidth limit exceeded for peer {}",
                    peer_addr
                ));
            }
            Err(e) => {
                warn!("Bandwidth check error for filter service: {}", e);
                return Err(anyhow::anyhow!("Bandwidth check failed: {}", e));
            }
        }

        self.bandwidth_protection()
            .record_service_request(ServiceType::Filters, peer_addr)
            .await;

        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let request = match protocol_msg {
            ProtocolMessage::GetCfilters(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected GetCfilters message")),
        };

        let cpu_start = Instant::now();
        let storage_ref = self.storage().as_ref();
        let responses = handle_getcfilters(&request, self.filter_service(), storage_ref)?;

        let cpu_time_ms = cpu_start.elapsed().as_millis() as u64;
        const MAX_FILTER_CPU_TIME_MS: u64 = 5000;
        if cpu_time_ms > MAX_FILTER_CPU_TIME_MS {
            warn!(
                "Filter service CPU time limit exceeded ({}ms > {}ms) for peer {}, disconnecting",
                cpu_time_ms, MAX_FILTER_CPU_TIME_MS, peer_addr
            );

            let mut pm = self.peer_manager_ref().lock().await;
            let should_disconnect = if let Some(peer) = pm.get_peer(&TransportAddr::Tcp(peer_addr))
            {
                !peer.has_noban_permission()
            } else {
                true
            };
            drop(pm);

            if should_disconnect {
                let _ = self
                    .peer_tx()
                    .send(NetworkMessage::PeerDisconnected(TransportAddr::Tcp(
                        peer_addr,
                    )));
                return Err(anyhow::anyhow!(
                    "Filter service CPU time limit exceeded: {}ms",
                    cpu_time_ms
                ));
            } else {
                warn!(
                    "Peer {} has NoBan permission, not disconnecting for filter CPU time violation",
                    peer_addr
                );
                return Ok(());
            }
        }

        let mut total_bytes = 0u64;
        for response in responses {
            let response_wire = ProtocolParser::serialize_message(&response)?;
            total_bytes += response_wire.len() as u64;
            self.send_to_peer(peer_addr, response_wire).await?;
        }

        if total_bytes > 0 {
            self.bandwidth_protection()
                .record_service_bandwidth(ServiceType::Filters, peer_addr, total_bytes)
                .await;
        }

        Ok(())
    }

    /// Handle GetCfheaders request from a peer
    pub(crate) async fn handle_getcfheaders_request(
        &self,
        data: Vec<u8>,
        peer_addr: SocketAddr,
    ) -> Result<()> {
        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let request = match protocol_msg {
            ProtocolMessage::GetCfheaders(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected GetCfheaders message")),
        };

        let response = handle_getcfheaders(&request, self.filter_service())?;
        let response_wire = ProtocolParser::serialize_message(&response)?;
        self.send_to_peer(peer_addr, response_wire).await?;

        Ok(())
    }

    /// Handle GetCfcheckpt request from a peer
    pub(crate) async fn handle_getcfcheckpt_request(
        &self,
        data: Vec<u8>,
        peer_addr: SocketAddr,
    ) -> Result<()> {
        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let request = match protocol_msg {
            ProtocolMessage::GetCfcheckpt(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected GetCfcheckpt message")),
        };

        let response = handle_getcfcheckpt(&request, self.filter_service())?;
        let response_wire = ProtocolParser::serialize_message(&response)?;
        self.send_to_peer(peer_addr, response_wire).await?;

        Ok(())
    }
}
