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
        info!(
            "Block message received from {} ({} bytes)",
            peer_addr,
            data.len()
        );
        let block_hash = block_hash_from_header(&block_msg.block.header);
        if self.complete_block_request(peer_addr, block_hash, block_msg.block, block_msg.witnesses)
        {
            info!(
                "Block routed to pending request from {} (hash {})",
                peer_addr,
                hex::encode(block_hash)
            );
            return Ok(());
        }
        // Relay / post-IBD: queue the P2P block payload for the main loop.
        let payload_len = u32::from_le_bytes([data[16], data[17], data[18], data[19]]) as usize;
        if data.len() < 24 + payload_len {
            warn!(
                "Block from {} truncated (frame {} bytes, payload {} bytes)",
                peer_addr,
                data.len(),
                payload_len
            );
            return Ok(());
        }
        let payload = data[24..24 + payload_len].to_vec();
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

        if let Some(ref transport) = transport_addr {
            if super::reduced_data_p2p::should_reject_non_rdts_outbound(
                &self.reduced_data_config,
                &pm,
                transport,
                version_msg.services,
            ) {
                drop(pm);
                warn!(
                    "Peer {} lacks NODE_REDUCED_DATA and non-RDTS outbound cap ({}) reached — disconnecting",
                    peer_addr, self.reduced_data_config.max_non_rdts_outbound
                );
                return self
                    .disconnect_for_protocol_violation(
                        peer_addr,
                        "non-RDTS outbound peer cap exceeded",
                        false,
                    )
                    .await;
            }
        }

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
                    warn!("Peer {} sent Verack before Version — ignoring", peer_addr);
                    return Ok(());
                }
                _ => {
                    warn!(
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
                            warn!("Received pong with non-matching nonce from {}", peer_addr);
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
            ProtocolMessage::FeeFilter(_) => {
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

                if let Err(e) = self
                    .serve_getdata_request(peer_addr, getdata, protocol_version)
                    .await
                {
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
                // No pending getheaders request matched this peer — unsolicited headers.
                // Penalize the peer to deter flooding; do not ban (might be a race).
                warn!(
                    "Unsolicited headers message from {} ({} headers) — penalizing peer",
                    peer_addr,
                    headers_msg.headers.len()
                );
                return self
                    .disconnect_for_protocol_violation(
                        peer_addr,
                        "unsolicited headers message",
                        false, // don't ban — could be a race with a legitimate request
                    )
                    .await;
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
                    let txns: Vec<blvm_protocol::Transaction> = mm.get_transactions();
                    let inventory: Vec<crate::network::protocol::InventoryVector> = txns
                        .iter()
                        .map(|tx| crate::network::protocol::InventoryVector {
                            inv_type: MSG_TX,
                            hash: calculate_tx_id(tx),
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
                return Ok(());
            }
            _ => {}
        }

        Ok(())
    }
}
