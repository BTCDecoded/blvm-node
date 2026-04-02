//! Address relay handlers (GetAddr, Addr).

use crate::network::network_manager::NetworkManager;
use crate::network::protocol::{AddrMessage, NetworkAddress, ProtocolMessage, ProtocolParser};
use crate::network::transport::TransportAddr;
use crate::network::NetworkMessage;
use crate::utils::current_timestamp;
use anyhow::Result;
use std::net::SocketAddr;
use tracing::warn;

impl NetworkManager {
    /// Handle GetAddr request - return known addresses
    pub(crate) async fn handle_get_addr(&self, peer_addr: SocketAddr) -> Result<()> {
        let ban_list = self.ban_list().read().await.clone();
        let connected_peers: Vec<SocketAddr> = {
            let pm = self.peer_manager_ref().lock().await;
            pm.peer_socket_addresses()
        };

        let addresses = {
            let db = self.address_database().write().await;
            let fresh = db.get_fresh_addresses(2500);
            db.filter_addresses(fresh, &ban_list, &connected_peers)
        };

        let addr_msg = AddrMessage { addresses };
        let response = ProtocolMessage::Addr(addr_msg);
        let wire_msg = ProtocolParser::serialize_message(&response)?;

        self.send_to_peer(peer_addr, wire_msg).await?;
        Ok(())
    }

    /// Handle Addr message - store addresses and optionally relay
    pub(crate) async fn handle_addr(&self, peer_addr: SocketAddr, msg: AddrMessage) -> Result<()> {
        let max_addr = self.protocol_limits().max_addr_to_send;
        if msg.addresses.len() > max_addr {
            warn!(
                "addr message size = {} exceeds max_addr_to_send ({}), disconnecting peer {}",
                msg.addresses.len(),
                max_addr,
                peer_addr
            );
            let _ = self
                .peer_tx()
                .send(NetworkMessage::PeerDisconnected(TransportAddr::Tcp(
                    peer_addr,
                )));
            return Err(anyhow::anyhow!("addr message size exceeded"));
        }

        let peer_services = {
            let peer_states = self.peer_states().read().await;
            peer_states
                .get(&peer_addr)
                .map(|state| state.services)
                .unwrap_or(0)
        };

        {
            let mut db = self.address_database().write().await;
            for addr in &msg.addresses {
                db.add_address(addr.clone(), peer_services);
            }
        }

        self.relay_addresses(peer_addr, &msg.addresses).await?;

        Ok(())
    }

    /// Relay addresses to other peers (excluding sender)
    pub(crate) async fn relay_addresses(
        &self,
        sender_addr: SocketAddr,
        addresses: &[NetworkAddress],
    ) -> Result<()> {
        let now = current_timestamp();
        let min_interval = 2 * 60 * 60 + 24 * 60;

        {
            let last_sent = *self.last_addr_sent().lock().await;
            if now.saturating_sub(last_sent) < min_interval {
                return Ok(());
            }
        }

        let ban_list = self.ban_list().read().await.clone();
        let connected_peers: Vec<SocketAddr> = {
            let pm = self.peer_manager_ref().lock().await;
            pm.peer_socket_addresses()
        };

        let filtered = {
            let db = self.address_database().read().await;
            db.filter_addresses(addresses.to_vec(), &ban_list, &connected_peers)
        };

        if filtered.is_empty() {
            return Ok(());
        }

        let max_addr = self.protocol_limits().max_addr_to_send;
        let addresses_to_relay: Vec<NetworkAddress> = filtered.into_iter().take(max_addr).collect();

        let addr_msg = AddrMessage {
            addresses: addresses_to_relay,
        };
        let relay_msg = ProtocolMessage::Addr(addr_msg);
        let wire_msg = ProtocolParser::serialize_message(&relay_msg)?;

        let peer_addrs: Vec<SocketAddr> = {
            let pm = self.peer_manager_ref().lock().await;
            pm.peer_socket_addresses()
                .into_iter()
                .filter(|addr| *addr != sender_addr)
                .collect()
        };

        for peer_addr in peer_addrs {
            if let Err(e) = self.send_to_peer(peer_addr, wire_msg.clone()).await {
                warn!("Failed to relay addresses to {}: {}", peer_addr, e);
            }
        }

        *self.last_addr_sent().lock().await = now;

        Ok(())
    }
}
