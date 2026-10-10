//! Protocol message dispatch for incoming wire messages.
//!
//! Handles special cases and routes messages to the appropriate handlers.

#[cfg(feature = "protocol-verification")]
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
    #[cfg_attr(feature = "protocol-verification", spec_locked("10.2.1"))]
    #[cfg_attr(feature = "protocol-verification", blvm_spec_lock::ensures(true))]
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

        let mut pm = self.peer_manager_mutex().lock().await;
        let transport_addr = pm.find_transport_addr_by_socket(peer_addr);
        let transport_addr_for_verack = transport_addr.clone();

        if let Some(transport_addr) = transport_addr {
            if let Some(peer) = pm.get_peer_mut(&transport_addr) {
                peer.set_version(version_msg.version as u32);
                peer.set_services(version_msg.services);
                peer.set_user_agent(version_msg.user_agent.clone());
                peer.set_start_height(version_msg.start_height);
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
            state.version = version_msg.version as u32;
        }

        if let Some(ref transport_addr) = transport_addr_for_verack {
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
        let bytes = blvm_protocol::serialization::serialize_block_with_witnesses(
            &block,
            &witnesses,
            true,
        );
        self.queue_block(bytes);
        Ok(())
    }

    /// Match a compact block against the pool and ask for the holes.
    pub(crate) async fn handle_cmpctblock(
        &self,
        peer_addr: SocketAddr,
        compact: &blvm_protocol::bip152::CompactBlock,
    ) -> Result<()> {
        use crate::network::compact_blocks::begin_compact_assembly;
        use crate::network::txhash::calculate_wtxid;
        use blvm_protocol::block::calculate_tx_id;
        use std::collections::HashMap;

        let mut pool = HashMap::new();
        if let Some(mm) = self.mempool_manager() {
            for tx in mm.get_transactions() {
                let txid = calculate_tx_id(&tx);
                let witness = mm.get_transaction_witnesses(&txid);
                let wtxid = calculate_wtxid(&tx, witness.as_deref());
                pool.insert(wtxid, (tx, witness));
            }
        }
        let assembly = begin_compact_assembly(compact, &pool)?;
        let block_hash = block_hash_from_header(&compact.header);
        if assembly.missing.is_empty() {
            return self.queue_completed_compact(&assembly);
        }
        let indices: Vec<u16> = assembly
            .missing
            .iter()
            .filter_map(|index| u16::try_from(*index).ok())
            .collect();
        if let Ok(mut pending) = self.pending_compact.lock() {
            pending.insert(block_hash, assembly);
        }
        let msg = ProtocolMessage::GetBlockTxn(crate::network::protocol::GetBlockTxnMessage {
            block_hash,
            indices,
        });
        if let Ok(wire) = ProtocolParser::serialize_message(&msg) {
            let _ = self.send_to_peer(peer_addr, wire).await;
        }
        Ok(())
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
        let Some(block) = storage
            .blocks()
            .get_block(&msg.block_hash)
            .ok()
            .flatten()
        else {
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
        let reply = ProtocolMessage::BlockTxn(crate::network::protocol::BlockTxnMessage {
            block_hash: msg.block_hash,
            transactions,
            witnesses: None,
        });
        if let Ok(wire) = ProtocolParser::serialize_message(&reply) {
            let _ = self.send_to_peer(peer_addr, wire).await;
        }
        Ok(())
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
        let version_received = {
            let peer_states = self.peer_states().read().await;
            peer_states
                .get(&peer_addr)
                .map(|s| s.version > 0)
                .unwrap_or(false)
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
            ProtocolMessage::Tx(_) => {
                let _ = self
                    .peer_tx()
                    .send(NetworkMessage::TransactionReceived(data));
                return Ok(());
            }
            ProtocolMessage::FeeFilter(fee_filter) => {
                // BIP133 feerate is satoshis per 1000 virtual bytes.
                let mut states = self.peer_states().write().await;
                if let Some(state) = states.get_mut(&peer_addr) {
                    state.min_fee_rate = Some(fee_filter.feerate / 1000);
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
                let is_full_chain_request = getheaders.block_locator_hashes.is_empty();

                if is_full_chain_request {
                    match self.ibd_protection().can_serve_ibd(peer_addr).await {
                        Ok(true) => {
                            self.ibd_protection().start_ibd_serving(peer_addr).await;
                            debug!(
                                "IBD protection: Allowing full chain sync request from {}",
                                peer_addr
                            );
                        }
                        Ok(false) => {
                            warn!(
                                "IBD protection: Rejecting full chain sync request from {} (bandwidth limit exceeded or cooldown active)",
                                peer_addr
                            );
                            return Ok(());
                        }
                        Err(e) => {
                            warn!("IBD protection check failed for {}: {}", peer_addr, e);
                        }
                    }
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
                            let msg = ProtocolMessage::Headers(HeadersMessage { headers });
                            if let Ok(wire) = ProtocolParser::serialize_message(&msg) {
                                if let Err(e) = self.send_to_peer(peer_addr, wire).await {
                                    warn!("Failed to send Headers to {}: {}", peer_addr, e);
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
                let has_block_requests = getdata
                    .inventory
                    .iter()
                    .any(|inv| inv.inv_type == MSG_BLOCK || inv.inv_type == MSG_WITNESS_BLOCK);

                if has_block_requests {
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
                            let notfound = NotFoundMessage {
                                inventory: getdata.inventory.clone(),
                            };
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
                            return Ok(());
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

                let serve_result = self
                    .serve_getdata_request(peer_addr, getdata, protocol_version)
                    .await;
                if has_block_requests {
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
                            false,
                        )
                        .await;
                }
                let mut wanted = Vec::new();
                for inv in &inv_msg.inventory {
                    if inv.inv_type != crate::network::inventory::MSG_TX
                        && inv.inv_type != crate::network::inventory::MSG_WITNESS_TX
                    {
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
                        wanted.push(crate::network::protocol::InventoryVector {
                            inv_type: crate::network::inventory::MSG_WITNESS_TX,
                            hash: inv.hash,
                        });
                    }
                }
                if !wanted.is_empty() {
                    let getdata = ProtocolMessage::GetData(
                        crate::network::protocol::GetDataMessage { inventory: wanted },
                    );
                    if let Ok(wire) = ProtocolParser::serialize_message(&getdata) {
                        let _ = self.send_to_peer(peer_addr, wire).await;
                    }
                }
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
                if headers_msg.headers.len() > max_headers {
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
                        let mut batch = Vec::new();
                        for header in &headers_msg.headers {
                            if header.version < 1
                                || header.timestamp == 0
                                || header.prev_block_hash != prev
                            {
                                break;
                            }
                            height = height.saturating_add(1);
                            let hash = blvm_consensus::block::block_header_hash(header);
                            batch.push((hash, header.clone(), height));
                            prev = hash;
                        }
                        if !batch.is_empty() {
                            if let Err(e) = storage.blocks().store_headers_batch(&batch) {
                                warn!("unsolicited headers from {}: {}", peer_addr, e);
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
                if cmpct_msg.compact_block.short_ids.len() > 10000 {
                    warn!(
                        "Invalid compact block: too many short IDs ({}) from {}",
                        cmpct_msg.compact_block.short_ids.len(),
                        peer_addr
                    );
                    let _ =
                        self.peer_tx()
                            .send(NetworkMessage::PeerDisconnected(TransportAddr::Tcp(
                                peer_addr,
                            )));
                    return Err(anyhow::anyhow!("Invalid compact block: too many short IDs"));
                }
                self.handle_cmpctblock(peer_addr, &cmpct_msg.compact_block)
                    .await?;
                return Ok(());
            }
            ProtocolMessage::GetBlockTxn(getblocktxn_msg) => {
                if getblocktxn_msg.indices.len() > 10000 {
                    warn!(
                        "GetBlockTxn with too many indices ({}) from {}",
                        getblocktxn_msg.indices.len(),
                        peer_addr
                    );
                    let _ =
                        self.peer_tx()
                            .send(NetworkMessage::PeerDisconnected(TransportAddr::Tcp(
                                peer_addr,
                            )));
                    return Err(anyhow::anyhow!("GetBlockTxn with too many indices"));
                }
                self.handle_getblocktxn(peer_addr, getblocktxn_msg).await?;
                return Ok(());
            }
            ProtocolMessage::BlockTxn(blocktxn_msg) => {
                if blocktxn_msg.transactions.len() > 10000 {
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
                if let Some(storage) = self.storage().as_ref() {
                    use crate::network::inventory::MSG_BLOCK;
                    use blvm_consensus::block::block_header_hash;

                    const MAX_GETBLOCKS_INV: usize = 500;
                    match storage.blocks().build_headers_response(
                        &getblocks.block_locator_hashes,
                        &getblocks.hash_stop,
                        MAX_GETBLOCKS_INV,
                    ) {
                        Ok(headers) if !headers.is_empty() => {
                            let inventory: Vec<crate::network::protocol::InventoryVector> = headers
                                .into_iter()
                                .map(|header| crate::network::protocol::InventoryVector {
                                    inv_type: MSG_BLOCK,
                                    hash: block_header_hash(&header),
                                })
                                .collect();
                            let inv_msg =
                                ProtocolMessage::Inv(crate::network::protocol::InvMessage {
                                    inventory,
                                });
                            if let Ok(wire) = ProtocolParser::serialize_message(&inv_msg) {
                                let _ = self.send_to_peer(peer_addr, wire).await;
                            }
                        }
                        Ok(_) => {
                            let empty_inv =
                                ProtocolMessage::Inv(crate::network::protocol::InvMessage {
                                    inventory: vec![],
                                });
                            if let Ok(wire) = ProtocolParser::serialize_message(&empty_inv) {
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
                if let Some(mm) = self.mempool_manager() {
                    use crate::network::inventory::MSG_TX;
                    use blvm_protocol::block::calculate_tx_id;
                    let min_sat_vb = {
                        let states = self.peer_states().read().await;
                        states.get(&peer_addr).and_then(|state| state.min_fee_rate)
                    };
                    let txns: Vec<blvm_protocol::Transaction> = mm.get_transactions();
                    let inventory: Vec<crate::network::protocol::InventoryVector> = txns
                        .iter()
                        .filter_map(|tx| {
                            let hash = calculate_tx_id(tx);
                            if let Some(min) = min_sat_vb {
                                if mm.cached_fee_rate(&hash).unwrap_or(u64::MAX) < min {
                                    return None;
                                }
                            }
                            Some(crate::network::protocol::InventoryVector {
                                inv_type: MSG_TX,
                                hash,
                            })
                        })
                        .collect();
                    let inv_msg =
                        ProtocolMessage::Inv(crate::network::protocol::InvMessage { inventory });
                    if let Ok(wire) = ProtocolParser::serialize_message(&inv_msg) {
                        let _ = self.send_to_peer(peer_addr, wire).await;
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

                return Ok(());
            }
            ProtocolMessage::GetBanList(msg) => {
                if self.ban_list_sharing_config.is_some() {
                    self.handle_get_ban_list(peer_addr, msg.clone()).await?;
                }
                return Ok(());
            }
            ProtocolMessage::BanList(msg) => {
                if self.ban_list_sharing_config.is_some() {
                    self.handle_ban_list(peer_addr, msg.clone()).await?;
                }
                return Ok(());
            }
            ProtocolMessage::NotFound(msg) => {
                // A peer that does not have the block used to be ignored here, so the
                // download oneshot stayed open until the 90–135s chunk deadline.
                use crate::network::inventory::{MSG_BLOCK, MSG_WITNESS_BLOCK};
                let mut dropped = 0u32;
                for inv in &msg.inventory {
                    if inv.inv_type != MSG_BLOCK && inv.inv_type != MSG_WITNESS_BLOCK {
                        continue;
                    }
                    self.cancel_block_request_force(peer_addr, inv.hash);
                    dropped = dropped.saturating_add(1);
                }
                if dropped > 0 {
                    warn!(
                        "[IBD_NOTFOUND] peer={} dropped {} block request(s)",
                        peer_addr, dropped
                    );
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
mod plan_wire_locks {
    use super::*;
    use crate::network::inventory::{MSG_TX, MSG_WITNESS_TX};
    use crate::network::peer::Peer;
    use crate::network::protocol::{FeeFilterMessage, GetDataMessage, InvMessage, InventoryVector};
    use crate::network::transport::TransportAddr;
    use crate::node::mempool::MempoolManager;
    use crate::storage::Storage;
    use blvm_protocol::constants::SEQUENCE_FINAL;
    use blvm_protocol::network::PeerState;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{
        BitcoinProtocolEngine, BlockHeader, OutPoint, ProtocolVersion, Transaction,
        TransactionInput, TransactionOutput, UTXO, UtxoSet,
    };
    use std::sync::Arc;

    struct Wired {
        manager: NetworkManager,
        peer: SocketAddr,
        incoming: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        storage: Arc<Storage>,
        mempool: Arc<MempoolManager>,
        _dir: tempfile::TempDir,
    }

    async fn wired() -> Wired {
        let dir = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(dir.path()).unwrap());
        let mempool = Arc::new(MempoolManager::new());
        let protocol =
            Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let listen: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let manager = NetworkManager::new(listen).with_dependencies(
            protocol,
            Arc::clone(&storage),
            Arc::clone(&mempool),
        );
        let peer: SocketAddr = "127.0.0.1:18444".parse().unwrap();
        let (peer_conn, incoming) = Peer::pair_for_testing(peer);
        manager
            .peer_manager_mutex()
            .lock()
            .await
            .add_peer(TransportAddr::Tcp(peer), peer_conn)
            .unwrap();
        let mut state = PeerState::new();
        state.version = 70015;
        manager.peer_states().write().await.insert(peer, state);
        Wired {
            manager,
            peer,
            incoming,
            storage,
            mempool,
            _dir: dir,
        }
    }

    fn spend(prevout: OutPoint, value: i64) -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout,
                script_sig: Vec::new(),
                sequence: SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        }
    }

    #[tokio::test]
    async fn peer_at_ten_sat_per_vb_is_not_offered_a_one_sat_transaction() {
        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::segwit::transaction_weight_from_stacks;

        let mut node = wired().await;
        let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
        policy.min_tx_fee = 0;
        policy.min_relay_fee_rate = 1;
        node.mempool.set_policy_config(Some(policy));
        let cheap_prev = OutPoint {
            hash: [1u8; 32],
            index: 0,
        };
        let rich_prev = OutPoint {
            hash: [2u8; 32],
            index: 0,
        };
        let mut cheap = spend(cheap_prev, 50_000);
        let mut rich = spend(rich_prev, 50_000);
        cheap.inputs[0].script_sig = vec![OP_1];
        rich.inputs[0].script_sig = vec![OP_1];
        let vsize = transaction_weight_from_stacks(&cheap, None)
            .unwrap()
            .div_ceil(4)
            .max(1) as i64;
        cheap.outputs[0].value = 50_000;
        rich.outputs[0].value = 50_000;
        let mut set = UtxoSet::default();
        for (prev, extra) in [(cheap_prev, vsize), (rich_prev, vsize * 20)] {
            let utxo = UTXO {
                value: 50_000 + extra,
                script_pubkey: vec![OP_1].into(),
                height: 1,
                is_coinbase: false,
            };
            set.insert(prev, Arc::new(utxo));
        }
        node.mempool
            .set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
        assert!(node.mempool.add_transaction(cheap.clone()).unwrap());
        assert!(node.mempool.add_transaction(rich.clone()).unwrap());
        node.manager
            .dispatch_protocol_message(
                node.peer,
                &ProtocolMessage::FeeFilter(FeeFilterMessage { feerate: 10_000 }),
                Vec::new(),
            )
            .await
            .unwrap();
        node.manager
            .dispatch_protocol_message(node.peer, &ProtocolMessage::MemPool, Vec::new())
            .await
            .unwrap();
        let bytes = node.incoming.recv().await.unwrap();
        let parsed = ProtocolParser::parse_message(&bytes).unwrap();
        let ProtocolMessage::Inv(InvMessage { inventory }) = parsed else {
            panic!("mempool reply was not an inv");
        };
        let cheap_id = calculate_tx_id(&cheap);
        let rich_id = calculate_tx_id(&rich);
        assert!(inventory.iter().any(|item| item.hash == rich_id));
        assert!(inventory.iter().all(|item| item.hash != cheap_id));
    }

    #[tokio::test]
    async fn unknown_transaction_inv_produces_one_getdata() {
        let mut node = wired().await;
        let hash = [9u8; 32];
        node.manager
            .dispatch_protocol_message(
                node.peer,
                &ProtocolMessage::Inv(InvMessage {
                    inventory: vec![InventoryVector {
                        inv_type: MSG_TX,
                        hash,
                    }],
                }),
                Vec::new(),
            )
            .await
            .unwrap();
        let bytes = node.incoming.recv().await.unwrap();
        let parsed = ProtocolParser::parse_message(&bytes).unwrap();
        let ProtocolMessage::GetData(GetDataMessage { inventory }) = parsed else {
            panic!("inv reply was not getdata");
        };
        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory[0].inv_type, MSG_WITNESS_TX);
        assert_eq!(inventory[0].hash, hash);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unsolicited_header_on_the_tip_extends_the_header_chain() {
        let node = wired().await;
        let genesis = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [2u8; 32],
            timestamp: 1_500_000_000,
            bits: 0x207fffff,
            nonce: 0,
        };
        node.storage.chain().initialize(&genesis).unwrap();
        let tip = node.storage.chain().get_tip_hash().unwrap().unwrap();
        node.storage.blocks().store_header(&tip, &genesis).unwrap();
        node.storage.blocks().store_height(0, &tip).unwrap();
        let child = BlockHeader {
            version: 1,
            prev_block_hash: tip,
            merkle_root: [3u8; 32],
            timestamp: 1_500_000_001,
            bits: 0x207fffff,
            nonce: 1,
        };
        node.manager
            .dispatch_protocol_message(
                node.peer,
                &ProtocolMessage::Headers(HeadersMessage {
                    headers: vec![child],
                }),
                Vec::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            node.storage.blocks().highest_stored_height().unwrap(),
            Some(1)
        );
        node.manager
            .dispatch_protocol_message(node.peer, &ProtocolMessage::SendHeaders, Vec::new())
            .await
            .unwrap();
        let states = node.manager.peer_states().read().await;
        assert!(states.get(&node.peer).unwrap().prefer_headers);
    }
}
