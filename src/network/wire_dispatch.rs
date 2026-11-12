//! Protocol message dispatch for incoming wire messages.
//!
//! Handles special cases and routes messages to the appropriate handlers.

#[cfg(feature = "spec-lock")]
use blvm_spec_lock::spec_locked;

use crate::network::NetworkMessage;
use crate::network::network_manager::NetworkManager;
use crate::network::protocol::{
    BlockMessage, HeadersMessage, ProtocolMessage, ProtocolParser, VersionMessage,
};
use crate::network::transport::TransportAddr;
use anyhow::Result;
use blvm_protocol::BlockHeader;
use blvm_protocol::ProtocolVersion;
use std::net::SocketAddr;
use tracing::{debug, info, warn};

/// Compact-block short ids, `getblocktxn` indexes, and `blocktxn` transactions share this ceiling.
const MAX_COMPACT_RELAY_ITEMS: usize = 50_000;

/// `fee_sats / vsize` meets the BIP133 filter. The comparison keeps the remainder below 1 sat/vB.
pub(crate) fn fee_rate_meets_filter(fee_sats: u64, vsize: u64, min_sat_per_kvb: u64) -> bool {
    vsize > 0 && fee_sats.saturating_mul(1000) / vsize >= min_sat_per_kvb
}

/// A mempool inv item is sent when the peer has no filter, or the cached basis meets it.
pub(crate) fn mempool_inv_includes(
    basis: Option<(u64, u64)>,
    min_sat_per_kvb: Option<u64>,
) -> bool {
    match min_sat_per_kvb {
        None => true,
        Some(min) => match basis {
            Some((fee_sats, vsize)) => fee_rate_meets_filter(fee_sats, vsize, min),
            None => false,
        },
    }
}

/// Split an inventory so each message stays within `max_len`.
pub(crate) fn chunk_inventory<T: Clone>(items: Vec<T>, max_len: usize) -> Vec<Vec<T>> {
    let max_len = max_len.max(1);
    if items.is_empty() {
        return vec![Vec::new()];
    }
    items.chunks(max_len).map(|chunk| chunk.to_vec()).collect()
}

fn block_hash_from_header(header: &BlockHeader) -> [u8; 32] {
    use crate::storage::hashing::double_sha256;
    let mut header_bytes = [0u8; 80];
    header_bytes[0..4].copy_from_slice(&(header.version as i32).to_le_bytes());
    header_bytes[4..36].copy_from_slice(&header.prev_block_hash);
    header_bytes[36..68].copy_from_slice(&header.merkle_root);
    header_bytes[68..72].copy_from_slice(&(header.timestamp as u32).to_le_bytes());
    header_bytes[72..76].copy_from_slice(&(header.bits as u32).to_le_bytes());
    header_bytes[76..80].copy_from_slice(&(header.nonce as u32).to_le_bytes());
    double_sha256(&header_bytes)
}

/// A short block `getdata` is a tip reply. Cooldown and the reputation step apply
/// only when the inventory is a historical range (more than one tip-follow window).
pub(crate) fn block_getdata_is_historical_download(block_count: usize) -> bool {
    block_count > crate::network::block_announced_sync::MAX_BLOCK_REQUESTS_PER_INV
}

/// Blocks refused by IBD protection, and the other inventory items that are still served.
pub(crate) fn split_ibd_refused_inventory(
    inventory: &[crate::network::protocol::InventoryVector],
) -> (
    Vec<crate::network::protocol::InventoryVector>,
    Vec<crate::network::protocol::InventoryVector>,
) {
    use crate::network::inventory::{MSG_BLOCK, MSG_WITNESS_BLOCK};
    let mut refused = Vec::new();
    let mut serve = Vec::new();
    for item in inventory {
        if item.inv_type == MSG_BLOCK || item.inv_type == MSG_WITNESS_BLOCK {
            refused.push(item.clone());
        } else {
            serve.push(item.clone());
        }
    }
    (refused, serve)
}

enum StagedCompact {
    Fetch(Vec<[u8; 32]>),
    Complete {
        assembly: crate::network::compact_blocks::CompactAssembly,
        expired: Vec<[u8; 32]>,
    },
    Holes {
        indices: Vec<u16>,
        expired: Vec<[u8; 32]>,
    },
}

impl NetworkManager {
    /// Handle an incoming `block` wire message (IBD GetData response or relay).
    ///
    /// Moves `block` + `witnesses` into pending IBD requests — no clone after parse.
    /// Unmatched blocks queue raw payload bytes from `data` for the main loop (relay path).
    pub(crate) async fn handle_block_wire_message(
        &self,
        peer_addr: SocketAddr,
        block_msg: BlockMessage,
        data: Vec<u8>,
    ) -> Result<()> {
        let handshake_complete = self
            .peer_states()
            .read()
            .await
            .get(&peer_addr)
            .is_some_and(|state| state.handshake_complete);
        if !handshake_complete {
            debug!("Peer {} sent a block before Verack — ignoring", peer_addr);
            return Ok(());
        }
        debug!(
            "Block message received from {} ({} bytes)",
            peer_addr,
            data.len()
        );
        let block_hash = block_hash_from_header(&block_msg.block.header);
        // Extract P2P payload once — IBD path keeps it for W5 wire-bytes store; relay queues it.
        let payload_len = u32::from_le_bytes([data[16], data[17], data[18], data[19]]) as usize;
        let wire_payload = if data.len() >= 24 + payload_len {
            Some(data[24..24 + payload_len].to_vec())
        } else {
            None
        };
        crate::node::parallel_ibd::body_dup::note_wire_block(data.len() as u64);
        if self.complete_block_request_with_wire(
            peer_addr,
            block_hash,
            block_msg.block,
            block_msg.witnesses,
            wire_payload.clone(),
        ) {
            debug!(
                "Block routed to pending request from {} (hash {})",
                peer_addr,
                hex::encode(block_hash)
            );
            return Ok(());
        }
        // Relay / post-IBD: queue the P2P block payload for the main loop.
        let Some(payload) = wire_payload else {
            warn!(
                "Block from {} truncated (frame {} bytes, payload {} bytes)",
                peer_addr,
                data.len(),
                payload_len
            );
            return Ok(());
        };
        let payload_bytes = payload.len();
        const UNSOLICITED_BLOCK_QUEUE_CAP: usize = 16;
        if self.pending_block_queue_len() >= UNSOLICITED_BLOCK_QUEUE_CAP {
            debug!(
                "Dropping unsolicited block from {} (hash {}): {} already queued",
                peer_addr,
                hex::encode(block_hash),
                UNSOLICITED_BLOCK_QUEUE_CAP
            );
            return Ok(());
        }
        self.queue_block(payload);
        if let Ok(mut inv) = self.inventory().lock() {
            inv.mark_fulfilled(&block_hash);
        }
        debug!(
            "Block from {} (hash {}) queued for main loop ({} payload bytes)",
            peer_addr,
            hex::encode(block_hash),
            payload_bytes
        );
        Ok(())
    }

    /// Handle Version message: update peer state and send VerAck (handshake).
    /// Orange Paper 10.2.1: On Version received, send VerAck. VerAck never sent before Version.
    ///
    /// The ordering invariant (VerAckSent ⟹ VersionReceived) is enforced by the handshake
    /// state machine: verack is only sent inside this handler, which only executes after a
    /// valid version message is received.  The invariant requires integration-level proof;
    /// Z3 body translation is not applicable to async network handlers.
    #[cfg_attr(feature = "spec-lock", spec_locked("10.2.1"))]
    #[cfg_attr(feature = "spec-lock", blvm_spec_lock::ensures(true))]
    pub(crate) async fn handle_version_received(
        &self,
        peer_addr: SocketAddr,
        version_msg: &VersionMessage,
    ) -> Result<()> {
        /// Minimum accepted protocol version (matches Bitcoin Core's MIN_PEER_PROTO_VERSION).
        const MIN_PEER_VERSION: i32 = 31800;

        // P-1: Reject peers running ancient versions.
        if version_msg.version < MIN_PEER_VERSION {
            warn!(
                "Peer {} sent Version {} which is below minimum {} — disconnecting",
                peer_addr, version_msg.version, MIN_PEER_VERSION
            );
            return self
                .disconnect_for_protocol_violation(peer_addr, "version below minimum", false)
                .await;
        }

        // P-2: Self-connection detection.  If the peer's nonce matches one we sent,
        //       we are connected to ourselves.
        if self
            .local_version_nonces
            .lock()
            .unwrap()
            .contains(&version_msg.nonce)
        {
            warn!(
                "Peer {} echoed our own version nonce — self-connection, disconnecting",
                peer_addr
            );
            return self
                .disconnect_for_protocol_violation(peer_addr, "self-connection detected", false)
                .await;
        }

        // P-3: Reject duplicate Version (peer already completed version exchange).
        {
            let peer_states = self.peer_states().read().await;
            if let Some(state) = peer_states.get(&peer_addr) {
                if state.version > 0 {
                    warn!("Peer {} sent Version twice — disconnecting", peer_addr);
                    drop(peer_states);
                    return self
                        .disconnect_for_protocol_violation(
                            peer_addr,
                            "duplicate version message",
                            false,
                        )
                        .await;
                }
            }
        }

        if version_msg.user_agent.len() > crate::network::protocol::MAX_SUBVERSION_LENGTH {
            warn!(
                "Peer {} version user agent is {} bytes",
                peer_addr,
                version_msg.user_agent.len()
            );
            return self
                .disconnect_for_protocol_violation(
                    peer_addr,
                    "version user agent exceeds 256",
                    true,
                )
                .await;
        }

        let mut pm = self.peer_manager_mutex().lock().await;
        let transport_addr = pm.find_transport_addr_by_socket(peer_addr);
        let transport_addr_for_verack = transport_addr.clone();
        let block_relay_only = transport_addr
            .as_ref()
            .and_then(|addr| pm.get_peer(addr))
            .is_some_and(|peer| {
                peer.outbound_role() == Some(crate::network::peer::OutboundRole::BlockRelayOnly)
            });

        if let Some(transport_addr) = transport_addr {
            if let Some(peer) = pm.get_peer_mut(&transport_addr) {
                peer.set_version(version_msg.version as u32);
                peer.set_services(version_msg.services);
                peer.set_user_agent(version_msg.user_agent.clone());
                peer.set_start_height(version_msg.start_height);
                peer.set_version_time(version_msg.timestamp);
                debug!(
                    "Updated peer {} with version={}, services={}, user_agent={}, start_height={}",
                    peer_addr,
                    version_msg.version,
                    version_msg.services,
                    version_msg.user_agent,
                    version_msg.start_height
                );
            }
        }
        drop(pm);

        // Mirror version into peer_states so dispatch_protocol_message's pre-handshake guard
        // allows subsequent messages (Verack, etc.) from this peer. The guard checks
        // peer_states[peer_addr].version > 0; without this the Verack the remote sends
        // immediately after their Version would always be dropped as "before Version".
        {
            let mut peer_states = self.peer_states().write().await;
            let state = peer_states
                .entry(peer_addr)
                .or_insert_with(blvm_protocol::network::PeerState::new);
            state.version = version_msg.version;
            state.relay_tx = version_msg.relay && !block_relay_only;
        }

        if let Some(ref transport_addr) = transport_addr_for_verack {
            if let Ok(packages) = ProtocolParser::serialize_message(&ProtocolMessage::SendPackages(
                crate::network::package_messages::PACKAGE_VERSION_LOCAL,
            )) {
                let _ = self
                    .send_to_peer_by_transport(transport_addr.clone(), packages)
                    .await;
            }
            match ProtocolParser::serialize_message(&ProtocolMessage::Verack) {
                Ok(verack_msg) => {
                    if let Err(e) = self
                        .send_to_peer_by_transport(transport_addr.clone(), verack_msg)
                        .await
                    {
                        warn!("Failed to send VerAck to {:?}: {}", transport_addr, e);
                    } else {
                        debug!("Sent VerAck to {:?} (handshake completing)", transport_addr);
                    }
                }
                Err(e) => {
                    warn!("Failed to serialize VerAck for {:?}: {}", transport_addr, e);
                }
            }
            self.publish_companion_udp_peer_after_handshake(transport_addr, version_msg)
                .await;
        }
        Ok(())
    }

    fn queue_completed_compact(
        &self,
        assembly: &crate::network::compact_blocks::CompactAssembly,
    ) -> Result<()> {
        use crate::network::compact_blocks::completed_compact_block;
        let (block, witnesses) = completed_compact_block(assembly)?;
        let bytes =
            blvm_protocol::serialization::serialize_block_with_witnesses(&block, &witnesses, true);
        self.queue_block(bytes);
        Ok(())
    }

    /// Match a compact block against the pool and ask for the holes.
    /// Over the pending cap, the block is requested whole.
    pub(crate) async fn handle_cmpctblock(
        &self,
        peer_addr: SocketAddr,
        compact: &blvm_protocol::bip152::CompactBlock,
    ) -> Result<()> {
        use crate::network::compact_blocks::MAX_COMPACT_BLOCK_TXS;

        let block_hash = block_hash_from_header(&compact.header);
        let total = compact
            .short_ids
            .len()
            .saturating_add(compact.prefilled_txs.len());
        if total > MAX_COMPACT_BLOCK_TXS {
            self.send_witness_block_getdata(peer_addr, block_hash).await;
            return Ok(());
        }

        let matches = self
            .mempool_manager()
            .map(|mm| {
                mm.transactions_for_short_ids(&compact.header, compact.nonce, &compact.short_ids)
            })
            .unwrap_or_default();

        let staged = self.stage_compact_block(block_hash, compact, &matches)?;
        match staged {
            StagedCompact::Fetch(hashes) => {
                for hash in hashes {
                    self.send_witness_block_getdata(peer_addr, hash).await;
                }
            }
            StagedCompact::Complete { assembly, expired } => {
                for hash in expired {
                    self.send_witness_block_getdata(peer_addr, hash).await;
                }
                self.queue_completed_compact(&assembly)?;
            }
            StagedCompact::Holes { indices, expired } => {
                for hash in expired {
                    self.send_witness_block_getdata(peer_addr, hash).await;
                }
                let msg =
                    ProtocolMessage::GetBlockTxn(crate::network::protocol::GetBlockTxnMessage {
                        block_hash,
                        indices,
                    });
                if let Ok(wire) = ProtocolParser::serialize_message(&msg) {
                    let _ = self.send_to_peer(peer_addr, wire).await;
                }
            }
        }
        Ok(())
    }

    /// Lock the pending table, expire old assemblies, and record this one.
    /// The lock is released before the caller writes to the peer.
    fn stage_compact_block(
        &self,
        block_hash: [u8; 32],
        compact: &blvm_protocol::bip152::CompactBlock,
        matches: &std::collections::HashMap<
            blvm_protocol::Hash,
            (
                blvm_protocol::Transaction,
                Option<Vec<blvm_protocol::segwit::Witness>>,
            ),
        >,
    ) -> Result<StagedCompact> {
        use crate::network::compact_blocks::{
            CompactBudget, CompactDecision, assembly_retained_bytes, decide_compact_block,
        };
        use std::time::Instant;

        let mut pending = match self.pending_compact.lock() {
            Ok(pending) => pending,
            Err(_) => return Ok(StagedCompact::Fetch(vec![block_hash])),
        };
        let now = Instant::now();
        let mut expired = Vec::new();
        pending.retain(|hash, assembly| {
            if assembly.expired_at(now) {
                expired.push(*hash);
                false
            } else {
                true
            }
        });
        let same_hash = pending.contains_key(&block_hash);
        let assemblies = pending.len() - usize::from(same_hash);
        let retained_bytes = pending
            .iter()
            .filter(|(hash, _)| **hash != block_hash)
            .map(|(_, assembly)| assembly_retained_bytes(assembly))
            .sum();
        let staged = match decide_compact_block(
            compact,
            matches,
            CompactBudget {
                assemblies,
                retained_bytes,
                same_hash,
            },
        )? {
            CompactDecision::FullBlock => {
                expired.push(block_hash);
                StagedCompact::Fetch(expired)
            }
            CompactDecision::KeepCurrent => StagedCompact::Fetch(expired),
            CompactDecision::Hold(mut assembly) if assembly.missing.is_empty() => {
                if same_hash {
                    if let Some(previous) = pending.get(&block_hash) {
                        assembly.started = previous.started;
                    }
                }
                StagedCompact::Complete { assembly, expired }
            }
            CompactDecision::Hold(mut assembly) => {
                if same_hash {
                    if let Some(previous) = pending.get(&block_hash) {
                        assembly.started = previous.started;
                    }
                }
                let indices = assembly
                    .missing
                    .iter()
                    .map(|index| {
                        u16::try_from(*index).expect("decide_compact_block rejects a non-u16 index")
                    })
                    .collect::<Vec<_>>();
                pending.insert(block_hash, assembly);
                StagedCompact::Holes { indices, expired }
            }
        };
        Ok(staged)
    }

    async fn send_witness_block_getdata(&self, peer_addr: SocketAddr, block_hash: [u8; 32]) {
        use crate::network::inventory::MSG_WITNESS_BLOCK;
        use crate::network::protocol::{GetDataMessage, InventoryVector};
        let msg = ProtocolMessage::GetData(GetDataMessage {
            inventory: vec![InventoryVector {
                inv_type: MSG_WITNESS_BLOCK,
                hash: block_hash,
            }],
        });
        if let Ok(wire) = ProtocolParser::serialize_message(&msg) {
            let _ = self.send_to_peer(peer_addr, wire).await;
        }
    }

    /// Fill a pending compact block from `blocktxn`, including witness stacks.
    pub(crate) fn handle_blocktxn_fill(
        &self,
        msg: &crate::network::protocol::BlockTxnMessage,
    ) -> Result<()> {
        use crate::network::compact_blocks::apply_blocktxn;
        let Some(mut assembly) = self
            .pending_compact
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(&msg.block_hash))
        else {
            return Ok(());
        };
        apply_blocktxn(&mut assembly, &msg.transactions, msg.witnesses.as_deref())?;
        self.queue_completed_compact(&assembly)
    }

    /// Answer `getblocktxn`. An index past the block is an error and sends nothing.
    pub(crate) async fn handle_getblocktxn(
        &self,
        peer_addr: SocketAddr,
        msg: &crate::network::protocol::GetBlockTxnMessage,
    ) -> Result<()> {
        let Some(storage) = self.storage() else {
            return Ok(());
        };
        let Some(block) = storage.blocks().get_block(&msg.block_hash).ok().flatten() else {
            return Ok(());
        };
        if msg
            .indices
            .iter()
            .any(|index| *index as usize >= block.transactions.len())
        {
            anyhow::bail!("getblocktxn index past end of block");
        }
        let transactions: Vec<_> = msg
            .indices
            .iter()
            .map(|index| block.transactions[*index as usize].clone())
            .collect();
        let witnesses = storage
            .blocks()
            .get_witness(&msg.block_hash)
            .ok()
            .flatten()
            .map(|all| {
                msg.indices
                    .iter()
                    .map(|index| all.get(*index as usize).cloned().unwrap_or_default())
                    .collect::<Vec<_>>()
            });
        let reply = ProtocolMessage::BlockTxn(crate::network::protocol::BlockTxnMessage {
            block_hash: msg.block_hash,
            transactions,
            witnesses,
        });
        if let Ok(wire) = ProtocolParser::serialize_message(&reply) {
            let _ = self.send_to_peer(peer_addr, wire).await;
        }
        Ok(())
    }

    fn admit_announced_txs(
        &self,
        peer_addr: SocketAddr,
        hashes: &[blvm_protocol::Hash],
    ) -> Vec<blvm_protocol::Hash> {
        let mut tracker = self
            .tx_requests()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        tracker.admit(peer_addr, hashes, std::time::Instant::now())
    }

    fn finish_tx_request(
        &self,
        peer_addr: SocketAddr,
        hash: blvm_protocol::Hash,
    ) -> Vec<blvm_protocol::Hash> {
        let mut tracker = self
            .tx_requests()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        tracker.complete(peer_addr, hash, std::time::Instant::now())
    }

    async fn request_witness_txs(&self, peer_addr: SocketAddr, hashes: &[blvm_protocol::Hash]) {
        if hashes.is_empty() {
            return;
        }
        let inv_type = {
            let states = self.peer_states().read().await;
            crate::network::inventory::tx_announce_inv_type(
                states
                    .get(&peer_addr)
                    .is_some_and(|state| state.wtxid_relay),
            )
        };
        let inventory = hashes
            .iter()
            .map(|hash| crate::network::protocol::InventoryVector {
                inv_type,
                hash: *hash,
            })
            .collect();
        let getdata =
            ProtocolMessage::GetData(crate::network::protocol::GetDataMessage { inventory });
        if let Ok(wire) = ProtocolParser::serialize_message(&getdata) {
            let _ = self.send_to_peer(peer_addr, wire).await;
        }
    }

    /// Dispatch protocol message to handlers or route to message queue.
    /// Returns Ok(()) when message is fully handled; Err when peer should be disconnected.
    pub(crate) async fn dispatch_protocol_message(
        &self,
        peer_addr: SocketAddr,
        parsed: &ProtocolMessage,
        data: Vec<u8>,
    ) -> Result<()> {
        // Pre-handshake guard: reject everything except Version before the peer
        // has identified itself.  Serving data (headers, inv, addr, …) to an
        // unversioned peer leaks information and bypasses per-peer limits.
        let (version_received, handshake_complete) = {
            let peer_states = self.peer_states().read().await;
            match peer_states.get(&peer_addr) {
                Some(state) => (state.version > 0, state.handshake_complete),
                None => (false, false),
            }
        };

        if !version_received {
            match parsed {
                ProtocolMessage::Version(_) => {
                    // Allow — Version is the first required handshake message.
                }
                ProtocolMessage::Verack => {
                    // A Verack before we've received the peer's Version is a
                    // protocol violation; drop it silently.
                    debug!("Peer {} sent Verack before Version — ignoring", peer_addr);
                    return Ok(());
                }
                _ => {
                    // Common during fast reconnects (e.g. Inv sent before our Version
                    // was processed). Not a serious violation — log at debug.
                    debug!(
                        "Peer {} sent {:?} before Version — ignoring",
                        peer_addr,
                        std::mem::discriminant(parsed)
                    );
                    return Ok(());
                }
            }
        }

        if matches!(
            parsed,
            ProtocolMessage::Tx(_) | ProtocolMessage::Block(_) | ProtocolMessage::GetData(_)
        ) && !handshake_complete
        {
            debug!(
                "Peer {} sent {:?} before Verack — ignoring",
                peer_addr,
                std::mem::discriminant(parsed)
            );
            return Ok(());
        }

        if version_received && !self.allow_peer_message(peer_addr).await {
            debug!(
                "Peer {} exceeded the configured message rate limit",
                peer_addr
            );
            return Ok(());
        }

        match parsed {
            ProtocolMessage::Version(version_msg) => {
                self.handle_version_received(peer_addr, version_msg).await?;
            }
            ProtocolMessage::Ping(ping_msg) => {
                use crate::network::protocol::PongMessage;
                let pong_msg = ProtocolMessage::Pong(PongMessage {
                    nonce: ping_msg.nonce,
                });
                match ProtocolParser::serialize_message(&pong_msg) {
                    Ok(pong_wire) => {
                        let pm = self.peer_manager_mutex().lock().await;
                        let transport_addr = pm.find_transport_addr_by_socket(peer_addr);
                        drop(pm);
                        if let Some(transport_addr) = transport_addr {
                            if let Err(e) = self
                                .send_to_peer_by_transport(transport_addr.clone(), pong_wire)
                                .await
                            {
                                warn!("Failed to send Pong to {}: {}", peer_addr, e);
                            } else {
                                debug!("Sent Pong to {} (nonce={})", peer_addr, ping_msg.nonce);
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to serialize Pong for {}: {}", peer_addr, e);
                    }
                }
                return Ok(());
            }
            ProtocolMessage::Pong(pong_msg) => {
                let mut pm = self.peer_manager_mutex().lock().await;
                let transport_addr = pm.find_transport_addr_by_socket(peer_addr).or_else(|| {
                    pm.peers()
                        .iter()
                        .find(|(addr, _)| match addr {
                            TransportAddr::Tcp(sock) => sock == &peer_addr,
                            #[cfg(feature = "quinn")]
                            TransportAddr::Quinn(sock) => sock == &peer_addr,
                            #[cfg(feature = "iroh")]
                            TransportAddr::Iroh(_) => false,
                        })
                        .map(|(addr, _)| addr.clone())
                });

                if let Some(addr) = transport_addr {
                    if let Some(peer) = pm.get_peer_mut(&addr) {
                        if !peer.record_pong_received(pong_msg.nonce) {
                            debug!("Received pong with non-matching nonce from {}", peer_addr);
                        } else {
                            debug!(
                                "Received valid pong from {} (nonce={})",
                                peer_addr, pong_msg.nonce
                            );
                        }
                    }
                }
            }
            ProtocolMessage::Tx(tx_msg) => {
                let txid = blvm_protocol::block::calculate_tx_id(&tx_msg.transaction);
                let follow = self.finish_tx_request(peer_addr, txid);
                self.request_witness_txs(peer_addr, &follow).await;
                let _ = self
                    .peer_tx()
                    .send(NetworkMessage::TransactionReceived(data));
                return Ok(());
            }
            ProtocolMessage::FeeFilter(fee_filter) => {
                // BIP133 feerate is satoshis per 1000 virtual bytes.
                let mut states = self.peer_states().write().await;
                if let Some(state) = states.get_mut(&peer_addr) {
                    state.min_fee_rate = Some(fee_filter.feerate);
                }
                return Ok(());
            }
            ProtocolMessage::GetAddr => {
                self.handle_get_addr(peer_addr).await?;
                return Ok(());
            }
            ProtocolMessage::Addr(msg) => {
                self.handle_addr(peer_addr, msg.clone()).await?;
                return Ok(());
            }
            ProtocolMessage::AddrV2(addrv2) => {
                self.handle_addr_v2(peer_addr, addrv2.clone()).await?;
                return Ok(());
            }
            ProtocolMessage::GetHeaders(getheaders) => {
                if getheaders.block_locator_hashes.len() > crate::network::protocol::MAX_LOCATOR_SZ
                {
                    return self
                        .disconnect_for_protocol_violation(
                            peer_addr,
                            "getheaders locator exceeds 101",
                            true,
                        )
                        .await;
                }
                if let Some(storage) = self.storage().as_ref() {
                    let max = self.protocol_limits().max_headers_results.max(1);
                    match storage.blocks().build_headers_response(
                        &getheaders.block_locator_hashes,
                        &getheaders.hash_stop,
                        max,
                    ) {
                        Ok(headers) => {
                            debug!(
                                "GetHeaders from {}: sending {} header(s) (locator_len={})",
                                peer_addr,
                                headers.len(),
                                getheaders.block_locator_hashes.len()
                            );
                            let msg = ProtocolMessage::Headers(HeadersMessage {
                                headers: headers.clone(),
                            });
                            if let Ok(wire) = ProtocolParser::serialize_message(&msg) {
                                if let Err(e) = self.send_to_peer(peer_addr, wire).await {
                                    warn!("Failed to send Headers to {}: {}", peer_addr, e);
                                } else if let Some(last) = headers.last() {
                                    let last_hash = blvm_consensus::block::block_header_hash(last);
                                    if let Ok(Some(height)) =
                                        storage.blocks().get_height_by_hash(&last_hash)
                                    {
                                        self.note_known_header_height(peer_addr, height).await;
                                    }
                                }
                            } else {
                                warn!("Failed to serialize Headers for {}", peer_addr);
                            }
                        }
                        Err(e) => warn!("GetHeaders: build_headers_response failed: {}", e),
                    }
                } else {
                    debug!("GetHeaders from {}: no storage, not replying", peer_addr);
                }
                return Ok(());
            }
            ProtocolMessage::GetData(getdata) => {
                let max_inv = self.protocol_limits().max_inv_sz;
                if getdata.inventory.len() > max_inv {
                    warn!(
                        "getdata message size = {} exceeds max_inv_sz ({}), disconnecting peer {}",
                        getdata.inventory.len(),
                        max_inv,
                        peer_addr
                    );
                    return self
                        .disconnect_for_protocol_violation(
                            peer_addr,
                            "getdata message size exceeded",
                            true,
                        )
                        .await;
                }

                use crate::network::inventory::{MSG_BLOCK, MSG_WITNESS_BLOCK};
                let block_count = getdata
                    .inventory
                    .iter()
                    .filter(|inv| inv.inv_type == MSG_BLOCK || inv.inv_type == MSG_WITNESS_BLOCK)
                    .count();

                let mut refused_blocks_only = None;
                if block_getdata_is_historical_download(block_count) {
                    match self.ibd_protection().can_serve_ibd(peer_addr).await {
                        Ok(true) => {
                            self.ibd_protection().start_ibd_serving(peer_addr).await;
                            debug!("IBD protection: Allowing block request from {}", peer_addr);
                        }
                        Ok(false) => {
                            warn!(
                                "IBD protection: Rejecting block request from {} (bandwidth limit exceeded or cooldown active)",
                                peer_addr
                            );
                            use crate::network::protocol::{
                                NotFoundMessage, ProtocolMessage, ProtocolParser,
                            };
                            let (refused, serve) = split_ibd_refused_inventory(&getdata.inventory);
                            if !refused.is_empty() {
                                let notfound = NotFoundMessage { inventory: refused };
                                if let Ok(wire_msg) = ProtocolParser::serialize_message(
                                    &ProtocolMessage::NotFound(notfound),
                                ) {
                                    if let Err(e) = self.send_to_peer(peer_addr, wire_msg).await {
                                        warn!(
                                            "Failed to send NotFound message to {}: {}",
                                            peer_addr, e
                                        );
                                    }
                                }
                            }
                            if serve.is_empty() {
                                return Ok(());
                            }
                            refused_blocks_only =
                                Some(crate::network::protocol::GetDataMessage { inventory: serve });
                        }
                        Err(e) => {
                            warn!("IBD protection check failed for {}: {}", peer_addr, e);
                        }
                    }
                }

                let protocol_version = self
                    .protocol_engine()
                    .map(|e| e.get_protocol_version())
                    .unwrap_or(ProtocolVersion::BitcoinV1);

                let serve_result = if let Some(message) = refused_blocks_only.as_ref() {
                    self.serve_getdata_request(peer_addr, message, protocol_version)
                        .await
                } else {
                    self.serve_getdata_request(peer_addr, getdata, protocol_version)
                        .await
                };
                if block_getdata_is_historical_download(block_count) {
                    // Pair start_ibd_serving — without this, concurrent count leaks to the cap
                    // and Mode T / archive getdata is rejected mid-soak.
                    self.ibd_protection().stop_ibd_serving(peer_addr).await;
                }
                if let Err(e) = serve_result {
                    warn!("getdata: failed to serve peer {}: {}", peer_addr, e);
                }
                return Ok(());
            }
            ProtocolMessage::SendPkgTxn(_) => {
                let _ = self
                    .peer_tx()
                    .send(NetworkMessage::SendPkgTxnReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::PkgTxn(_) => {
                let _ = self
                    .peer_tx()
                    .send(NetworkMessage::PkgTxnReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::GetCfilters(_) => {
                let _ = self
                    .peer_tx()
                    .send(NetworkMessage::GetCfiltersReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::GetCfheaders(_) => {
                let _ = self
                    .peer_tx()
                    .send(NetworkMessage::GetCfheadersReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::GetCfcheckpt(_) => {
                let _ = self
                    .peer_tx()
                    .send(NetworkMessage::GetCfcheckptReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::Inv(inv_msg) => {
                let max_inv = self.protocol_limits().max_inv_sz;
                if inv_msg.inventory.len() > max_inv {
                    warn!(
                        "inv message size = {} exceeds max_inv_sz ({}), disconnecting peer {}",
                        inv_msg.inventory.len(),
                        max_inv,
                        peer_addr
                    );
                    return self
                        .disconnect_for_protocol_violation(
                            peer_addr,
                            "inv message size exceeded",
                            true,
                        )
                        .await;
                }
                let wtxid_relay = self
                    .peer_states()
                    .read()
                    .await
                    .get(&peer_addr)
                    .is_some_and(|state| state.wtxid_relay);
                let mut announced = Vec::new();
                for inv in &inv_msg.inventory {
                    let transaction = inv.inv_type == crate::network::inventory::MSG_TX
                        || inv.inv_type == crate::network::inventory::MSG_WITNESS_TX
                        || (wtxid_relay && inv.inv_type == crate::network::inventory::MSG_WTX);
                    if !transaction {
                        continue;
                    }
                    let in_pool = self
                        .mempool_manager()
                        .and_then(|pool| pool.get_transaction(&inv.hash))
                        .is_some();
                    let in_chain = self
                        .storage()
                        .as_ref()
                        .and_then(|storage| storage.transactions().has_transaction(&inv.hash).ok())
                        .unwrap_or(false);
                    if !in_pool && !in_chain {
                        announced.push(inv.hash);
                    }
                }
                let wanted = self.admit_announced_txs(peer_addr, &announced);
                self.request_witness_txs(peer_addr, &wanted).await;
                let _ = self
                    .peer_tx()
                    .send(NetworkMessage::InventoryReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::GetModule(_) => {
                let _ = self
                    .peer_tx()
                    .send(NetworkMessage::GetModuleReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::Module(_) => {
                let _ = self
                    .peer_tx()
                    .send(NetworkMessage::ModuleReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::GetModuleByHash(_) => {
                let _ = self
                    .peer_tx()
                    .send(NetworkMessage::GetModuleByHashReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::ModuleByHash(_) => {
                let _ = self
                    .peer_tx()
                    .send(NetworkMessage::ModuleByHashReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::GetModuleList(_) => {
                let _ = self
                    .peer_tx()
                    .send(NetworkMessage::GetModuleListReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::ModuleList(_) => {
                let _ = self
                    .peer_tx()
                    .send(NetworkMessage::ModuleListReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::Headers(headers_msg) => {
                let max_headers = self.protocol_limits().max_headers_results;
                if crate::config::headers_message_exceeds_limit(
                    headers_msg.headers.len(),
                    max_headers,
                ) {
                    warn!(
                        "headers message size = {} exceeds max_headers_results ({}), disconnecting peer {}",
                        headers_msg.headers.len(),
                        max_headers,
                        peer_addr
                    );
                    return self
                        .disconnect_for_protocol_violation(
                            peer_addr,
                            "headers message size exceeded",
                            true,
                        )
                        .await;
                }

                let headers = headers_msg.headers.clone();
                if self.complete_headers_request(peer_addr, headers) {
                    debug!(
                        "Routed Headers response to pending request from {}",
                        peer_addr
                    );
                    return Ok(());
                }
                // No outstanding getheaders. A header that connects to the header tip
                // extends the header chain. A gap is left for the next locator request.
                if let Some(storage) = self.storage().as_ref() {
                    let block_tip = storage.chain().get_tip_hash().ok().flatten();
                    let block_height = storage.chain().get_height().ok().flatten().unwrap_or(0);
                    let header_height = storage
                        .blocks()
                        .highest_stored_height()
                        .ok()
                        .flatten()
                        .unwrap_or(block_height);
                    let link_hash = if header_height > block_height {
                        storage
                            .blocks()
                            .get_hash_by_height(header_height)
                            .ok()
                            .flatten()
                    } else {
                        block_tip
                    };
                    if let Some(mut prev) = link_hash {
                        let mut height = header_height.max(block_height);
                        let network = crate::network::header_work::network_for_header_work(
                            self.protocol_engine()
                                .map(|engine| engine.get_protocol_version()),
                            storage.chain().consensus_network()?,
                        );
                        let mut pending: Vec<(u64, u64, u64)> = Vec::new();
                        let mut batch = Vec::new();
                        for header in &headers_msg.headers {
                            let next_height = height.saturating_add(1);
                            let meets = {
                                let stored = |h: u64| {
                                    let hash =
                                        storage.blocks().get_hash_by_height(h).ok().flatten()?;
                                    let found =
                                        storage.blocks().get_header(&hash).ok().flatten()?;
                                    Some((found.bits, found.timestamp))
                                };
                                let ancestor = crate::network::header_work::ancestor_seeing_pending(
                                    &pending, &stored,
                                );
                                crate::network::header_work::header_meets_required_work(
                                    header,
                                    next_height,
                                    network,
                                    &ancestor,
                                )
                            };
                            let version_ok = blvm_consensus::bip_validation::check_bip90_network(
                                header.version,
                                next_height,
                                network,
                            )
                            .unwrap_or(false);
                            if !version_ok
                                || header.timestamp == 0
                                || header.prev_block_hash != prev
                                || !meets
                            {
                                break;
                            }
                            pending.push((next_height, header.bits, header.timestamp));
                            height = next_height;
                            let hash = blvm_consensus::block::block_header_hash(header);
                            batch.push((hash, header.clone(), height));
                            prev = hash;
                        }
                        if !batch.is_empty() {
                            if let Err(e) = storage.blocks().store_headers_batch(&batch) {
                                warn!("unsolicited headers from {}: {}", peer_addr, e);
                            } else {
                                self.note_known_header_height(peer_addr, height).await;
                                self.note_block_announcement(peer_addr).await;
                            }
                        }
                    }
                }
                return Ok(());
            }
            ProtocolMessage::Block(_) => {
                warn!(
                    "Block message reached generic dispatch from {} — should use handle_block_wire_message",
                    peer_addr
                );
                return Ok(());
            }
            ProtocolMessage::CmpctBlock(cmpct_msg) => {
                if cmpct_msg.compact_block.short_ids.len() > MAX_COMPACT_RELAY_ITEMS {
                    warn!(
                        "Invalid compact block: too many short IDs ({}) from {}",
                        cmpct_msg.compact_block.short_ids.len(),
                        peer_addr
                    );
                    return self
                        .disconnect_for_protocol_violation(
                            peer_addr,
                            "compact block has too many short ids",
                            true,
                        )
                        .await;
                }
                self.handle_cmpctblock(peer_addr, &cmpct_msg.compact_block)
                    .await?;
                return Ok(());
            }
            ProtocolMessage::GetBlockTxn(getblocktxn_msg) => {
                if getblocktxn_msg.indices.len() > MAX_COMPACT_RELAY_ITEMS {
                    warn!(
                        "GetBlockTxn with too many indices ({}) from {}",
                        getblocktxn_msg.indices.len(),
                        peer_addr
                    );
                    return self
                        .disconnect_for_protocol_violation(
                            peer_addr,
                            "getblocktxn has too many indices",
                            true,
                        )
                        .await;
                }
                self.handle_getblocktxn(peer_addr, getblocktxn_msg).await?;
                return Ok(());
            }
            ProtocolMessage::BlockTxn(blocktxn_msg) => {
                if blocktxn_msg.transactions.len() > MAX_COMPACT_RELAY_ITEMS {
                    warn!(
                        "BlockTxn with too many transactions ({}) from {}",
                        blocktxn_msg.transactions.len(),
                        peer_addr
                    );
                    let _ =
                        self.peer_tx()
                            .send(NetworkMessage::PeerDisconnected(TransportAddr::Tcp(
                                peer_addr,
                            )));
                    return Err(anyhow::anyhow!("BlockTxn with too many transactions"));
                }
                self.handle_blocktxn_fill(blocktxn_msg)?;
                return Ok(());
            }
            #[cfg(feature = "utxo-commitments")]
            ProtocolMessage::UTXOSet(_) => {
                let _ = self
                    .peer_tx()
                    .send(NetworkMessage::UTXOSetReceived(data, peer_addr));
                return Ok(());
            }
            #[cfg(feature = "utxo-commitments")]
            ProtocolMessage::FilteredBlock(_) => {
                let _ = self
                    .peer_tx()
                    .send(NetworkMessage::FilteredBlockReceived(data, peer_addr));
                return Ok(());
            }
            #[cfg(feature = "utxo-commitments")]
            ProtocolMessage::GetFilteredBlock(_) => {
                let _ = self
                    .peer_tx()
                    .send(NetworkMessage::GetFilteredBlockReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::GetBlocks(getblocks) => {
                if getblocks.block_locator_hashes.len() > crate::network::protocol::MAX_LOCATOR_SZ {
                    return self
                        .disconnect_for_protocol_violation(
                            peer_addr,
                            "getblocks locator exceeds 101",
                            true,
                        )
                        .await;
                }
                if let Some(storage) = self.storage().as_ref() {
                    use crate::network::inventory::{MSG_BLOCK, MSG_WITNESS_BLOCK};
                    use blvm_consensus::block::block_header_hash;

                    const MAX_GETBLOCKS_INV: usize = 500;
                    match storage.blocks().build_headers_response(
                        &getblocks.block_locator_hashes,
                        &getblocks.hash_stop,
                        MAX_GETBLOCKS_INV,
                    ) {
                        Ok(headers) => {
                            let mut inventory = Vec::new();
                            for header in headers {
                                let hash = block_header_hash(&header);
                                if !storage.blocks().has_block_body(&hash).unwrap_or(false) {
                                    break;
                                }
                                let witnessed = storage
                                    .blocks()
                                    .get_witness(&hash)
                                    .ok()
                                    .flatten()
                                    .is_some_and(|txs| {
                                        txs.iter()
                                            .any(|tx| tx.iter().any(|stack| !stack.is_empty()))
                                    });
                                inventory.push(crate::network::protocol::InventoryVector {
                                    inv_type: if witnessed {
                                        MSG_WITNESS_BLOCK
                                    } else {
                                        MSG_BLOCK
                                    },
                                    hash,
                                });
                            }
                            let inv_msg =
                                ProtocolMessage::Inv(crate::network::protocol::InvMessage {
                                    inventory,
                                });
                            if let Ok(wire) = ProtocolParser::serialize_message(&inv_msg) {
                                let _ = self.send_to_peer(peer_addr, wire).await;
                            }
                        }
                        Err(e) => {
                            warn!("GetBlocks: build_headers_response failed: {}", e);
                        }
                    }
                }
                return Ok(());
            }
            ProtocolMessage::MemPool => {
                // BIP35: respond with an Inv listing all txids currently in our mempool.
                if self.outbound_role_is_block_only(peer_addr).await {
                    return Ok(());
                }
                if let Some(mm) = self.mempool_manager() {
                    let min_sat_per_kvb = {
                        let states = self.peer_states().read().await;
                        states.get(&peer_addr).and_then(|state| state.min_fee_rate)
                    };
                    let inv_type = {
                        let states = self.peer_states().read().await;
                        crate::network::inventory::tx_announce_inv_type(
                            states
                                .get(&peer_addr)
                                .is_some_and(|state| state.wtxid_relay),
                        )
                    };
                    let inventory: Vec<crate::network::protocol::InventoryVector> = mm
                        .pooled_fee_basis()
                        .into_iter()
                        .filter_map(|(hash, basis)| {
                            if !mempool_inv_includes(basis, min_sat_per_kvb) {
                                return None;
                            }
                            Some(crate::network::protocol::InventoryVector { inv_type, hash })
                        })
                        .collect();
                    let max_inv = self.protocol_limits().max_inv_sz.max(1);
                    for inventory in chunk_inventory(inventory, max_inv) {
                        let inv_msg = ProtocolMessage::Inv(crate::network::protocol::InvMessage {
                            inventory,
                        });
                        if let Ok(wire) = ProtocolParser::serialize_message(&inv_msg) {
                            let _ = self.send_to_peer(peer_addr, wire).await;
                        }
                    }
                }
                return Ok(());
            }
            ProtocolMessage::Verack => {
                // Mark handshake complete on the peer state.
                let mut peer_states = self.peer_states().write().await;
                if let Some(state) = peer_states.get_mut(&peer_addr) {
                    state.handshake_complete = true;
                }
                drop(peer_states);

                self.fill_high_bandwidth_slots().await;

                let fee_rate = self
                    .mempool_manager()
                    .map(|mm| mm.min_relay_fee_sat_per_kvb())
                    .unwrap_or(100);
                let fee_msg =
                    ProtocolMessage::FeeFilter(crate::network::protocol::FeeFilterMessage {
                        feerate: fee_rate,
                    });
                if let Ok(wire) = ProtocolParser::serialize_message(&fee_msg) {
                    let _ = self.send_to_peer(peer_addr, wire).await;
                }

                // Send GetAddr immediately after handshake to populate the address DB
                // with peers known to this node — including potentially full-history
                // archival nodes that aren't in the DNS seeds.
                match ProtocolParser::serialize_message(&ProtocolMessage::GetAddr) {
                    Ok(wire) => {
                        if let Err(e) = self.send_to_peer(peer_addr, wire).await {
                            tracing::debug!("GetAddr to {} failed: {}", peer_addr, e);
                        }
                    }
                    Err(e) => {
                        tracing::debug!("Failed to serialize GetAddr: {}", e);
                    }
                }
                match ProtocolParser::serialize_message(&ProtocolMessage::SendHeaders) {
                    Ok(wire) => {
                        if let Err(e) = self.send_to_peer(peer_addr, wire).await {
                            tracing::debug!("SendHeaders to {} failed: {}", peer_addr, e);
                        }
                    }
                    Err(e) => {
                        tracing::debug!("Failed to serialize SendHeaders: {}", e);
                    }
                }

                return Ok(());
            }
            ProtocolMessage::GetBanList(msg) => {
                if self.ban_sharing_active() {
                    self.handle_get_ban_list(peer_addr, msg.clone()).await?;
                }
                return Ok(());
            }
            ProtocolMessage::BanList(msg) => {
                if self.ban_sharing_active() {
                    self.handle_ban_list(peer_addr, msg.clone()).await?;
                }
                return Ok(());
            }
            ProtocolMessage::NotFound(msg) => {
                // A peer that does not have the block used to be ignored here, so the
                // download oneshot stayed open until the 90–135s chunk deadline.
                use crate::network::inventory::{
                    MSG_BLOCK, MSG_TX, MSG_WITNESS_BLOCK, MSG_WITNESS_TX, MSG_WTX,
                };
                let mut dropped = 0u32;
                let mut follow = Vec::new();
                for inv in &msg.inventory {
                    if inv.inv_type == MSG_BLOCK || inv.inv_type == MSG_WITNESS_BLOCK {
                        self.cancel_block_request_force(peer_addr, inv.hash);
                        dropped = dropped.saturating_add(1);
                    } else if inv.inv_type == MSG_TX
                        || inv.inv_type == MSG_WITNESS_TX
                        || inv.inv_type == MSG_WTX
                    {
                        follow.extend(self.finish_tx_request(peer_addr, inv.hash));
                    }
                }
                self.request_witness_txs(peer_addr, &follow).await;
                if dropped > 0 {
                    warn!(
                        "[IBD_NOTFOUND] peer={} dropped {} block request(s)",
                        peer_addr, dropped
                    );
                }
                return Ok(());
            }
            ProtocolMessage::SendPackages(version) => {
                let mut states = self.peer_states().write().await;
                if let Some(state) = states.get_mut(&peer_addr) {
                    if state.version > 0
                        && !state.handshake_complete
                        && state.package_remote.is_none()
                    {
                        state.package_remote = Some(*version);
                    }
                }
                return Ok(());
            }
            ProtocolMessage::AncPkgInfo(_)
            | ProtocolMessage::GetPkgTxns(_)
            | ProtocolMessage::PkgTxns(_) => {
                return Ok(());
            }
            ProtocolMessage::WtxidRelay => {
                let mut states = self.peer_states().write().await;
                if let Some(state) = states.get_mut(&peer_addr) {
                    if state.version > 0 && !state.handshake_complete && !state.wtxid_relay {
                        state.wtxid_relay = true;
                    }
                }
                return Ok(());
            }
            ProtocolMessage::SendHeaders => {
                let mut states = self.peer_states().write().await;
                if let Some(state) = states.get_mut(&peer_addr) {
                    state.prefer_headers = true;
                }
                return Ok(());
            }
            _ => {}
        }

        Ok(())
    }
}

#[cfg(test)]
#[path = "wire_dispatch_plan_wire_locks.rs"]
mod plan_wire_locks;

#[cfg(test)]
mod ibd_refusal_tests {
    use super::split_ibd_refused_inventory;
    use crate::network::inventory::{MSG_WITNESS_BLOCK, MSG_WITNESS_TX};
    use crate::network::protocol::InventoryVector;

    #[test]
    fn ibd_refusal_notfound_lists_blocks_and_keeps_the_transaction() {
        let block = InventoryVector {
            inv_type: MSG_WITNESS_BLOCK,
            hash: [1u8; 32],
        };
        let tx = InventoryVector {
            inv_type: MSG_WITNESS_TX,
            hash: [2u8; 32],
        };
        let (refused, serve) = split_ibd_refused_inventory(&[block.clone(), tx.clone()]);
        assert_eq!(refused, vec![block]);
        assert_eq!(serve, vec![tx]);
    }

    #[test]
    fn seventeen_block_getdata_is_historical() {
        assert!(!super::block_getdata_is_historical_download(1));
        assert!(!super::block_getdata_is_historical_download(16));
        assert!(super::block_getdata_is_historical_download(17));
    }
}
