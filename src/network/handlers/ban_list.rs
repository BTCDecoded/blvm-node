//! Ban list sharing handlers (GetBanList, BanList).

use crate::network::ban_list_merging::{
    calculate_ban_list_hash, validate_ban_entry, verify_ban_list_hash,
};
use crate::network::network_manager::NetworkManager;
use crate::network::protocol::{
    BanEntry, BanListMessage, NetworkAddress, ProtocolMessage, ProtocolParser,
};
use crate::utils::current_timestamp;
use anyhow::Result;
use std::net::{IpAddr, SocketAddr};
use tracing::{debug, warn};

impl NetworkManager {
    /// Handle GetBanList message - respond with ban list or hash
    pub(crate) async fn handle_get_ban_list(
        &self,
        peer_addr: SocketAddr,
        msg: crate::network::protocol::GetBanListMessage,
    ) -> Result<()> {
        debug!(
            "GetBanList request from {}: full={}, min_duration={}",
            peer_addr, msg.request_full, msg.min_ban_duration
        );

        let ban_list = self.ban_list().read().await;
        let now = current_timestamp();

        let mut ban_entries: Vec<BanEntry> = Vec::new();
        for (addr, &unban_timestamp) in ban_list.iter() {
            if unban_timestamp != u64::MAX && now >= unban_timestamp {
                continue;
            }

            if msg.min_ban_duration > 0 {
                let ban_duration = if unban_timestamp == u64::MAX {
                    u64::MAX
                } else {
                    unban_timestamp.saturating_sub(now)
                };
                if ban_duration < msg.min_ban_duration {
                    continue;
                }
            }

            let ip_bytes = match addr.ip() {
                std::net::IpAddr::V4(ipv4) => {
                    let mut bytes = [0u8; 16];
                    bytes[12..16].copy_from_slice(&ipv4.octets());
                    bytes
                }
                std::net::IpAddr::V6(ipv6) => ipv6.octets(),
            };

            ban_entries.push(BanEntry {
                addr: NetworkAddress {
                    services: 0,
                    ip: ip_bytes,
                    port: addr.port(),
                },
                unban_timestamp,
                reason: Some("DoS protection".to_string()),
            });
        }

        let ban_list_hash = calculate_ban_list_hash(&ban_entries);
        let ban_entries_count = ban_entries.len();

        let response = BanListMessage {
            is_full: msg.request_full,
            ban_list_hash,
            ban_entries: if msg.request_full {
                ban_entries
            } else {
                Vec::new()
            },
            timestamp: now,
        };

        let response_msg = ProtocolMessage::BanList(response);
        let serialized = ProtocolParser::serialize_message(&response_msg)?;
        self.send_to_peer(peer_addr, serialized).await?;

        debug!(
            "Sent BanList response to {}: {} entries",
            peer_addr,
            if msg.request_full {
                ban_entries_count
            } else {
                0
            }
        );

        Ok(())
    }

    /// Handle BanList message - merge received ban list
    pub(crate) async fn handle_ban_list(
        &self,
        peer_addr: SocketAddr,
        msg: crate::network::protocol::BanListMessage,
    ) -> Result<()> {
        debug!(
            "BanList received from {}: full={}, {} entries",
            peer_addr,
            msg.is_full,
            msg.ban_entries.len()
        );

        if let Err(e) = crate::network::replay_protection::ReplayProtection::validate_timestamp(
            msg.timestamp as i64,
            86400,
        ) {
            warn!(
                "Replay protection: Invalid timestamp in BanList from {}: {}",
                peer_addr, e
            );
            return Err(anyhow::anyhow!("Replay protection: {}", e));
        }

        if msg.is_full && !verify_ban_list_hash(&msg.ban_entries, &msg.ban_list_hash) {
            warn!("Ban list hash verification failed from {}", peer_addr);
            return Ok(());
        }

        if !msg.is_full {
            debug!(
                "Received hash-only ban list from {}, skipping merge",
                peer_addr
            );
            return Ok(());
        }

        let mut ban_list = self.ban_list().write().await;
        let mut merged_count = 0;

        for entry in &msg.ban_entries {
            if !validate_ban_entry(entry) {
                continue;
            }

            let ip = if entry.addr.ip[0..12] == [0u8; 12] {
                let ipv4_bytes = &entry.addr.ip[12..16];
                IpAddr::V4(std::net::Ipv4Addr::new(
                    ipv4_bytes[0],
                    ipv4_bytes[1],
                    ipv4_bytes[2],
                    ipv4_bytes[3],
                ))
            } else {
                let mut ipv6_bytes = [0u8; 16];
                ipv6_bytes.copy_from_slice(&entry.addr.ip);
                IpAddr::V6(std::net::Ipv6Addr::from(ipv6_bytes))
            };

            let socket_addr = SocketAddr::new(ip, entry.addr.port);

            match ban_list.get(&socket_addr) {
                Some(&existing_timestamp) => {
                    if entry.unban_timestamp == u64::MAX {
                        ban_list.insert(socket_addr, u64::MAX);
                        merged_count += 1;
                    } else if existing_timestamp != u64::MAX
                        && entry.unban_timestamp > existing_timestamp
                    {
                        ban_list.insert(socket_addr, entry.unban_timestamp);
                        merged_count += 1;
                    }
                }
                None => {
                    ban_list.insert(socket_addr, entry.unban_timestamp);
                    merged_count += 1;
                }
            }
        }

        debug!("Merged {} ban entries from {}", merged_count, peer_addr);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::NetworkManager;
    use crate::network::ban_list_merging::calculate_ban_list_hash;
    use crate::network::protocol::{BanEntry, BanListMessage, NetworkAddress};
    use crate::utils::current_timestamp;
    use std::net::SocketAddr;

    fn legacy_ban_entry(port: u16) -> BanEntry {
        let mut ip = [0u8; 16];
        ip[12..16].copy_from_slice(&[203, 0, 113, 1]);
        BanEntry {
            addr: NetworkAddress {
                services: 0,
                ip,
                port,
            },
            unban_timestamp: current_timestamp() + 3600,
            reason: Some("test".into()),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn handle_ban_list_hash_only_skips_merge() {
        let listen: SocketAddr = "127.0.0.1:18420".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:18422".parse().unwrap();
        let nm = NetworkManager::new(listen);
        nm.handle_ban_list(
            peer,
            BanListMessage {
                is_full: false,
                ban_list_hash: [0u8; 32],
                ban_entries: vec![],
                timestamp: current_timestamp(),
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn handle_ban_list_merges_valid_entry() {
        let listen: SocketAddr = "127.0.0.1:18423".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:18425".parse().unwrap();
        let nm = NetworkManager::new(listen);
        let entries = vec![legacy_ban_entry(8333)];
        let hash = calculate_ban_list_hash(&entries);
        nm.handle_ban_list(
            peer,
            BanListMessage {
                is_full: true,
                ban_list_hash: hash,
                ban_entries: entries,
                timestamp: current_timestamp(),
            },
        )
        .await
        .unwrap();
        assert!(
            nm.ban_list()
                .read()
                .await
                .contains_key(&"203.0.113.1:8333".parse::<SocketAddr>().unwrap())
        );
    }
}
