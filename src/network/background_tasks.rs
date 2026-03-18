//! Background task starters for NetworkManager.
//!
//! Periodic tasks: request cleanup, DoS protection, ban cleanup, chain sync timeout,
//! peer eviction, ping, ping timeout, peer reconnection.

use crate::network::network_manager::NetworkManager;
use crate::network::transport::{Transport, TransportAddr};
use crate::network::NetworkMessage;
use crate::utils::{
    current_timestamp, retry_async_with_backoff, RetryConfig, BACKGROUND_TASK_BACKOFF_SLEEP,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

impl NetworkManager {
    /// Start periodic task to clean up expired pending requests
    pub(crate) fn start_request_cleanup_task(&self) {
        let pending_requests = Arc::clone(self.pending_requests());
        let timeout_config = Arc::clone(self.request_timeout_config());

        tokio::spawn(async move {
            let cleanup_interval = timeout_config.request_cleanup_interval_seconds;
            let max_age = timeout_config.pending_request_max_age_seconds;
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(cleanup_interval));
            loop {
                interval.tick().await;

                let now = current_timestamp();

                let mut pending = pending_requests.lock().await;
                let initial_count = pending.len();
                pending.retain(|_, req| now.saturating_sub(req.timestamp()) < max_age);
                let removed = initial_count - pending.len();
                if removed > 0 {
                    debug!(
                        "Cleaned up {} stale pending requests (older than {}s)",
                        removed, max_age
                    );
                }
                if !pending.is_empty() {
                    debug!("Pending requests: {}", pending.len());
                }
            }
        });
    }

    /// Start periodic task to clean up DoS protection data
    pub(crate) fn start_dos_protection_cleanup_task(&self) {
        let dos_protection = Arc::clone(self.dos_protection());
        let ban_list = Arc::clone(self.ban_list());
        let bg = self.background_task_config();
        let outer_secs = bg.dos_cleanup_interval_secs;
        let inner_secs = bg.ban_cleanup_interval_secs;

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(outer_secs));
            loop {
                interval.tick().await;

                dos_protection.cleanup().await;

                let dos_clone = Arc::clone(&dos_protection);
                let ban_list_clone = Arc::clone(&ban_list);
                let ban_duration = dos_protection.ban_duration_seconds();
                tokio::spawn(async move {
                    let mut interval =
                        tokio::time::interval(tokio::time::Duration::from_secs(inner_secs));
                    loop {
                        interval.tick().await;

                        let ips_to_ban = dos_clone.get_ips_to_auto_ban().await;

                        if !ips_to_ban.is_empty() {
                            let now = current_timestamp();
                            let unban_timestamp = now + ban_duration;

                            let mut ban_list_guard = ban_list_clone.write().await;
                            for ip in ips_to_ban {
                                let socket_addr = std::net::SocketAddr::new(ip, 0);
                                if let std::collections::hash_map::Entry::Vacant(e) =
                                    ban_list_guard.entry(socket_addr)
                                {
                                    e.insert(unban_timestamp);
                                    warn!(
                                        "Auto-banned IP {} for connection rate violations (unban at {})",
                                        ip, unban_timestamp
                                    );
                                }
                            }
                        }
                    }
                });
            }
        });
    }

    /// Start periodic task to clean up expired bans
    pub(crate) fn start_ban_cleanup_task(&self) {
        let ban_list = Arc::clone(self.ban_list());
        let secs = self.background_task_config().ban_cleanup_outer_interval_secs;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(secs));
            loop {
                interval.tick().await;

                let now = current_timestamp();

                let mut ban_list_guard = ban_list.write().await;
                let expired: Vec<std::net::SocketAddr> = ban_list_guard
                    .iter()
                    .filter(|(_, &unban_timestamp)| {
                        unban_timestamp != u64::MAX && now >= unban_timestamp
                    })
                    .map(|(addr, _)| *addr)
                    .collect();

                let expired_count = expired.len();
                for addr in &expired {
                    ban_list_guard.remove(addr);
                    debug!("Cleaned up expired ban for {}", addr);
                }

                if expired_count > 0 {
                    info!("Cleaned up {} expired ban(s)", expired_count);
                }
            }
        });
    }

    /// Start chain sync timeout checking task
    pub(crate) fn start_chain_sync_timeout_check_task(&self) {
        let peer_manager = Arc::clone(self.peer_manager_mutex());
        let peer_tx = self.peer_tx().clone();
        let storage = self.storage().clone();
        let bg = self.background_task_config();
        let interval_secs = bg.chain_sync_check_interval_secs;
        let timeout_secs = bg.chain_sync_timeout_secs;

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));

            loop {
                interval.tick().await;

                let now = current_timestamp();
                let chain_sync_timeout = timeout_secs;

                let our_chainwork = {
                    if let Some(storage) = &storage {
                        if let Ok(Some(tip_hash)) = storage.chain().get_tip_hash() {
                            if let Ok(Some(chainwork)) = storage.chain().get_chainwork(&tip_hash) {
                                chainwork
                            } else {
                                0
                            }
                        } else {
                            0
                        }
                    } else {
                        0
                    }
                };

                let mut pm = peer_manager.lock().await;
                let mut peers_to_disconnect: Vec<TransportAddr> = Vec::new();

                for (addr, peer) in pm.peers().iter() {
                    let is_outbound = peer.is_outbound();

                    if is_outbound {
                        let connection_age = now.saturating_sub(peer.conntime());

                        if connection_age > chain_sync_timeout {
                            if let Some(peer_chainwork) = peer.chainwork() {
                                if peer_chainwork < our_chainwork {
                                    warn!(
                                        "Outbound peer {:?} has insufficient chainwork after {} minutes (peer: {}, ours: {}), disconnecting",
                                        addr, connection_age / 60, peer_chainwork, our_chainwork
                                    );
                                    peers_to_disconnect.push(addr.clone());
                                }
                            } else {
                                warn!(
                                    "Outbound peer {:?} has no chainwork after {} minutes, disconnecting",
                                    addr, connection_age / 60
                                );
                                peers_to_disconnect.push(addr.clone());
                            }
                        }
                    }
                }

                drop(pm);

                for addr in peers_to_disconnect {
                    let _ = peer_tx.send(NetworkMessage::PeerDisconnected(addr));
                }
            }
        });
    }

    /// Start outbound peer eviction task
    pub(crate) fn start_outbound_peer_eviction_task(&self) {
        let peer_manager = Arc::clone(self.peer_manager_mutex());
        let peer_tx = self.peer_tx().clone();
        let secs = self.background_task_config().peer_eviction_interval_secs;

        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(secs));
            use crate::network::peer_manager::MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT;

            loop {
                interval.tick().await;

                let mut pm = peer_manager.lock().await;

                let mut outbound_peers: Vec<(TransportAddr, u64)> = pm
                    .peers()
                    .iter()
                    .filter(|(_, peer)| peer.is_outbound() && !peer.is_manual())
                    .map(|(addr, peer)| {
                        let last_announce = peer.last_block_announcement().unwrap_or(0);
                        (addr.clone(), last_announce)
                    })
                    .collect();

                if outbound_peers.len() <= MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT {
                    continue;
                }

                outbound_peers.sort_by_key(|(_, time)| *time);

                let peers_to_evict =
                    outbound_peers.len() - MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT;
                let peers_to_disconnect: Vec<_> = outbound_peers
                    .iter()
                    .take(peers_to_evict)
                    .map(|(addr, _)| addr.clone())
                    .collect();

                drop(pm);

                for addr in peers_to_disconnect {
                    let now = current_timestamp();
                    let pm_check = peer_manager.lock().await;
                    let last_announce = pm_check
                        .get_peer(&addr)
                        .and_then(|p| p.last_block_announcement())
                        .unwrap_or(0);
                    drop(pm_check);

                    warn!(
                        "Evicting extra outbound peer {:?} (last block announcement: {} seconds ago)",
                        addr,
                        if last_announce > 0 {
                            now.saturating_sub(last_announce)
                        } else {
                            0
                        }
                    );
                    let _ = peer_tx.send(NetworkMessage::PeerDisconnected(addr));
                }
            }
        });
    }

    /// Start ping timeout checking task
    pub(crate) fn start_ping_timeout_check_task(&self) {
        let peer_manager = Arc::clone(self.peer_manager_mutex());
        let peer_tx = self.peer_tx().clone();
        let secs = self.background_task_config().ping_timeout_check_interval_secs;

        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(secs));

            loop {
                interval.tick().await;

                let mut pm = peer_manager.lock().await;
                let mut peers_to_disconnect: Vec<TransportAddr> = Vec::new();

                for (addr, peer) in pm.peers().iter() {
                    if peer.is_ping_timed_out() {
                        warn!("Ping timeout for peer {:?}, disconnecting", addr);
                        peers_to_disconnect.push(addr.clone());
                    }
                }

                drop(pm);

                for addr in peers_to_disconnect {
                    let _ = peer_tx.send(NetworkMessage::PeerDisconnected(addr));
                }
            }
        });
    }

    /// Start periodic ping task
    pub(crate) fn start_ping_task(&self) {
        let peer_manager = Arc::clone(self.peer_manager_mutex());
        let secs = self.background_task_config().ping_interval_secs;

        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(secs));

            loop {
                interval.tick().await;

                let nonce = crate::utils::current_timestamp_nanos();

                use crate::network::protocol::{PingMessage, ProtocolMessage, ProtocolParser};
                let ping_msg = ProtocolMessage::Ping(PingMessage { nonce });
                let wire_msg = match ProtocolParser::serialize_message(&ping_msg) {
                    Ok(msg) => msg,
                    Err(e) => {
                        warn!("Failed to serialize ping message: {}", e);
                        continue;
                    }
                };

                {
                    let mut pm = peer_manager.lock().await;
                    for (addr, peer) in pm.peers_mut().iter_mut() {
                        peer.record_ping_sent(nonce);

                        if let Err(e) = peer.send_tx.send(wire_msg.clone()) {
                            warn!("Failed to send ping to peer {:?}: {}", addr, e);
                        }
                    }
                }
            }
        });
    }

    /// Start periodic task to attempt peer reconnections with exponential backoff
    pub(crate) fn start_peer_reconnection_task(&self) {
        let reconnection_queue = Arc::clone(self.peer_reconnection_queue());
        let peer_manager = Arc::clone(self.peer_manager_mutex());
        let peer_tx = self.peer_tx().clone();
        let tcp_transport = self.tcp_transport().clone();
        let ban_list = Arc::clone(self.ban_list());
        let secs = self.background_task_config().peer_reconnection_interval_secs;

        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(secs));

            loop {
                interval.tick().await;

                let now = current_timestamp();

                let mut queue = reconnection_queue.lock().await;

                let (current_peers, max_peers) = {
                    let pm = peer_manager.lock().await;
                    (pm.peer_count(), pm.max_peers())
                };

                let min_peers = std::cmp::max(1, max_peers / 2);

                if current_peers >= min_peers {
                    queue
                        .retain(|_, (_, last_attempt, _)| {
                            now.saturating_sub(*last_attempt) < 3600
                        });
                    continue;
                }

                let now = current_timestamp();
                let mut peers_to_reconnect: Vec<(std::net::SocketAddr, u32, f64, u64)> = queue
                    .iter()
                    .map(|(addr, (attempts, last_attempt, quality))| {
                        (*addr, *attempts, *quality, *last_attempt)
                    })
                    .collect();

                peers_to_reconnect.sort_by(|a, b| {
                    b.2.partial_cmp(&a.2)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.1.cmp(&b.1))
                        .then_with(|| a.3.cmp(&b.3))
                });

                let peers_to_reconnect: Vec<(std::net::SocketAddr, u32, f64)> = peers_to_reconnect
                    .into_iter()
                    .map(|(addr, attempts, quality, _)| (addr, attempts, quality))
                    .collect();

                for (addr, attempts, quality) in peers_to_reconnect.iter() {
                    {
                        let ban_list_guard = ban_list.read().await;
                        if let Some(unban_timestamp) = ban_list_guard.get(addr) {
                            if *unban_timestamp != u64::MAX && now < *unban_timestamp {
                                continue;
                            }
                        }
                    }

                    let backoff_seconds = std::cmp::min(1u64 << attempts, 60);
                    let last_attempt = queue.get(addr).map(|(_, la, _)| *la).unwrap_or(0);

                    if now.saturating_sub(last_attempt) < backoff_seconds {
                        continue;
                    }

                    if *attempts >= 10 {
                        debug!(
                            "Removing peer {} from reconnection queue (max attempts reached)",
                            addr
                        );
                        queue.remove(addr);
                        continue;
                    }

                    if current_peers >= max_peers {
                        break;
                    }

                    info!(
                        "Attempting to reconnect to peer {} (attempt {}, quality: {:.2})",
                        addr,
                        attempts + 1,
                        quality
                    );

                    if let Some((ref mut attempts_ref, ref mut last_attempt_ref, _)) =
                        queue.get_mut(addr)
                    {
                        *attempts_ref += 1;
                        *last_attempt_ref = now;
                    }

                    let addr_clone = *addr;
                    let peer_tx_clone = peer_tx.clone();
                    let peer_manager_clone = Arc::clone(&peer_manager);
                    let tcp_transport_clone = tcp_transport.clone();
                    let reconnection_queue_clone = Arc::clone(&reconnection_queue);

                    tokio::spawn(async move {
                        use crate::network::peer::Peer;

                        let connect_result = retry_async_with_backoff(
                            &RetryConfig::network(),
                            || tcp_transport_clone.connect(TransportAddr::Tcp(addr_clone)),
                        )
                        .await;

                        match connect_result {
                            Ok(conn) => {
                                info!("Successfully reconnected to peer {}", addr_clone);

                                let peer = Peer::from_transport_connection(
                                    conn,
                                    addr_clone,
                                    TransportAddr::Tcp(addr_clone),
                                    peer_tx_clone.clone(),
                                );

                                let mut pm = peer_manager_clone.lock().await;
                                if let Err(e) =
                                    pm.add_peer(TransportAddr::Tcp(addr_clone), peer)
                                {
                                    warn!("Failed to add reconnected peer {}: {}", addr_clone, e);
                                    let _ = peer_tx_clone.send(NetworkMessage::PeerDisconnected(
                                        TransportAddr::Tcp(addr_clone),
                                    ));
                                } else {
                                    let mut queue = reconnection_queue_clone.lock().await;
                                    queue.remove(&addr_clone);
                                    info!("Peer {} successfully reconnected and added", addr_clone);
                                }
                            }
                            Err(e) => {
                                debug!(
                                    "Reconnection attempt to {} failed: {} (will retry with backoff)",
                                    addr_clone, e
                                );
                            }
                        }
                    });

                    if !queue.is_empty() {
                        tokio::time::sleep(BACKGROUND_TASK_BACKOFF_SLEEP).await;
                    }
                }
            }
        });
    }
}
