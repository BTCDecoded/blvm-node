//! Address relay handlers (GetAddr, Addr).

use crate::network::network_manager::NetworkManager;
use crate::network::protocol::{
    AddrMessage, AddrV2Message, NetworkAddress, ProtocolMessage, ProtocolParser,
};
use crate::utils::current_timestamp;
use anyhow::Result;
use std::net::SocketAddr;
use tracing::{debug, warn};

impl NetworkManager {
    /// Handle GetAddr request - return known addresses
    pub(crate) async fn handle_get_addr(&self, peer_addr: SocketAddr) -> Result<()> {
        // Answer GetAddr at most once per connection (Core behaviour).
        // A second GetAddr on the same connection gets an empty response.
        let already_responded = {
            let mut responded = self.getaddr_responded.lock().unwrap();
            if responded.contains(&peer_addr) {
                true
            } else {
                responded.insert(peer_addr);
                false
            }
        }; // guard dropped here

        if already_responded {
            debug!(
                "GetAddr from {}: already answered once this connection, returning empty",
                peer_addr
            );
            let empty = crate::network::protocol::ProtocolMessage::Addr(
                crate::network::protocol::AddrMessage { addresses: vec![] },
            );
            if let Ok(wire) = crate::network::protocol::ProtocolParser::serialize_message(&empty) {
                if let Err(e) = self.send_to_peer(peer_addr, wire).await {
                    warn!(
                        "GetAddr from {}: failed to send empty already-answered response: {}",
                        peer_addr, e
                    );
                }
            }
            return Ok(());
        }

        let ban_list = self.ban_list().read().await.clone();
        let connected_peers: Vec<SocketAddr> = {
            let pm = self.peer_manager_ref().lock().await;
            pm.peer_socket_addresses()
        };

        let max_addr = self.protocol_limits().max_addr_to_send;
        let addresses = {
            let db = self.address_database().write().await;
            let fresh = db.get_fresh_addresses(2500.max(max_addr));
            db.filter_addresses(fresh, &ban_list, &connected_peers)
                .into_iter()
                .take(max_addr)
                .collect::<Vec<_>>()
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
            return self
                .disconnect_for_protocol_violation(peer_addr, "addr message size exceeded", true)
                .await;
        }

        {
            let mut db = self.address_database().write().await;
            for addr in &msg.addresses {
                // Use the services field FROM the addr entry itself — each NetworkAddress
                // includes the services that the sending peer reports for that address.
                // Previously we used `peer_services` (the sender's own services), which
                // was wrong and caused NODE_NETWORK_LIMITED to contaminate addresses
                // received from pruned peers, poisoning the address DB.
                db.add_address(addr.clone(), addr.services);
            }
        }

        let relayable: Vec<NetworkAddress> = msg
            .addresses
            .into_iter()
            .filter(|addr| crate::network::address_db::address_within_relay_horizon(addr.time))
            .collect();
        if !relayable.is_empty() {
            self.relay_addresses(peer_addr, &relayable).await?;
        }

        Ok(())
    }

    /// Handle BIP155 `addrv2`: map IPv4/IPv6 entries to legacy [`NetworkAddress`] and reuse [`Addr`] storage/relay.
    pub(crate) async fn handle_addr_v2(
        &self,
        peer_addr: SocketAddr,
        msg: AddrV2Message,
    ) -> Result<()> {
        let max_addr = self.protocol_limits().max_addr_to_send;
        if msg.addresses.len() > max_addr {
            warn!(
                "addrv2 message size = {} exceeds max_addr_to_send ({}), disconnecting peer {}",
                msg.addresses.len(),
                max_addr,
                peer_addr
            );
            return self
                .disconnect_for_protocol_violation(peer_addr, "addrv2 message size exceeded", true)
                .await;
        }

        let legacy: Vec<NetworkAddress> = msg
            .addresses
            .iter()
            .filter_map(|a| a.to_legacy())
            .map(|a| NetworkAddress {
                time: a.time,
                services: a.services,
                ip: a.ip,
                port: a.port,
            })
            .collect();

        if legacy.is_empty() {
            debug!(
                "AddrV2 from {}: {} entr(ies), none map to legacy net_addr (e.g. onion-only)",
                peer_addr,
                msg.addresses.len()
            );
            return Ok(());
        }

        self.handle_addr(peer_addr, AddrMessage { addresses: legacy })
            .await
    }

    /// Relay addresses to other peers (excluding sender)
    pub(crate) async fn relay_addresses(
        &self,
        sender_addr: SocketAddr,
        addresses: &[NetworkAddress],
    ) -> Result<()> {
        let now = current_timestamp();
        let min_interval = self.network_timing().addr_relay_min_interval_seconds;

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

        let max_addr = self
            .network_timing()
            .max_addresses_per_addr_message
            .min(self.protocol_limits().max_addr_to_send);
        let addresses_to_relay: Vec<NetworkAddress> = filtered.into_iter().take(max_addr).collect();

        let addr_msg = AddrMessage {
            addresses: addresses_to_relay,
        };
        let relay_msg = ProtocolMessage::Addr(addr_msg);
        let wire_msg = ProtocolParser::serialize_message(&relay_msg)?;

        let mut peer_addrs: Vec<SocketAddr> = {
            let pm = self.peer_manager_ref().lock().await;
            pm.peer_socket_addresses()
                .into_iter()
                .filter(|addr| *addr != sender_addr)
                .collect()
        };
        peer_addrs.sort();
        let subset: Vec<SocketAddr> = {
            let last_sent = self.last_addr_sent().lock().await;
            peer_addrs
                .into_iter()
                .filter(|addr| {
                    now.saturating_sub(*last_sent.get(addr).unwrap_or(&0)) >= min_interval
                })
                .take(2)
                .collect()
        };

        let mut sent = Vec::new();
        for peer_addr in subset {
            if let Err(e) = self.send_to_peer(peer_addr, wire_msg.clone()).await {
                warn!("Failed to relay addresses to {}: {}", peer_addr, e);
            } else {
                sent.push(peer_addr);
            }
        }
        if !sent.is_empty() {
            let mut last_sent = self.last_addr_sent().lock().await;
            for peer_addr in sent {
                last_sent.insert(peer_addr, now);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::NetworkManager;
    use crate::network::protocol::{AddrMessage, NetworkAddress};
    use std::net::SocketAddr;

    fn legacy_addr(port: u16) -> NetworkAddress {
        let mut ip = [0u8; 16];
        ip[12..16].copy_from_slice(&[127, 0, 0, 1]);
        NetworkAddress {
            services: 1,
            ip,
            port,
            time: current_timestamp() as u32,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn handle_get_addr_smoke() {
        let listen: SocketAddr = "127.0.0.1:18400".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:18401".parse().unwrap();
        let nm = NetworkManager::new(listen);
        let _ = nm.handle_get_addr(peer).await;
        nm.handle_get_addr(peer).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn handle_addr_stores_and_relays() {
        let listen: SocketAddr = "127.0.0.1:18402".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:18403".parse().unwrap();
        let nm = NetworkManager::new(listen);
        let msg = AddrMessage {
            addresses: vec![legacy_addr(8333)],
        };
        nm.handle_addr(peer, msg).await.unwrap();
    }

    #[tokio::test]
    async fn second_addr_is_offered_to_a_peer_the_first_did_not_use() {
        use crate::network::peer::Peer;
        use crate::network::transport::TransportAddr;

        let listen: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let nm = NetworkManager::new(listen);
        let sender: SocketAddr = "127.0.0.1:18420".parse().unwrap();
        let mut incoming = Vec::new();
        for port in [18421u16, 18422, 18423] {
            let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
            let (peer, rx) = Peer::pair_for_testing(addr);
            nm.peer_manager_ref()
                .lock()
                .await
                .add_peer(TransportAddr::Tcp(addr), peer)
                .unwrap();
            incoming.push((addr, rx));
        }
        let addresses = vec![legacy_addr_public(8333)];
        nm.relay_addresses(sender, &addresses).await.unwrap();
        nm.relay_addresses(sender, &addresses).await.unwrap();
        let mut got = 0usize;
        for (_addr, rx) in &mut incoming {
            if rx.try_recv().is_ok() {
                got += 1;
            }
        }
        assert_eq!(got, 3);
    }

    fn legacy_addr_public(port: u16) -> NetworkAddress {
        let mut ip = [0u8; 16];
        ip[10] = 0xff;
        ip[11] = 0xff;
        ip[12..16].copy_from_slice(&[8, 8, 8, 8]);
        NetworkAddress {
            services: 1,
            ip,
            port,
            time: current_timestamp() as u32,
        }
    }
}
