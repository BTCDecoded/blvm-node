//! Background task starters for NetworkManager.
//!
//! Periodic tasks: request cleanup, DoS protection, ban cleanup, chain sync timeout,
//! peer eviction, ping, ping timeout, peer reconnection.

use crate::network::NetworkMessage;
use crate::network::network_manager::NetworkManager;
use crate::network::peer::Peer;
use crate::network::transport::TransportAddr;
use crate::utils::{BACKGROUND_TASK_BACKOFF_SLEEP, current_timestamp};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// Record each connection-rate offender in the ban list once.
async fn ban_connection_rate_offenders(
    dos_protection: &Arc<crate::network::dos_protection::DosProtectionManager>,
    ban_list: &Arc<tokio::sync::RwLock<std::collections::HashMap<SocketAddr, u64>>>,
) {
    let ips_to_ban = dos_protection.get_ips_to_auto_ban().await;
    if ips_to_ban.is_empty() {
        return;
    }
    let now = current_timestamp();
    let unban_timestamp = now + dos_protection.ban_duration_seconds();
    let mut ban_list_guard = ban_list.write().await;
    for ip in ips_to_ban {
        let socket_addr = SocketAddr::new(ip, 0);
        if let std::collections::hash_map::Entry::Vacant(e) = ban_list_guard.entry(socket_addr) {
            e.insert(unban_timestamp);
            warn!(
                "Auto-banned IP {} for connection rate violations (unban at {})",
                ip, unban_timestamp
            );
        }
    }
}

/// Drop an outbound peer for low chainwork only while a parallel download is running
/// and the peer's chainwork has been recorded as strictly less than ours.
///
/// A missing chainwork value stays connected. Once the download session is finished,
/// this check leaves outbound peers connected.
pub(crate) fn outbound_chain_sync_disconnect(
    peer: &Peer,
    now: u64,
    timeout_secs: u64,
    our_chainwork: u128,
    still_downloading: bool,
) -> bool {
    if !peer.is_outbound() || !still_downloading {
        return false;
    }
    if now.saturating_sub(peer.conntime()) <= timeout_secs {
        return false;
    }
    match peer.chainwork() {
        Some(peer_chainwork) => peer_chainwork < our_chainwork,
        None => false,
    }
}

/// Peers with no block announcement are dropped first. `protect` peers stay.
/// Indices are into `last_announcements`.
pub(crate) fn select_outbound_evictions(
    last_announcements: &[Option<u64>],
    protect: usize,
) -> Vec<usize> {
    if last_announcements.len() <= protect {
        return Vec::new();
    }
    let mut order: Vec<usize> = (0..last_announcements.len()).collect();
    order.sort_by_key(|&index| last_announcements[index].unwrap_or(0));
    let evict = last_announcements.len() - protect;
    order.into_iter().take(evict).collect()
}

impl NetworkManager {
    /// Start periodic task to clean up expired pending requests
    pub(crate) fn start_request_cleanup_task(&self) {
        let pending_requests = Arc::clone(self.pending_requests());
        let inventory = Arc::clone(self.inventory());
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
                drop(pending);
                let mut inv = inventory.lock().unwrap_or_else(|e| e.into_inner());
                inv.cleanup_old_requests(max_age);
            }
        });
    }

    /// Start periodic task to clean up DoS protection data
    pub(crate) fn start_dos_protection_cleanup_task(&self) {
        let dos_protection = Arc::clone(self.dos_protection());
        let bandwidth_protection = Arc::clone(self.bandwidth_protection());
        let ban_list = Arc::clone(self.ban_list());
        let bg = self.background_task_config();
        let outer_secs = bg.dos_cleanup_interval_secs;

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(outer_secs));
            loop {
                interval.tick().await;

                dos_protection.cleanup().await;

                // Evict stale bandwidth tracker entries (idle > 25h).
                bandwidth_protection.evict_stale_entries(90_000).await;

                ban_connection_rate_offenders(&dos_protection, &ban_list).await;
            }
        });
    }

    /// Ban connection-rate offenders once. The cleanup task calls this on each outer tick.
    pub(crate) async fn apply_auto_ban_tick(&self) {
        ban_connection_rate_offenders(self.dos_protection(), self.ban_list()).await;
    }

    /// Start periodic task to clean up expired bans
    pub(crate) fn start_ban_cleanup_task(&self) {
        let ban_list = Arc::clone(self.ban_list());
        let secs = self
            .background_task_config()
            .ban_cleanup_outer_interval_secs;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(secs));
            loop {
                interval.tick().await;

                let now = current_timestamp();

                let mut ban_list_guard = ban_list.write().await;
                let expired: Vec<std::net::SocketAddr> = ban_list_guard
                    .iter()
                    .filter(|&(_, unban_timestamp)| {
                        *unban_timestamp != u64::MAX && now >= *unban_timestamp
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

    /// Push the ban list on `share_mode = periodic`. Immediate mode sends from `ban_peer`.
    pub(crate) fn start_ban_list_share_task(&self) {
        let Some(cfg) = self.ban_list_sharing_config.clone() else {
            return;
        };
        if !cfg.shares() || !matches!(cfg.share_mode, crate::config::BanShareMode::Periodic) {
            return;
        }
        let interval = cfg.periodic_interval_seconds.max(1);
        let min_duration = cfg.min_ban_duration_to_share;
        let ban_list = Arc::clone(self.ban_list());
        let peer_manager = Arc::clone(self.peer_manager_mutex());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval));
            loop {
                tick.tick().await;
                super::handlers::ban_list::broadcast_ban_list(
                    ban_list.as_ref(),
                    peer_manager.as_ref(),
                    min_duration,
                )
                .await;
            }
        });
    }

    /// Start chain sync timeout checking task
    pub(crate) fn start_chain_sync_timeout_check_task(&self) {
        let peer_manager = Arc::clone(self.peer_manager_mutex());
        let peer_tx = self.peer_tx().clone();
        let reconnection_queue = Arc::clone(self.peer_reconnection_queue());
        let storage = self.storage().clone();
        let bg = self.background_task_config();
        let interval_secs = bg.chain_sync_check_interval_secs;
        let timeout_secs = bg.chain_sync_timeout_secs;

        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));

            loop {
                interval.tick().await;

                let now = current_timestamp();
                let chain_sync_timeout = timeout_secs;

                let our_chainwork = {
                    if let Some(storage) = &storage {
                        if let Ok(Some(tip_hash)) = storage.chain().get_tip_hash() {
                            if let Ok(Some(chainwork)) = storage.chain().get_chainwork(&tip_hash) {
                                chainwork.low_u128()
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

                let still_downloading = crate::node::parallel_ibd::PARALLEL_IBD_SESSION_ACTIVE
                    .load(std::sync::atomic::Ordering::Acquire);

                let mut pm = peer_manager.lock().await;
                let mut peers_to_disconnect: Vec<TransportAddr> = Vec::new();

                for (addr, peer) in pm.peers().iter() {
                    if outbound_chain_sync_disconnect(
                        peer,
                        now,
                        chain_sync_timeout,
                        our_chainwork,
                        still_downloading,
                    ) {
                        warn!(
                            "Outbound peer {:?} has insufficient chainwork after {} minutes (peer: {}, ours: {}), disconnecting",
                            addr,
                            now.saturating_sub(peer.conntime()) / 60,
                            peer.chainwork().unwrap_or(0),
                            our_chainwork
                        );
                        peers_to_disconnect.push(addr.clone());
                    }
                }

                drop(pm);

                for addr in peers_to_disconnect {
                    if let TransportAddr::Tcp(sock) = &addr {
                        let mut q = reconnection_queue.lock().await;
                        q.entry(*sock).or_insert((0, 0, 1.0));
                    }
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
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(secs));
            use crate::network::peer_manager::MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT;

            loop {
                interval.tick().await;

                let mut pm = peer_manager.lock().await;

                let mut outbound_peers: Vec<(TransportAddr, Option<u64>)> = pm
                    .peers()
                    .iter()
                    .filter(|(_, peer)| peer.is_outbound() && !peer.is_manual())
                    .map(|(addr, peer)| (addr.clone(), peer.last_block_announcement()))
                    .collect();

                let victims = select_outbound_evictions(
                    &outbound_peers
                        .iter()
                        .map(|(_, announced)| *announced)
                        .collect::<Vec<_>>(),
                    MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT,
                );
                let peers_to_disconnect: Vec<_> = victims
                    .into_iter()
                    .filter_map(|index| outbound_peers.get(index).map(|(addr, _)| addr.clone()))
                    .collect();
                outbound_peers.clear();

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
        let secs = self
            .background_task_config()
            .ping_timeout_check_interval_secs;

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(secs));

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
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(secs));

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
        let persistent_peers = Arc::clone(self.persistent_peers_lock());
        let ibd_evicted_ips = Arc::clone(&self.ibd_evicted_ips);
        let secs = self
            .background_task_config()
            .peer_reconnection_interval_secs;
        let stale_secs = self.background_task_config().peer_reconnection_stale_secs;
        let ping_timeout_secs = self.background_task_config().ping_timeout_secs;
        let connect_timeout = self.request_timeout_config().connect_timeout_secs;
        let max_msg_len = self.protocol_limits().max_protocol_message_length;

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(secs));

            loop {
                interval.tick().await;

                let now = current_timestamp();
                let persistent_set = persistent_peers.lock().await.clone();

                let (current_peers, max_peers) = {
                    let pm = peer_manager.lock().await;
                    (pm.peer_count(), pm.max_peers())
                };

                let min_peers = std::cmp::max(1, max_peers / 2);
                let try_persistent_only = current_peers >= min_peers;

                let disconnected_persistent: std::collections::HashSet<std::net::SocketAddr> = {
                    let pm = peer_manager.lock().await;
                    persistent_set
                        .iter()
                        .copied()
                        .filter(|a| {
                            pm.get_peer(&TransportAddr::Tcp(*a))
                                .map(|p| p.is_connected())
                                != Some(true)
                        })
                        .collect()
                };

                let mut queue = reconnection_queue.lock().await;

                if try_persistent_only {
                    queue.retain(|addr, (_, last_attempt, _)| {
                        now.saturating_sub(*last_attempt) < stale_secs
                            || persistent_set.contains(addr)
                    });
                }

                let now = current_timestamp();
                // When outbound count is "high", still process: (1) disconnected persistent peers,
                // (2) any other queued TCP peer (e.g. IBD added the addr after GetData "not found").
                let mut peers_to_reconnect: Vec<(std::net::SocketAddr, u32, f64, u64)> = queue
                    .iter()
                    .filter(|(addr, _)| {
                        !try_persistent_only
                            || disconnected_persistent.contains(addr)
                            || !persistent_set.contains(addr)
                    })
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
                    // Skip evicted IBD peers while IBD is active (NODE_NETWORK_LIMITED / pruned).
                    // Do NOT remove them from the queue: the eviction set is cleared when IBD
                    // restarts, so removing here would prevent reconnection after a failed IBD run.
                    let is_evicted = {
                        let evicted = ibd_evicted_ips.read().unwrap();
                        evicted.contains(&addr.ip())
                    };
                    if is_evicted {
                        continue;
                    }
                    if crate::network::peer_connections::dial_is_cooled(*addr) {
                        continue;
                    }
                    {
                        let ban_list_guard = ban_list.read().await;
                        if let Some(unban_timestamp) =
                            ban_list_guard.get(&std::net::SocketAddr::new(addr.ip(), 0))
                        {
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

                    if *attempts >= 10 && !persistent_set.contains(addr) {
                        debug!(
                            "Removing peer {} from reconnection queue (max attempts reached)",
                            addr
                        );
                        queue.remove(addr);
                        continue;
                    }
                    if *attempts >= 10 && persistent_set.contains(addr) {
                        if let Some((att, _, _)) = queue.get_mut(addr) {
                            *att = 0;
                        }
                    }

                    // At capacity, still try persistent peers (IBD / operator-configured) — same slot
                    // may have been freed; `add_peer` will fail if we are truly full.
                    if current_peers >= max_peers && !persistent_set.contains(addr) {
                        break;
                    }

                    info!(
                        "Attempting to reconnect to peer {} (attempt {}, quality: {:.2})",
                        addr,
                        attempts + 1,
                        quality
                    );

                    if let Some((attempts_ref, last_attempt_ref, _)) = queue.get_mut(addr) {
                        *attempts_ref += 1;
                        *last_attempt_ref = now;
                    }

                    let addr_clone = *addr;
                    let peer_tx_clone = peer_tx.clone();
                    let peer_manager_clone = Arc::clone(&peer_manager);
                    let tcp_transport_clone = tcp_transport.clone();
                    let reconnection_queue_clone = Arc::clone(&reconnection_queue);
                    let ct = connect_timeout;
                    let mml = max_msg_len;
                    let ping_timeout = ping_timeout_secs;

                    tokio::spawn(async move {
                        use crate::network::peer::Peer;

                        let connect_result = tcp_transport_clone
                            .connect_stream_with_timeout(addr_clone, ct)
                            .await;

                        match connect_result {
                            Ok(stream) => {
                                info!("Successfully reconnected to peer {}", addr_clone);

                                let mut peer = Peer::from_tcp_stream_split(
                                    stream,
                                    addr_clone,
                                    peer_tx_clone.clone(),
                                    mml,
                                );
                                peer.set_is_outbound(true);
                                peer.set_ping_timeout_seconds(ping_timeout);

                                let mut pm = peer_manager_clone.lock().await;
                                if let Err(e) = pm.add_peer(TransportAddr::Tcp(addr_clone), peer) {
                                    warn!("Failed to add reconnected peer {}: {}", addr_clone, e);
                                    let _ = peer_tx_clone.send(NetworkMessage::PeerDisconnected(
                                        TransportAddr::Tcp(addr_clone),
                                    ));
                                } else {
                                    let _ = peer_tx_clone.send(NetworkMessage::PeerConnected(
                                        TransportAddr::Tcp(addr_clone),
                                    ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[tokio::test(flavor = "multi_thread")]
    async fn dos_protection_cleanup_smoke() {
        let listen: SocketAddr = "127.0.0.1:18460".parse().unwrap();
        let nm = NetworkManager::new(listen);
        nm.dos_protection().cleanup().await;
        nm.bandwidth_protection().evict_stale_entries(90_000).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn background_task_starters_smoke() {
        let listen: SocketAddr = "127.0.0.1:18461".parse().unwrap();
        let nm = NetworkManager::new(listen);
        nm.start_ban_cleanup_task();
        nm.start_request_cleanup_task();
        nm.start_chain_sync_timeout_check_task();
        nm.start_ping_task();
        nm.start_outbound_peer_eviction_task();
        nm.start_dos_protection_cleanup_task();
        nm.start_ping_timeout_check_task();
        nm.start_peer_reconnection_task();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    async fn aged_outbound_peer(age_secs: u64) -> Peer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let mut peer = Peer::from_tcp_stream_split(
            server,
            addr,
            tx,
            crate::network::protocol::MAX_PROTOCOL_MESSAGE_LENGTH,
        );
        peer.set_is_outbound(true);
        peer.set_conntime(current_timestamp().saturating_sub(age_secs));
        peer
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chain_sync_lock_keeps_peer_without_chainwork() {
        let peer = aged_outbound_peer(1201).await;
        let now = current_timestamp();
        assert!(
            !outbound_chain_sync_disconnect(&peer, now, 1200, 100, true),
            "missing chainwork stays connected"
        );
        assert!(
            !outbound_chain_sync_disconnect(&peer, now, 1200, 100, false),
            "a current tip stays connected"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chain_sync_lock_drops_lower_chainwork_only_while_downloading() {
        let mut peer = aged_outbound_peer(1201).await;
        peer.set_chainwork(50);
        let now = current_timestamp();
        assert!(
            outbound_chain_sync_disconnect(&peer, now, 1200, 100, true),
            "known lower chainwork disconnects during download"
        );
        assert!(
            !outbound_chain_sync_disconnect(&peer, now, 1200, 100, false),
            "known lower chainwork stays once download is finished"
        );
        assert!(
            !outbound_chain_sync_disconnect(&peer, now, 1200, 50, true),
            "equal chainwork stays connected"
        );
        peer.set_is_outbound(false);
        assert!(!outbound_chain_sync_disconnect(&peer, now, 1200, 100, true));

        let mut young = aged_outbound_peer(1200).await;
        young.set_chainwork(1);
        assert!(
            !outbound_chain_sync_disconnect(&young, now, 1200, 100, true),
            "age equal to the timeout stays connected"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chain_sync_lock_empty_peer_set_starts_a_connection() {
        let listen: SocketAddr = "127.0.0.1:18462".parse().unwrap();
        let nm = NetworkManager::new(listen);
        let ip = std::net::Ipv4Addr::new(203, 0, 113, 5);
        let mut octets = [0u8; 16];
        octets[10] = 0xff;
        octets[11] = 0xff;
        octets[12] = 203;
        octets[13] = 0;
        octets[14] = 113;
        octets[15] = 5;
        nm.address_database().write().await.add_address(
            crate::network::protocol::NetworkAddress {
                time: current_timestamp() as u32,
                services: blvm_protocol::service_flags::standard::NODE_NETWORK,
                ip: octets,
                port: 8333,
            },
            blvm_protocol::service_flags::standard::NODE_NETWORK,
        );
        let connected = nm.connect_outbound_when_no_peers().await.unwrap();
        assert_eq!(connected, 0);
        assert!(
            nm.dos_protection()
                .get_connection_attempts(std::net::IpAddr::V4(ip))
                .await
                >= 1,
            "empty peer set dials an address from the database"
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local = listener.local_addr().unwrap();
        let accept = tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        nm.connect_to_peer(local).await.unwrap();
        assert_eq!(nm.peer_count(), 1);
        assert_eq!(nm.connect_outbound_when_no_peers().await.unwrap(), 0);
        accept.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ping_lock_unanswered_ping_times_out_after_later_pings() {
        let mut peer = aged_outbound_peer(0).await;
        peer.record_ping_sent(1);
        assert!(!peer.is_ping_timed_out());
        peer.set_ping_sent_time(current_timestamp().saturating_sub(1201));
        peer.record_ping_sent(2);
        assert_eq!(peer.pending_ping_nonce(), Some(2));
        assert!(
            peer.is_ping_timed_out(),
            "the original unanswered ping still times out after a later ping"
        );
        assert!(peer.record_pong_received(2));
        assert!(!peer.is_ping_timed_out());
        peer.record_ping_sent(3);
        assert!(!peer.is_ping_timed_out());
    }

    #[test]
    fn eviction_lock_drops_a_silent_peer_and_keeps_the_announcer() {
        let announcements = [None, None, Some(50), None, None];
        let victims = select_outbound_evictions(&announcements, 4);
        assert_eq!(victims.len(), 1);
        assert_ne!(victims[0], 2, "the peer that announced a block stays");
        assert!(announcements[victims[0]].is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_ban_tick_records_each_address_once() {
        use crate::config::{DosProtectionConfig, NodeConfig};
        use crate::network::transport::TransportPreference;
        use std::net::IpAddr;

        let mut dos = DosProtectionConfig::default();
        dos.max_connections_per_window = 0;
        dos.auto_ban_threshold = 1;
        let mut config = NodeConfig::default();
        config.dos_protection = Some(dos);
        let listen: SocketAddr = "127.0.0.1:18462".parse().unwrap();
        let nm =
            NetworkManager::with_config(listen, 8, TransportPreference::TCP_ONLY, Some(&config));
        let ip: IpAddr = "203.0.113.9".parse().unwrap();
        assert!(!nm.dos_protection().check_connection(ip).await);
        nm.apply_auto_ban_tick().await;
        nm.apply_auto_ban_tick().await;
        let bans = nm.ban_list().read().await;
        assert_eq!(bans.len(), 1);
        assert!(bans.contains_key(&SocketAddr::new(ip, 0)));
    }
}
