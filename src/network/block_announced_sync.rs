//! Post-IBD block sync: act on peer `inv` announcements and refresh stale peer tips.
//!
//! After parallel IBD completes, new blocks are announced via `inv` (MSG_BLOCK). Previously the
//! node only recorded inventory and never sent `getdata`, so the main loop never received blocks.
//! Catch-up IBD also keyed off version `start_height`, which does not advance on an open connection
//! when the chain tip moves.

use crate::network::inventory::{MSG_BLOCK, MSG_WITNESS_BLOCK};
use crate::network::network_manager::NetworkManager;
use crate::network::protocol::{
    GetDataMessage, GetHeadersMessage, HeadersMessage, InventoryVector, ProtocolMessage,
    ProtocolParser, SendCmpctMessage,
};
use crate::network::transport::{TransportAddr, TransportType};
use anyhow::Result;
use blvm_protocol::Hash;
use std::net::SocketAddr;
use tokio::time::{Duration, timeout};
use tracing::{debug, info, warn};

/// How many block hashes one `inv` may turn into `getdata`.
pub(crate) const MAX_BLOCK_REQUESTS_PER_INV: usize = 16;

/// At most this many peers are asked for high-bandwidth compact announcements.
pub(crate) const MAX_HIGH_BANDWIDTH_PEERS: usize = 3;

/// A headers announcement is sent only when the peer's known header is this close to the tip.
pub(crate) const MAX_HEADER_ANNOUNCE_DISTANCE: u64 = 8;

/// Length of the headers batch for a peer, or none when the gap is outside 1..=8.
pub(crate) fn header_announce_len(tip_height: u64, known_height: Option<u64>) -> Option<u64> {
    let known = known_height?;
    let distance = tip_height.checked_sub(known)?;
    if (1..=MAX_HEADER_ANNOUNCE_DISTANCE).contains(&distance) {
        Some(distance)
    } else {
        None
    }
}

/// Handshake-complete peers that are not already in the high-bandwidth set, up to the open slots.
pub(crate) fn high_bandwidth_candidates(
    states: &std::collections::HashMap<SocketAddr, blvm_protocol::network::PeerState>,
) -> Vec<SocketAddr> {
    let selected = states.values().filter(|state| state.high_bandwidth).count();
    let room = MAX_HIGH_BANDWIDTH_PEERS.saturating_sub(selected);
    if room == 0 {
        return Vec::new();
    }
    let mut open: Vec<SocketAddr> = states
        .iter()
        .filter(|(_, state)| state.handshake_complete && !state.high_bandwidth)
        .map(|(addr, _)| *addr)
        .collect();
    open.sort();
    open.truncate(room);
    open
}

/// Record a header height the peer has. A lower height does not rewind the mark.
pub(crate) fn advance_known_header_height(
    state: &mut blvm_protocol::network::PeerState,
    height: u64,
) {
    let next = state
        .known_header_height
        .map_or(height, |known| known.max(height));
    state.known_header_height = Some(next);
}

/// Clear a peer's high-bandwidth slot. Returns whether that peer had the slot.
pub(crate) fn revoke_high_bandwidth(state: &mut blvm_protocol::network::PeerState) -> bool {
    if !state.high_bandwidth {
        return false;
    }
    state.high_bandwidth = false;
    true
}

fn headers_for_announce(
    storage: &crate::storage::Storage,
    tip_height: u64,
    distance: u64,
    tip_hash: &Hash,
    tip_header: &blvm_protocol::BlockHeader,
) -> Option<Vec<blvm_protocol::BlockHeader>> {
    if distance == 1 {
        return Some(vec![tip_header.clone()]);
    }
    let start = tip_height.saturating_sub(distance - 1);
    let mut headers = Vec::with_capacity(distance as usize);
    for height in start..=tip_height {
        let hash = storage.blocks().get_hash_by_height(height).ok().flatten()?;
        let header = storage.blocks().get_header(&hash).ok().flatten()?;
        headers.push(header);
    }
    if headers.len() as u64 != distance {
        return None;
    }
    let last = blvm_consensus::block::block_header_hash(headers.last()?);
    if last != *tip_hash {
        return None;
    }
    Some(headers)
}

const HEADER_PROBE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(120);

static HEADER_PROBE_FAILS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<SocketAddr, std::time::Instant>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn note_header_probe_failure(peer: SocketAddr) {
    let mut fails = HEADER_PROBE_FAILS.lock().unwrap_or_else(|e| e.into_inner());
    fails.insert(peer, std::time::Instant::now());
}

fn clear_header_probe_failure(peer: SocketAddr) {
    let mut fails = HEADER_PROBE_FAILS.lock().unwrap_or_else(|e| e.into_inner());
    fails.remove(&peer);
}

fn header_probe_failures() -> Vec<SocketAddr> {
    let mut fails = HEADER_PROBE_FAILS.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    fails.retain(|_, failed_at| now.duration_since(*failed_at) < HEADER_PROBE_COOLDOWN);
    fails.keys().copied().collect()
}

/// Same-port peers first. Peers that recently timed out are left out until the
/// cooldown ends, so one silent address cannot own every probe.
pub(crate) fn order_header_probe_peers(
    peers: &[SocketAddr],
    network_port: u16,
    failed: &[SocketAddr],
) -> Vec<SocketAddr> {
    let failed: std::collections::HashSet<SocketAddr> = failed.iter().copied().collect();
    let mut same_port = Vec::new();
    let mut other = Vec::new();
    for peer in peers {
        if failed.contains(peer) {
            continue;
        }
        if peer.port() == network_port {
            same_port.push(*peer);
        } else {
            other.push(*peer);
        }
    }
    same_port.extend(other);
    same_port
}

/// A peer on the wrong P2P port gets a short probe. Waiting 30s on mainnet
/// `:8333` during a testnet4 catch-up is how one address stalls the tip.
pub(crate) fn header_probe_timeout_secs(peer_port: u16, network_port: u16, configured: u64) -> u64 {
    if peer_port == network_port {
        configured
    } else {
        configured.min(3)
    }
}

/// Result of a `getheaders` tip probe.
pub(crate) struct HeadersTipProbe {
    pub peer_height: u64,
    pub peer: SocketAddr,
    /// First stored header that still needs a body.
    pub first_missing_hash: Option<Hash>,
    /// Bodies still missing on the stored prefix, in chain order.
    pub missing_bodies: Vec<Hash>,
    /// The prefix does not pass through the active tip.
    pub forks_before_tip: bool,
    /// First height in the probe that is not on the active chain.
    /// Undo is required from here through the tip to disconnect onto this fork.
    pub disconnect_from: Option<u64>,
    /// Last stored header has more cumulative work than the active tip.
    pub heavier_than_tip: bool,
    pub heavier_tip: Option<Hash>,
}

struct StoredProbeHeaders {
    stored: usize,
    last_height: Option<u64>,
    missing_bodies: Vec<Hash>,
    forks_before_tip: bool,
    disconnect_from: Option<u64>,
    heavier_than_tip: bool,
    heavier_tip: Option<Hash>,
}

impl StoredProbeHeaders {
    fn none() -> Self {
        Self {
            stored: 0,
            last_height: None,
            missing_bodies: Vec::new(),
            forks_before_tip: false,
            disconnect_from: None,
            heavier_than_tip: false,
            heavier_tip: None,
        }
    }
}

impl NetworkManager {
    /// Request unknown blocks announced in a peer `inv` message (post-IBD tip follow).
    pub async fn process_announced_block_inventory(
        &self,
        data: &[u8],
        peer_addr: SocketAddr,
    ) -> Result<()> {
        let inv_msg = match ProtocolParser::parse_message(data) {
            Ok(ProtocolMessage::Inv(msg)) => msg,
            _ => return Ok(()),
        };

        let Some(storage) = self.storage().as_ref() else {
            return Ok(());
        };

        let local_height = storage.chain().get_height()?.unwrap_or(0);
        // IBD owns GetHeaders/GetData from genesis. A height-0 inv probe
        // registered on peers[0] and poisoned the header FIFO (R-48 640k sit;
        // R-49 100k sit after CATCH_UP timeout + mass disconnect).
        if local_height == 0
            || crate::node::parallel_ibd::PARALLEL_IBD_SESSION_ACTIVE
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(());
        }
        let blockstore = storage.blocks();
        let announced_block = inv_msg
            .inventory
            .iter()
            .any(|item| item.inv_type == MSG_BLOCK || item.inv_type == MSG_WITNESS_BLOCK);
        if announced_block {
            self.note_block_announcement(peer_addr).await;
        }

        let peer_key = peer_addr.to_string();

        let mut inventory: Vec<InventoryVector> = Vec::new();
        {
            let mut inv_mgr = self.inventory().lock().unwrap_or_else(|e| e.into_inner());

            for item in &inv_msg.inventory {
                if item.inv_type != MSG_BLOCK && item.inv_type != MSG_WITNESS_BLOCK {
                    continue;
                }
                if blockstore.has_block_body(&item.hash).unwrap_or(false) {
                    continue;
                }
                if inv_mgr.has_pending_request(&item.hash) {
                    continue;
                }
                inventory.push(InventoryVector {
                    inv_type: MSG_WITNESS_BLOCK,
                    hash: item.hash,
                });
                if inventory.len() >= MAX_BLOCK_REQUESTS_PER_INV {
                    break;
                }
            }
        }

        if inventory.is_empty() {
            return Ok(());
        }

        // Headers stored by the probe are fetched before the raw inv. That covers a
        // same-height sibling as well as a gap of more than one block.
        // Parallel IBD is not started.
        if let Ok((tip_hash, _)) = storage.chain().get_tip_hash_and_height() {
            if let Ok(Some(probed)) = self
                .refresh_peer_tip_from(peer_addr, local_height, tip_hash)
                .await
            {
                if !probed.missing_bodies.is_empty() {
                    info!(
                        "[BLOCK_RELAY] Local {} peer tip ≈ {} — getdata for {} missing block(s)",
                        local_height,
                        probed.peer_height,
                        probed.missing_bodies.len()
                    );
                    let missing: Vec<InventoryVector> = probed
                        .missing_bodies
                        .iter()
                        .take(MAX_BLOCK_REQUESTS_PER_INV)
                        .map(|hash| InventoryVector {
                            inv_type: MSG_WITNESS_BLOCK,
                            hash: *hash,
                        })
                        .collect();
                    return self
                        .send_block_getdata(peer_addr, &peer_key, missing, local_height)
                        .await;
                }
            }
        }

        self.send_block_getdata(peer_addr, &peer_key, inventory, local_height)
            .await
    }

    async fn send_block_getdata(
        &self,
        peer_addr: SocketAddr,
        peer_key: &str,
        inventory: Vec<InventoryVector>,
        local_height: u64,
    ) -> Result<()> {
        self.bump_peer_best_height(peer_addr, local_height.saturating_add(1))
            .await;

        let getdata = GetDataMessage {
            inventory: inventory.clone(),
        };
        let count = getdata.inventory.len();
        let wire = ProtocolParser::serialize_message(&ProtocolMessage::GetData(getdata))?;
        self.send_to_peer(peer_addr, wire).await?;
        {
            let mut inv_mgr = self.inventory().lock().unwrap_or_else(|e| e.into_inner());
            for item in &inventory {
                inv_mgr
                    .request_data(item.hash, item.inv_type, peer_key)
                    .map_err(|e| anyhow::anyhow!("inventory request_data: {e}"))?;
            }
        }
        info!(
            "[BLOCK_RELAY] Requested {} block(s) from {} (local height {})",
            count, peer_addr, local_height
        );
        Ok(())
    }

    /// When version `start_height` equals our tip, probe the best peer with `getheaders` to learn
    /// the live chain tip (handles multi-block gaps while connections stay open).
    pub(crate) async fn refresh_peer_tip_via_headers(
        &self,
        local_height: u64,
        tip_hash: Hash,
    ) -> Result<Option<HeadersTipProbe>> {
        let peers = self.get_connected_peer_addresses().await;
        if peers.is_empty() {
            return Ok(None);
        }
        let network_port = ProtocolParser::dns_seed_network().1;
        let ordered = order_header_probe_peers(&peers, network_port, &header_probe_failures());
        let Some(peer) = ordered.first().copied() else {
            debug!("[CATCH_UP] every connected peer recently failed a header probe");
            return Ok(None);
        };
        self.refresh_peer_tip_from(peer, local_height, tip_hash)
            .await
    }

    /// Probe `peer_addr` with `getheaders` and store the linking prefix.
    pub(crate) async fn refresh_peer_tip_from(
        &self,
        peer_addr: SocketAddr,
        local_height: u64,
        tip_hash: Hash,
    ) -> Result<Option<HeadersTipProbe>> {
        let get_headers = GetHeadersMessage {
            version: 70015,
            block_locator_hashes: self.tip_follow_locator(local_height, tip_hash),
            hash_stop: [0; 32],
        };
        let wire = ProtocolParser::serialize_message(&ProtocolMessage::GetHeaders(get_headers))?;
        let headers_rx = self.register_headers_request(peer_addr);

        if let Err(e) = self.send_to_peer(peer_addr, wire).await {
            note_header_probe_failure(peer_addr);
            debug!(
                "[CATCH_UP] getheaders tip probe to {} failed: {}",
                peer_addr, e
            );
            return Ok(None);
        }

        let network_port = ProtocolParser::dns_seed_network().1;
        let timeout_secs = header_probe_timeout_secs(
            peer_addr.port(),
            network_port,
            self.headers_probe_timeout_secs(),
        );
        match timeout(Duration::from_secs(timeout_secs), headers_rx).await {
            Ok(Ok(headers)) if headers.is_empty() => {
                clear_header_probe_failure(peer_addr);
                debug!(
                    "[CATCH_UP] getheaders from {}: empty (no shared block after the locator)",
                    peer_addr
                );
                Ok(Some(HeadersTipProbe {
                    peer_height: 0,
                    peer: peer_addr,
                    first_missing_hash: None,
                    missing_bodies: Vec::new(),
                    forks_before_tip: false,
                    disconnect_from: None,
                    heavier_than_tip: false,
                    heavier_tip: None,
                }))
            }
            Ok(Ok(headers)) => {
                clear_header_probe_failure(peer_addr);
                let stored = self.store_headers_linking_to_tip(tip_hash, local_height, &headers);
                let peer_tip = stored.last_height.unwrap_or(local_height);
                if stored.stored > 0 {
                    self.bump_peer_best_height(peer_addr, peer_tip).await;
                }
                info!(
                    "[CATCH_UP] getheaders from {}: peer tip ≈ {} ({} headers stored, {} received, local {}, heavier={}, missing_bodies={})",
                    peer_addr,
                    peer_tip,
                    stored.stored,
                    headers.len(),
                    local_height,
                    stored.heavier_than_tip,
                    stored.missing_bodies.len()
                );
                Ok(Some(HeadersTipProbe {
                    peer_height: peer_tip,
                    peer: peer_addr,
                    first_missing_hash: stored.missing_bodies.first().copied(),
                    missing_bodies: stored.missing_bodies,
                    forks_before_tip: stored.forks_before_tip,
                    disconnect_from: stored.disconnect_from,
                    heavier_than_tip: stored.heavier_than_tip,
                    heavier_tip: stored.heavier_tip,
                }))
            }
            Ok(Err(_)) => {
                note_header_probe_failure(peer_addr);
                warn!(
                    "[CATCH_UP] getheaders response channel closed for {}",
                    peer_addr
                );
                Ok(None)
            }
            Err(_) => {
                note_header_probe_failure(peer_addr);
                warn!(
                    "[CATCH_UP] getheaders tip probe to {} timed out after {}s",
                    peer_addr, timeout_secs
                );
                Ok(None)
            }
        }
    }

    /// Locator hashes from the tip back to genesis, the same steps as header sync.
    fn tip_follow_locator(&self, local_height: u64, tip_hash: Hash) -> Vec<Hash> {
        use crate::node::parallel_ibd::ibd_header_locator_heights;

        let Some(storage) = self.storage().as_ref() else {
            return vec![tip_hash];
        };
        let Ok(stored_network) = storage.chain().consensus_network() else {
            return vec![tip_hash];
        };
        let network = crate::network::header_work::network_for_header_work(
            self.protocol_engine()
                .map(|engine| engine.get_protocol_version()),
            stored_network,
        );
        let genesis = blvm_consensus::block::block_header_hash(
            &blvm_consensus::block::genesis_block(network).header,
        );
        let blocks = storage.blocks();
        let mut hashes = Vec::new();
        for height in ibd_header_locator_heights(local_height) {
            if height == local_height {
                hashes.push(tip_hash);
                continue;
            }
            match blocks.get_hash_by_height(height) {
                Ok(Some(stored)) => hashes.push(stored),
                Ok(None) if height == 0 => hashes.push(genesis),
                _ => {}
            }
        }
        if hashes.is_empty() {
            hashes.push(tip_hash);
        }
        hashes
    }

    /// Store the prefix of `headers` that connects to the tip or to a known ancestor.
    ///
    /// A header that misses the required work or breaks the chain ends the prefix.
    /// A header whose height already names a different block is stored by hash and does
    /// not replace the active height entry. Each stored header caches cumulative
    /// chainwork so a heavier fork can be compared with the active tip.
    fn store_headers_linking_to_tip(
        &self,
        tip_hash: Hash,
        _local_height: u64,
        headers: &[blvm_protocol::BlockHeader],
    ) -> StoredProbeHeaders {
        let Some(storage) = self.storage().as_ref() else {
            return StoredProbeHeaders::none();
        };
        let mut anchor: Option<(usize, Hash, u64)> = None;
        for (i, header) in headers.iter().enumerate() {
            if header.prev_block_hash == tip_hash {
                anchor = Some((i, tip_hash, _local_height));
                break;
            }
            if let Ok(Some(parent_height)) =
                storage.blocks().get_height_by_hash(&header.prev_block_hash)
            {
                anchor = Some((i, header.prev_block_hash, parent_height));
                break;
            }
        }
        let Some((start, mut prev, mut height)) = anchor else {
            return StoredProbeHeaders::none();
        };
        let Ok(stored_network) = storage.chain().consensus_network() else {
            return StoredProbeHeaders::none();
        };
        let consensus_network = crate::network::header_work::network_for_header_work(
            self.protocol_engine()
                .map(|engine| engine.get_protocol_version()),
            stored_network,
        );
        let tip_header = storage.chain().get_tip_header().ok().flatten();
        let tip_height = storage.chain().get_height().ok().flatten();
        let mut pending: Vec<(u64, u64, u64)> = Vec::new();
        let mut batch = Vec::new();
        for header in headers.iter().skip(start) {
            let next_height = height.saturating_add(1);
            let meets = {
                let stored_at = |h: u64| {
                    if let Some(found) = storage.blocks().get_header_at_height(h).ok().flatten() {
                        return Some((found.bits, found.timestamp));
                    }
                    if tip_height == Some(h) {
                        return tip_header.as_ref().map(|tip| (tip.bits, tip.timestamp));
                    }
                    None
                };
                let ancestor =
                    crate::network::header_work::ancestor_seeing_pending(&pending, &stored_at);
                crate::network::header_work::header_meets_required_work(
                    header,
                    next_height,
                    consensus_network,
                    &ancestor,
                )
            };
            if header.version < 1
                || header.timestamp == 0
                || header.prev_block_hash != prev
                || !meets
            {
                break;
            }
            height = next_height;
            pending.push((height, header.bits, header.timestamp));
            let hash = blvm_consensus::block::block_header_hash(header);
            batch.push((hash, header.clone(), height));
            prev = hash;
        }
        let stored = batch.len();
        let last_height = batch.last().map(|(_, _, height)| *height);
        if stored == 0 {
            return StoredProbeHeaders::none();
        }
        let mut on_active = Vec::new();
        // The first header is on the active chain only when its prev is the active
        // hash at the previous height. A child of a side header at that height is
        // not. Once a header is off the active chain, later headers in this batch
        // stay off it too, including an empty next height.
        let mut disconnect_from = None;
        let mut extends_active = batch.first().is_some_and(|(_, header, height)| {
            let parent_height = height.saturating_sub(1);
            let indexed = storage
                .blocks()
                .get_hash_by_height(parent_height)
                .ok()
                .flatten();
            if indexed == Some(header.prev_block_hash) {
                return true;
            }
            // A different block already owns this height. The header's parent is a side hash.
            if indexed.is_some() {
                return false;
            }
            // The tip row is not in the height index yet. The chain tip is still the parent.
            let tip = storage.chain().get_tip_hash().ok().flatten();
            let tip_height = storage.chain().get_height().ok().flatten().unwrap_or(0);
            tip == Some(header.prev_block_hash) && parent_height == tip_height
        });
        for (hash, header, height) in &batch {
            let occupied = storage.blocks().get_hash_by_height(*height).ok().flatten();
            let claim = extends_active && occupied.is_none_or(|existing| existing == *hash);
            if claim {
                on_active.push((*hash, header.clone(), *height));
                extends_active = true;
            } else {
                if disconnect_from.is_none() {
                    disconnect_from = Some(*height);
                }
                extends_active = false;
                if let Err(e) = storage.blocks().store_header(hash, header) {
                    warn!("probe headers failed to store: {e}");
                    return StoredProbeHeaders::none();
                }
                if let Err(e) = storage.blocks().store_hash_height(*height, hash) {
                    warn!("probe headers failed to store: {e}");
                    return StoredProbeHeaders::none();
                }
            }
        }
        if !on_active.is_empty() {
            if let Err(e) = storage.blocks().store_headers_batch(&on_active) {
                warn!("probe headers failed to store: {e}");
                return StoredProbeHeaders::none();
            }
        }
        for (hash, header, height) in &batch {
            if let Err(e) = storage.chain().cache_block_chainwork(hash, header, *height) {
                warn!("probe header chainwork not cached: {e}");
            }
        }
        let missing_bodies: Vec<Hash> = batch
            .iter()
            .filter(|(hash, _, _)| !storage.blocks().has_block(hash).unwrap_or(false))
            .map(|(hash, _, _)| *hash)
            .collect();
        let passes_through_tip = batch
            .iter()
            .any(|(hash, header, _)| *hash == tip_hash || header.prev_block_hash == tip_hash);
        let heavier_tip = batch.last().map(|(hash, _, _)| *hash);
        let heavier_than_tip = heavier_tip.is_some_and(|hash| {
            crate::node::chain_selector::should_activate_over_active_tip(storage, &hash)
                .unwrap_or(false)
        });
        StoredProbeHeaders {
            stored,
            last_height,
            missing_bodies,
            forks_before_tip: !passes_through_tip,
            disconnect_from,
            heavier_than_tip,
            heavier_tip: heavier_tip.filter(|_| heavier_than_tip),
        }
    }

    pub(crate) async fn request_header_chain_bodies(
        &self,
        peer_addr: SocketAddr,
        hashes: &[Hash],
    ) -> Result<()> {
        let inventory: Vec<InventoryVector> = hashes
            .iter()
            .take(MAX_BLOCK_REQUESTS_PER_INV)
            .map(|hash| InventoryVector {
                inv_type: MSG_WITNESS_BLOCK,
                hash: *hash,
            })
            .collect();
        if inventory.is_empty() {
            return Ok(());
        }
        self.send_block_getdata(peer_addr, &peer_addr.to_string(), inventory, 0)
            .await
    }

    pub(crate) async fn note_block_announcement(&self, peer_addr: SocketAddr) {
        let mut pm = self.peer_manager().await;
        let transport = pm
            .find_transport_addr_by_socket(peer_addr)
            .unwrap_or(TransportAddr::Tcp(peer_addr));
        if let Some(peer) = pm.get_peer_mut(&transport) {
            peer.record_block_announcement();
        }
    }

    /// Tell connected peers about a block that just became the tip.
    ///
    /// A peer whose known header is 1 to 8 blocks behind receives that many headers.
    /// A wider gap, or a peer with no known header height, receives nothing.
    pub(crate) async fn announce_connected_block(
        &self,
        hash: Hash,
        header: blvm_protocol::BlockHeader,
    ) {
        if !self.relay_policies().enable_block_relay {
            return;
        }
        let Some(storage) = self.storage().as_ref() else {
            return;
        };
        let Some(tip_height) = storage
            .blocks()
            .get_height_by_hash(&hash)
            .ok()
            .flatten()
            .or_else(|| {
                let tip = storage.chain().get_tip_hash().ok().flatten()?;
                if tip != hash {
                    return None;
                }
                storage.chain().get_height().ok().flatten()
            })
        else {
            return;
        };
        let peers = self.get_connected_peer_addresses().await;
        for peer_addr in peers {
            let known = self
                .peer_states()
                .read()
                .await
                .get(&peer_addr)
                .and_then(|state| state.known_header_height);
            let Some(distance) = header_announce_len(tip_height, known) else {
                continue;
            };
            let Some(headers) = headers_for_announce(storage, tip_height, distance, &hash, &header)
            else {
                continue;
            };
            let message = ProtocolMessage::Headers(HeadersMessage { headers });
            if let Ok(wire) = ProtocolParser::serialize_message(&message) {
                if self.send_to_peer(peer_addr, wire).await.is_ok() {
                    self.note_known_header_height(peer_addr, tip_height).await;
                }
            }
        }
    }

    /// Offer announce-true `sendcmpct` while fewer than three peers hold a slot.
    /// A peer that does not get a slot is sent nothing.
    pub(crate) async fn fill_high_bandwidth_slots(&self) {
        let candidates = {
            let states = self.peer_states().read().await;
            high_bandwidth_candidates(&states)
        };
        for peer_addr in candidates {
            let took = {
                let mut states = self.peer_states().write().await;
                if states.values().filter(|state| state.high_bandwidth).count()
                    >= MAX_HIGH_BANDWIDTH_PEERS
                {
                    false
                } else if let Some(state) = states.get_mut(&peer_addr) {
                    if state.handshake_complete && !state.high_bandwidth {
                        state.high_bandwidth = true;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };
            if !took {
                continue;
            }
            if self
                .send_high_bandwidth_cmpct(peer_addr, true)
                .await
                .is_err()
            {
                let mut states = self.peer_states().write().await;
                if let Some(state) = states.get_mut(&peer_addr) {
                    state.high_bandwidth = false;
                }
            }
        }
    }

    /// A peer leaving the set is told announce false. A peer that never had the slot is not.
    pub(crate) async fn revoke_high_bandwidth_peer(&self, peer_addr: SocketAddr) -> bool {
        let leave = {
            let mut states = self.peer_states().write().await;
            states
                .get_mut(&peer_addr)
                .is_some_and(revoke_high_bandwidth)
        };
        if leave {
            let _ = self.send_high_bandwidth_cmpct(peer_addr, false).await;
        }
        leave
    }

    async fn send_high_bandwidth_cmpct(&self, peer_addr: SocketAddr, announce: bool) -> Result<()> {
        let message = ProtocolMessage::SendCmpct(SendCmpctMessage::for_transport(
            TransportType::Tcp,
            announce,
        ));
        let wire = ProtocolParser::serialize_message(&message)?;
        self.send_to_peer(peer_addr, wire).await
    }

    pub(crate) async fn note_known_header_height(&self, peer_addr: SocketAddr, height: u64) {
        let mut states = self.peer_states().write().await;
        if let Some(state) = states.get_mut(&peer_addr) {
            advance_known_header_height(state, height);
        }
    }

    /// Tell connected peers about a transaction the mempool accepted.
    /// A peer with a fee filter receives the inv only when the rate meets that filter.
    pub(crate) async fn announce_accepted_transaction(
        &self,
        txid: Hash,
        fee_basis: Option<(u64, u64)>,
    ) {
        if !self.relay_policies().enable_tx_relay {
            return;
        }
        let peers = self.get_connected_peer_addresses().await;
        let mut eligible = Vec::new();
        for peer_addr in peers {
            let allowed = {
                if self.outbound_role_is_block_only(peer_addr).await {
                    false
                } else {
                    let states = self.peer_states().read().await;
                    match states.get(&peer_addr) {
                        Some(state) if !state.relay_tx => false,
                        Some(state) => match state.min_fee_rate {
                            Some(min) => match fee_basis {
                                Some((fee_sats, vsize)) => {
                                    super::wire_dispatch::fee_rate_meets_filter(
                                        fee_sats, vsize, min,
                                    )
                                }
                                None => false,
                            },
                            None => true,
                        },
                        None => true,
                    }
                }
            };
            if allowed {
                eligible.push(peer_addr);
            }
        }
        let peer_ids: Vec<String> = eligible.iter().map(|addr| addr.to_string()).collect();
        let stem_peer = {
            let mut relay = self
                .relay_manager()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !relay.should_relay_transaction(&txid) {
                return;
            }
            let stem = relay.relay_transaction_dandelion(txid, "local".to_string(), &peer_ids);
            relay.mark_transaction_relayed(txid);
            stem
        };
        let targets: Vec<SocketAddr> = match stem_peer {
            Some(peer_id) => eligible
                .into_iter()
                .filter(|addr| addr.to_string() == peer_id)
                .collect(),
            None => eligible,
        };
        for peer_addr in targets {
            let inv_type = {
                let states = self.peer_states().read().await;
                crate::network::inventory::tx_announce_inv_type(
                    states
                        .get(&peer_addr)
                        .is_some_and(|state| state.wtxid_relay),
                )
            };
            let message = ProtocolMessage::Inv(crate::network::protocol::InvMessage {
                inventory: vec![InventoryVector {
                    inv_type,
                    hash: txid,
                }],
            });
            if let Ok(wire) = ProtocolParser::serialize_message(&message) {
                let _ = self.send_to_peer(peer_addr, wire).await;
            }
        }
    }

    async fn bump_peer_best_height(&self, peer_addr: SocketAddr, height: u64) {
        let mut pm = self.peer_manager().await;
        let transport = pm
            .find_transport_addr_by_socket(peer_addr)
            .unwrap_or(TransportAddr::Tcp(peer_addr));
        if let Some(peer) = pm.get_peer_mut(&transport) {
            let current = peer
                .best_block_height()
                .unwrap_or(peer.start_height().max(0) as u64);
            if height > current {
                peer.set_best_block_height(height);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::transport::TransportAddr;
    use crate::node::mempool::MempoolManager;
    use crate::storage::Storage;
    use blvm_protocol::{BitcoinProtocolEngine, BlockHeader, ProtocolVersion};
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    #[test]
    fn a_header_gap_past_eight_is_not_announced() {
        assert_eq!(header_announce_len(100, None), None);
        assert_eq!(header_announce_len(100, Some(100)), None);
        assert_eq!(header_announce_len(100, Some(99)), Some(1));
        assert_eq!(header_announce_len(100, Some(92)), Some(8));
        assert_eq!(header_announce_len(100, Some(91)), None);
    }

    #[test]
    fn high_bandwidth_keeps_three_and_does_not_clear_a_peer_that_was_never_selected() {
        use blvm_protocol::network::PeerState;
        use std::collections::HashMap;
        let mut states = HashMap::new();
        for host in 1u8..=5 {
            let mut state = PeerState::new();
            state.handshake_complete = true;
            states.insert(SocketAddr::from(([10, 0, 0, host], 8333)), state);
        }
        let picked = high_bandwidth_candidates(&states);
        assert_eq!(picked.len(), MAX_HIGH_BANDWIDTH_PEERS);
        for addr in &picked {
            states.get_mut(addr).unwrap().high_bandwidth = true;
        }
        assert!(high_bandwidth_candidates(&states).is_empty());
        let never = states
            .keys()
            .copied()
            .find(|addr| !picked.contains(addr))
            .unwrap();
        assert!(!revoke_high_bandwidth(states.get_mut(&never).unwrap()));
        assert!(revoke_high_bandwidth(states.get_mut(&picked[0]).unwrap()));
        assert!(!states.get(&picked[0]).unwrap().high_bandwidth);
        assert_eq!(high_bandwidth_candidates(&states).len(), 1);
    }

    async fn read_frame(sock: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut header = [0u8; 24];
        sock.read_exact(&mut header).await.unwrap();
        let len = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
        let mut frame = header.to_vec();
        let mut payload = vec![0u8; len];
        sock.read_exact(&mut payload).await.unwrap();
        frame.extend(payload);
        frame
    }

    fn connect_tip(storage: &Storage, tip_hash: &Hash, tip_header: &BlockHeader, height: u64) {
        if height > 0 {
            storage
                .chain()
                .cache_block_chainwork(&tip_header.prev_block_hash, tip_header, 0)
                .unwrap();
            // Regtest walks minimum-difficulty bits back to the period start. The tip
            // alone does not answer that lookup. The height-0 row is not the tip's prev.
            let root = BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [4u8; 32],
                timestamp: tip_header.timestamp.saturating_sub(1),
                bits: tip_header.bits,
                nonce: 0,
            };
            storage
                .blocks()
                .store_headers_batch(&[
                    ([0x5a; 32], root, 0),
                    (*tip_hash, tip_header.clone(), height),
                ])
                .unwrap();
        }
        storage
            .chain()
            .update_tip(tip_hash, tip_header, height)
            .unwrap();
    }

    fn with_proof(mut header: BlockHeader) -> BlockHeader {
        for nonce in 0..200_000 {
            header.nonce = nonce;
            if blvm_protocol::pow::check_proof_of_work(&header).unwrap_or(false) {
                return header;
            }
        }
        panic!("header did not meet the regtest target");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inv_lock_two_headers_stores_and_requests_first_block() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let tip_hash = [9u8; 32];
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [1u8; 32],
            timestamp: 1_700_000_000,
            bits: 0x207fffff,
            nonce: 0,
        };
        connect_tip(&storage, &tip_hash, &tip_header, 1);

        let header_one = with_proof(BlockHeader {
            version: 1,
            prev_block_hash: tip_hash,
            merkle_root: [2u8; 32],
            timestamp: 1_700_000_001,
            bits: 0x207fffff,
            nonce: 0,
        });
        let hash_one = blvm_consensus::block::block_header_hash(&header_one);
        let header_two = with_proof(BlockHeader {
            version: 1,
            prev_block_hash: hash_one,
            merkle_root: [3u8; 32],
            timestamp: 1_700_000_002,
            bits: 0x207fffff,
            nonce: 0,
        });
        let hash_two = blvm_consensus::block::block_header_hash(&header_two);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = listener.local_addr().unwrap();
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let nm = Arc::new(
            NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
                protocol,
                Arc::clone(&storage),
                Arc::new(MempoolManager::new()),
            ),
        );

        let nm_read = Arc::clone(&nm);
        let announced = vec![header_one.clone(), header_two.clone()];
        let reader = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut getdata = None;
            for _ in 0..4 {
                let frame = read_frame(&mut sock).await;
                match ProtocolParser::parse_message(&frame).unwrap() {
                    ProtocolMessage::GetHeaders(_) => {
                        assert!(nm_read.complete_headers_request(peer_addr, announced.clone()));
                    }
                    ProtocolMessage::GetData(msg) => {
                        getdata = Some(msg);
                        break;
                    }
                    _ => {}
                }
            }
            getdata.expect("getdata")
        });

        nm.connect_to_peer(peer_addr).await.unwrap();
        let inv = ProtocolMessage::Inv(crate::network::protocol::InvMessage {
            inventory: vec![
                InventoryVector {
                    inv_type: MSG_BLOCK,
                    hash: hash_one,
                },
                InventoryVector {
                    inv_type: MSG_BLOCK,
                    hash: hash_two,
                },
            ],
        });
        let inv_bytes = ProtocolParser::serialize_message(&inv).unwrap();
        timeout(
            Duration::from_secs(10),
            nm.process_announced_block_inventory(&inv_bytes, peer_addr),
        )
        .await
        .expect("headers probe")
        .unwrap();

        let getdata = timeout(Duration::from_secs(5), reader)
            .await
            .expect("reader")
            .unwrap();
        assert_eq!(getdata.inventory.len(), 2);
        assert_eq!(getdata.inventory[0].hash, hash_one);
        assert_eq!(getdata.inventory[1].hash, hash_two);
        assert_eq!(getdata.inventory[0].inv_type, MSG_WITNESS_BLOCK);

        let stored_one = storage
            .blocks()
            .get_header_at_height(2)
            .unwrap()
            .expect("first header");
        let stored_two = storage
            .blocks()
            .get_header_at_height(3)
            .unwrap()
            .expect("second header");
        assert_eq!(stored_one.prev_block_hash, tip_hash);
        assert_eq!(stored_two.prev_block_hash, hash_one);
        assert!(
            !crate::node::parallel_ibd::PARALLEL_IBD_SESSION_ACTIVE
                .load(std::sync::atomic::Ordering::Acquire),
            "parallel IBD stays idle for this gap"
        );
    }

    async fn best_height(nm: &NetworkManager, peer: SocketAddr) -> Option<u64> {
        let pm = nm.peer_manager().await;
        let transport = pm
            .find_transport_addr_by_socket(peer)
            .unwrap_or(TransportAddr::Tcp(peer));
        pm.get_peer(&transport)
            .and_then(|peer| peer.best_block_height())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn probe_height_uses_the_stored_header_count() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let tip_hash = [9u8; 32];
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [1u8; 32],
            timestamp: 1_700_000_000,
            bits: 0x207fffff,
            nonce: 0,
        };
        connect_tip(&storage, &tip_hash, &tip_header, 1);
        let linking = with_proof(BlockHeader {
            version: 1,
            prev_block_hash: tip_hash,
            merkle_root: [2u8; 32],
            timestamp: 1_700_000_001,
            bits: 0x207fffff,
            nonce: 0,
        });
        let unlinked = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [3u8; 32],
            timestamp: 1_700_000_002,
            bits: 0x207fffff,
            nonce: 4,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = listener.local_addr().unwrap();
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let nm = Arc::new(
            NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
                protocol,
                Arc::clone(&storage),
                Arc::new(MempoolManager::new()),
            ),
        );
        let nm_read = Arc::clone(&nm);
        let reply = vec![linking, unlinked];
        let reader = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let frame = read_frame(&mut sock).await;
            match ProtocolParser::parse_message(&frame).unwrap() {
                ProtocolMessage::GetHeaders(_) => {
                    assert!(nm_read.complete_headers_request(peer_addr, reply));
                }
                other => panic!("expected getheaders, got {other:?}"),
            }
        });
        nm.connect_to_peer(peer_addr).await.unwrap();
        let probed = timeout(
            Duration::from_secs(5),
            nm.refresh_peer_tip_from(peer_addr, 1, tip_hash),
        )
        .await
        .expect("probe")
        .unwrap()
        .expect("headers");
        timeout(Duration::from_secs(5), reader)
            .await
            .expect("reader")
            .unwrap();
        assert_eq!(probed.peer_height, 2);
        assert_eq!(best_height(&nm, peer_addr).await, Some(2));
        assert!(storage.blocks().get_hash_by_height(3).unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unlinked_probe_leaves_peer_height_unchanged() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let tip_hash = [9u8; 32];
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [1u8; 32],
            timestamp: 1_700_000_000,
            bits: 0x207fffff,
            nonce: 0,
        };
        connect_tip(&storage, &tip_hash, &tip_header, 1);
        let unlinked = BlockHeader {
            version: 1,
            prev_block_hash: [1u8; 32],
            merkle_root: [3u8; 32],
            timestamp: 1_700_000_002,
            bits: 0x207fffff,
            nonce: 4,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = listener.local_addr().unwrap();
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let nm = Arc::new(
            NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
                protocol,
                storage,
                Arc::new(MempoolManager::new()),
            ),
        );
        let nm_read = Arc::clone(&nm);
        let reader = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let frame = read_frame(&mut sock).await;
            match ProtocolParser::parse_message(&frame).unwrap() {
                ProtocolMessage::GetHeaders(_) => {
                    assert!(nm_read.complete_headers_request(peer_addr, vec![unlinked]));
                }
                other => panic!("expected getheaders, got {other:?}"),
            }
        });
        nm.connect_to_peer(peer_addr).await.unwrap();
        let probed = timeout(
            Duration::from_secs(5),
            nm.refresh_peer_tip_from(peer_addr, 1, tip_hash),
        )
        .await
        .expect("probe")
        .unwrap()
        .expect("headers");
        timeout(Duration::from_secs(5), reader)
            .await
            .expect("reader")
            .unwrap();
        assert_eq!(probed.peer_height, 1);
        assert!(best_height(&nm, peer_addr).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_headers_reply_does_not_claim_the_local_height() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let tip_hash = [9u8; 32];
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [1u8; 32],
            timestamp: 1_700_000_000,
            bits: 0x207fffff,
            nonce: 0,
        };
        connect_tip(&storage, &tip_hash, &tip_header, 1);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = listener.local_addr().unwrap();
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let nm = Arc::new(
            NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
                protocol,
                storage,
                Arc::new(MempoolManager::new()),
            ),
        );
        let nm_read = Arc::clone(&nm);
        let reader = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let frame = read_frame(&mut sock).await;
            match ProtocolParser::parse_message(&frame).unwrap() {
                ProtocolMessage::GetHeaders(_) => {
                    assert!(nm_read.complete_headers_request(peer_addr, Vec::new()));
                }
                other => panic!("expected getheaders, got {other:?}"),
            }
        });
        nm.connect_to_peer(peer_addr).await.unwrap();
        let probed = timeout(
            Duration::from_secs(5),
            nm.refresh_peer_tip_from(peer_addr, 1, tip_hash),
        )
        .await
        .expect("probe")
        .unwrap()
        .expect("headers");
        timeout(Duration::from_secs(5), reader)
            .await
            .expect("reader")
            .unwrap();
        assert_eq!(probed.peer_height, 0);
        assert!(probed.first_missing_hash.is_none());
        assert!(best_height(&nm, peer_addr).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ancestor_header_is_stored_without_replacing_the_tip_height() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let parent = with_proof(BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [4u8; 32],
            timestamp: 1_700_000_000,
            bits: 0x207fffff,
            nonce: 0,
        });
        let parent_hash = blvm_consensus::block::block_header_hash(&parent);
        storage
            .blocks()
            .store_header(&parent_hash, &parent)
            .unwrap();
        storage.blocks().store_height(0, &parent_hash).unwrap();
        let tip_hash = [9u8; 32];
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: parent_hash,
            merkle_root: [1u8; 32],
            timestamp: 1_700_000_001,
            bits: 0x207fffff,
            nonce: 1,
        };
        connect_tip(&storage, &tip_hash, &tip_header, 1);
        storage.blocks().store_height(1, &tip_hash).unwrap();
        let sibling = with_proof(BlockHeader {
            version: 1,
            prev_block_hash: parent_hash,
            merkle_root: [8u8; 32],
            timestamp: 1_700_000_002,
            bits: 0x207fffff,
            nonce: 0,
        });
        let sibling_hash = blvm_consensus::block::block_header_hash(&sibling);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = listener.local_addr().unwrap();
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let nm = Arc::new(
            NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
                protocol,
                Arc::clone(&storage),
                Arc::new(MempoolManager::new()),
            ),
        );
        let nm_read = Arc::clone(&nm);
        let sibling_reply = sibling.clone();
        let reader = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut getdata = None;
            for _ in 0..4 {
                let frame = read_frame(&mut sock).await;
                match ProtocolParser::parse_message(&frame).unwrap() {
                    ProtocolMessage::GetHeaders(msg) => {
                        assert!(msg.block_locator_hashes.contains(&parent_hash));
                        assert!(msg.block_locator_hashes.contains(&tip_hash));
                        assert!(
                            nm_read
                                .complete_headers_request(peer_addr, vec![sibling_reply.clone()])
                        );
                    }
                    ProtocolMessage::GetData(msg) => {
                        getdata = Some(msg);
                        break;
                    }
                    _ => {}
                }
            }
            getdata.expect("getdata for the stored sibling")
        });
        nm.connect_to_peer(peer_addr).await.unwrap();
        let announced = [0xcdu8; 32];
        let inv = ProtocolMessage::Inv(crate::network::protocol::InvMessage {
            inventory: vec![InventoryVector {
                inv_type: MSG_BLOCK,
                hash: announced,
            }],
        });
        let inv_bytes = ProtocolParser::serialize_message(&inv).unwrap();
        timeout(
            Duration::from_secs(5),
            nm.process_announced_block_inventory(&inv_bytes, peer_addr),
        )
        .await
        .expect("inv")
        .unwrap();
        let getdata = timeout(Duration::from_secs(5), reader)
            .await
            .expect("reader")
            .unwrap();
        assert_eq!(getdata.inventory.len(), 1);
        assert_eq!(getdata.inventory[0].hash, sibling_hash);
        assert_ne!(getdata.inventory[0].hash, announced);
        assert_eq!(
            storage.blocks().get_hash_by_height(1).unwrap().unwrap(),
            tip_hash
        );
        assert!(
            storage
                .blocks()
                .get_header(&sibling_hash)
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn heavier_fork_caches_chainwork_and_requests_both_bodies() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let parent = with_proof(BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [4u8; 32],
            timestamp: 1_700_000_000,
            bits: 0x207fffff,
            nonce: 0,
        });
        let parent_hash = blvm_consensus::block::block_header_hash(&parent);
        storage
            .blocks()
            .store_header(&parent_hash, &parent)
            .unwrap();
        storage.blocks().store_height(0, &parent_hash).unwrap();
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: parent_hash,
            merkle_root: [1u8; 32],
            timestamp: 1_700_000_001,
            bits: 0x207fffff,
            nonce: 1,
        };
        let tip_hash = blvm_consensus::block::block_header_hash(&tip_header);
        connect_tip(&storage, &tip_hash, &tip_header, 1);
        storage.blocks().store_height(1, &tip_hash).unwrap();
        storage
            .blocks()
            .store_header(&tip_hash, &tip_header)
            .unwrap();
        let sibling = with_proof(BlockHeader {
            version: 1,
            prev_block_hash: parent_hash,
            merkle_root: [8u8; 32],
            timestamp: 1_700_000_002,
            bits: 0x207fffff,
            nonce: 0,
        });
        let sibling_hash = blvm_consensus::block::block_header_hash(&sibling);
        let child = with_proof(BlockHeader {
            version: 1,
            prev_block_hash: sibling_hash,
            merkle_root: [9u8; 32],
            timestamp: 1_700_000_003,
            bits: 0x207fffff,
            nonce: 0,
        });
        let child_hash = blvm_consensus::block::block_header_hash(&child);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = listener.local_addr().unwrap();
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let nm = Arc::new(
            NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
                protocol,
                Arc::clone(&storage),
                Arc::new(MempoolManager::new()),
            ),
        );
        let nm_read = Arc::clone(&nm);
        let reply = vec![sibling, child];
        let reader = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut getdata = None;
            for _ in 0..4 {
                let frame = read_frame(&mut sock).await;
                match ProtocolParser::parse_message(&frame).unwrap() {
                    ProtocolMessage::GetHeaders(_) => {
                        assert!(nm_read.complete_headers_request(peer_addr, reply.clone()));
                    }
                    ProtocolMessage::GetData(msg) => {
                        getdata = Some(msg);
                        break;
                    }
                    _ => {}
                }
            }
            getdata.expect("getdata for the heavier fork")
        });
        nm.connect_to_peer(peer_addr).await.unwrap();
        let inv = ProtocolMessage::Inv(crate::network::protocol::InvMessage {
            inventory: vec![InventoryVector {
                inv_type: MSG_BLOCK,
                hash: [0x11u8; 32],
            }],
        });
        let inv_bytes = ProtocolParser::serialize_message(&inv).unwrap();
        timeout(
            Duration::from_secs(5),
            nm.process_announced_block_inventory(&inv_bytes, peer_addr),
        )
        .await
        .expect("inv")
        .unwrap();
        let getdata = timeout(Duration::from_secs(5), reader)
            .await
            .expect("reader")
            .unwrap();
        assert_eq!(
            getdata
                .inventory
                .iter()
                .map(|item| item.hash)
                .collect::<Vec<_>>(),
            vec![sibling_hash, child_hash]
        );
        assert_eq!(
            storage.blocks().get_hash_by_height(1).unwrap().unwrap(),
            tip_hash
        );
        assert!(
            storage.blocks().get_hash_by_height(2).unwrap().is_none(),
            "the heavier child must not become the next active height while height 1 is the other block"
        );
        let tip_work = storage.chain().get_chainwork(&tip_hash).unwrap().unwrap();
        let sibling_work = storage
            .chain()
            .get_chainwork(&sibling_hash)
            .unwrap()
            .expect("sibling chainwork cached");
        let child_work = storage
            .chain()
            .get_chainwork(&child_hash)
            .unwrap()
            .expect("child chainwork cached");
        assert_eq!(sibling_work, tip_work);
        assert!(child_work > tip_work);
        assert!(
            !crate::node::chain_selector::should_activate_over_active_tip(&storage, &sibling_hash)
                .unwrap()
        );
        assert!(
            crate::node::chain_selector::should_activate_over_active_tip(&storage, &child_hash)
                .unwrap()
        );
    }

    /// av3: the other block at the tip height is indexed by hash only. Its child
    /// must not be stored as the next active height. That child was coinbase-only,
    /// so connecting it did not spend anything from our tip.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn side_child_does_not_claim_the_next_height() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let parent = with_proof(BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [4u8; 32],
            timestamp: 1_700_000_000,
            bits: 0x207fffff,
            nonce: 0,
        });
        let parent_hash = blvm_consensus::block::block_header_hash(&parent);
        storage
            .blocks()
            .store_header(&parent_hash, &parent)
            .unwrap();
        storage.blocks().store_height(0, &parent_hash).unwrap();
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: parent_hash,
            merkle_root: [1u8; 32],
            timestamp: 1_700_000_001,
            bits: 0x207fffff,
            nonce: 1,
        };
        let tip_hash = blvm_consensus::block::block_header_hash(&tip_header);
        connect_tip(&storage, &tip_hash, &tip_header, 1);
        storage.blocks().store_height(1, &tip_hash).unwrap();
        storage
            .blocks()
            .store_header(&tip_hash, &tip_header)
            .unwrap();
        let side = with_proof(BlockHeader {
            version: 1,
            prev_block_hash: parent_hash,
            merkle_root: [8u8; 32],
            timestamp: 1_700_000_002,
            bits: 0x207fffff,
            nonce: 0,
        });
        let side_hash = blvm_consensus::block::block_header_hash(&side);
        storage.blocks().store_header(&side_hash, &side).unwrap();
        storage.blocks().store_hash_height(1, &side_hash).unwrap();
        let child = with_proof(BlockHeader {
            version: 1,
            prev_block_hash: side_hash,
            merkle_root: [9u8; 32],
            timestamp: 1_700_000_003,
            bits: 0x207fffff,
            nonce: 0,
        });
        let child_hash = blvm_consensus::block::block_header_hash(&child);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = listener.local_addr().unwrap();
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let nm = Arc::new(
            NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
                protocol,
                Arc::clone(&storage),
                Arc::new(MempoolManager::new()),
            ),
        );
        let nm_read = Arc::clone(&nm);
        let reply = vec![child];
        let reader = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut getdata = None;
            for _ in 0..4 {
                let frame = read_frame(&mut sock).await;
                match ProtocolParser::parse_message(&frame).unwrap() {
                    ProtocolMessage::GetHeaders(_) => {
                        assert!(nm_read.complete_headers_request(peer_addr, reply.clone()));
                    }
                    ProtocolMessage::GetData(msg) => {
                        getdata = Some(msg);
                        break;
                    }
                    _ => {}
                }
            }
            getdata.expect("getdata for the side child")
        });
        nm.connect_to_peer(peer_addr).await.unwrap();
        let inv = ProtocolMessage::Inv(crate::network::protocol::InvMessage {
            inventory: vec![InventoryVector {
                inv_type: MSG_BLOCK,
                hash: [0x11u8; 32],
            }],
        });
        let inv_bytes = ProtocolParser::serialize_message(&inv).unwrap();
        timeout(
            Duration::from_secs(5),
            nm.process_announced_block_inventory(&inv_bytes, peer_addr),
        )
        .await
        .expect("inv")
        .unwrap();
        let getdata = timeout(Duration::from_secs(5), reader)
            .await
            .expect("reader")
            .unwrap();
        assert_eq!(getdata.inventory[0].hash, child_hash);
        assert_eq!(
            storage.blocks().get_hash_by_height(1).unwrap().unwrap(),
            tip_hash
        );
        assert!(
            storage.blocks().get_hash_by_height(2).unwrap().is_none(),
            "side child must not occupy the next height"
        );
        assert!(storage.blocks().get_header(&child_hash).unwrap().is_some());
        assert_eq!(
            storage.blocks().get_height_by_hash(&child_hash).unwrap(),
            Some(2)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn announcer_is_probed_and_still_receives_getdata() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let tip_hash = [9u8; 32];
        let tip_header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [1u8; 32],
            timestamp: 1_700_000_000,
            bits: 0x207fffff,
            nonce: 0,
        };
        connect_tip(&storage, &tip_hash, &tip_header, 1);
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let nm = Arc::new(
            NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
                protocol,
                storage,
                Arc::new(MempoolManager::new()),
            ),
        );
        let listener_b = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_b = listener_b.local_addr().unwrap();
        let listener_a = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_a = listener_a.local_addr().unwrap();
        let announced = [0xabu8; 32];
        tokio::spawn(async move {
            let (mut sock, _) = listener_b.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let _ = timeout(Duration::from_secs(8), sock.read(&mut buf)).await;
        });
        let nm_read = Arc::clone(&nm);
        let reader_a = tokio::spawn(async move {
            let (mut sock, _) = listener_a.accept().await.unwrap();
            let mut saw_headers = false;
            let mut getdata = None;
            for _ in 0..6 {
                let frame = timeout(Duration::from_secs(5), read_frame(&mut sock))
                    .await
                    .expect("frame from A");
                match ProtocolParser::parse_message(&frame).unwrap() {
                    ProtocolMessage::GetHeaders(_) => {
                        saw_headers = true;
                        let unrelated = vec![
                            BlockHeader {
                                version: 1,
                                prev_block_hash: [1u8; 32],
                                merkle_root: [2u8; 32],
                                timestamp: 1_700_000_003,
                                bits: 0x207fffff,
                                nonce: 1,
                            },
                            BlockHeader {
                                version: 1,
                                prev_block_hash: [3u8; 32],
                                merkle_root: [4u8; 32],
                                timestamp: 1_700_000_004,
                                bits: 0x207fffff,
                                nonce: 2,
                            },
                        ];
                        assert!(nm_read.complete_headers_request(peer_a, unrelated));
                    }
                    ProtocolMessage::GetData(msg) => {
                        getdata = Some(msg);
                        break;
                    }
                    _ => {}
                }
            }
            assert!(saw_headers, "announcer received getheaders");
            getdata.expect("announcer received getdata")
        });
        nm.connect_to_peer(peer_b).await.unwrap();
        nm.connect_to_peer(peer_a).await.unwrap();
        let inv = ProtocolMessage::Inv(crate::network::protocol::InvMessage {
            inventory: vec![InventoryVector {
                inv_type: MSG_BLOCK,
                hash: announced,
            }],
        });
        let inv_bytes = ProtocolParser::serialize_message(&inv).unwrap();
        timeout(
            Duration::from_secs(5),
            nm.process_announced_block_inventory(&inv_bytes, peer_a),
        )
        .await
        .expect("inv handled")
        .unwrap();
        let getdata = timeout(Duration::from_secs(5), reader_a)
            .await
            .expect("reader")
            .unwrap();
        assert!(getdata.inventory.iter().any(|item| item.hash == announced));
    }
}

#[cfg(test)]
mod header_probe_order_tests {
    use super::{header_probe_timeout_secs, order_header_probe_peers};
    use std::net::SocketAddr;

    #[test]
    fn a_timed_out_peer_does_not_stay_first() {
        let stuck = "34.139.233.110:8333".parse().unwrap();
        let testnet4 = "1.2.3.4:48333".parse().unwrap();
        let other = "9.9.9.9:8333".parse().unwrap();
        let ordered = order_header_probe_peers(&[stuck, other, testnet4], 48333, &[stuck]);
        assert_eq!(ordered, vec![testnet4, other]);
    }

    #[test]
    fn every_recent_failure_is_skipped() {
        let a: SocketAddr = "1.1.1.1:8333".parse().unwrap();
        let b: SocketAddr = "2.2.2.2:8333".parse().unwrap();
        assert!(order_header_probe_peers(&[a, b], 8333, &[a, b]).is_empty());
    }

    #[test]
    fn wrong_port_probe_is_short() {
        assert_eq!(header_probe_timeout_secs(8333, 48333, 30), 3);
        assert_eq!(header_probe_timeout_secs(48333, 48333, 30), 30);
    }
}

#[cfg(test)]
mod high_bandwidth_tests {
    use super::{
        MAX_HEADER_ANNOUNCE_DISTANCE, MAX_HIGH_BANDWIDTH_PEERS, advance_known_header_height,
        header_announce_len, high_bandwidth_candidates, revoke_high_bandwidth,
    };
    use crate::network::protocol::SendCmpctMessage;
    use crate::network::transport::TransportType;
    use blvm_protocol::network::PeerState;
    use std::collections::HashMap;
    use std::net::SocketAddr;

    fn peer(addr: &str, handshake: bool) -> (SocketAddr, PeerState) {
        let addr = addr.parse().unwrap();
        let mut state = PeerState::new();
        state.handshake_complete = handshake;
        state.start_height = 50_000;
        (addr, state)
    }

    #[test]
    fn one_header_through_eight_is_inside_the_gap() {
        assert_eq!(header_announce_len(100, Some(99)), Some(1));
        assert_eq!(header_announce_len(100, Some(92)), Some(8));
        assert_eq!(MAX_HEADER_ANNOUNCE_DISTANCE, 8);
        assert_eq!(header_announce_len(100, Some(91)), None);
        assert_eq!(header_announce_len(100, Some(100)), None);
        assert_eq!(header_announce_len(100, None), None);
        assert_eq!(header_announce_len(100, Some(101)), None);
    }

    #[test]
    fn at_most_three_peers_are_offered_and_a_fourth_is_not() {
        let mut states = HashMap::new();
        for addr in [
            "4.4.4.4:8333",
            "1.1.1.1:8333",
            "3.3.3.3:8333",
            "2.2.2.2:8333",
        ] {
            let (addr, state) = peer(addr, true);
            states.insert(addr, state);
        }
        let (quiet, quiet_state) = peer("9.9.9.9:8333", false);
        states.insert(quiet, quiet_state);
        let picked = high_bandwidth_candidates(&states);
        assert_eq!(picked.len(), MAX_HIGH_BANDWIDTH_PEERS);
        let first: SocketAddr = "1.1.1.1:8333".parse().unwrap();
        let second: SocketAddr = "2.2.2.2:8333".parse().unwrap();
        let third: SocketAddr = "3.3.3.3:8333".parse().unwrap();
        assert_eq!(picked, vec![first, second, third]);
        for addr in &picked {
            states.get_mut(addr).unwrap().high_bandwidth = true;
        }
        assert!(high_bandwidth_candidates(&states).is_empty());
        assert!(revoke_high_bandwidth(states.get_mut(&picked[0]).unwrap()));
        let again = high_bandwidth_candidates(&states);
        assert_eq!(again.len(), 1);
        assert_ne!(again[0], picked[1]);
        assert_ne!(again[0], picked[2]);
        assert!(!states[&again[0]].high_bandwidth);
    }

    #[test]
    fn a_peer_that_was_never_selected_gets_no_revoke() {
        let mut state = PeerState::new();
        state.start_height = 100;
        assert!(!revoke_high_bandwidth(&mut state));
        assert!(!state.high_bandwidth);
        assert!(state.known_header_height.is_none());
        assert_eq!(state.start_height, 100);
    }

    #[test]
    fn known_header_height_advances_and_does_not_rewind() {
        let mut state = PeerState::new();
        state.start_height = 40;
        advance_known_header_height(&mut state, 10);
        advance_known_header_height(&mut state, 7);
        advance_known_header_height(&mut state, 12);
        assert_eq!(state.known_header_height, Some(12));
        assert_eq!(state.start_height, 40);
        assert_eq!(header_announce_len(13, state.known_header_height), Some(1));
        assert_eq!(header_announce_len(21, state.known_header_height), None);
    }

    #[test]
    fn announce_true_and_false_are_the_two_sendcmpct_flags() {
        let on = SendCmpctMessage::for_transport(TransportType::Tcp, true);
        let off = SendCmpctMessage::for_transport(TransportType::Tcp, false);
        assert_eq!(on.version, 1);
        assert_eq!(off.version, 1);
        assert_eq!(on.prefer_cmpct, 1);
        assert_eq!(off.prefer_cmpct, 0);
    }
}
