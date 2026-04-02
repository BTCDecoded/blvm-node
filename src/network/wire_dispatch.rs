//! Protocol message dispatch for incoming wire messages.
//!
//! Handles special cases and routes messages to the appropriate handlers.

#[cfg(feature = "protocol-verification")]
use blvm_spec_lock::spec_locked;

use crate::network::network_manager::NetworkManager;
use crate::network::protocol::{HeadersMessage, ProtocolMessage, ProtocolParser, VersionMessage};
use crate::network::transport::TransportAddr;
use crate::network::NetworkMessage;
use anyhow::Result;
use std::net::SocketAddr;
use tracing::{debug, info, warn};

impl NetworkManager {
    /// Handle Version message: update peer state and send VerAck (handshake).
    /// Orange Paper 10.2.1: On Version received, send VerAck. VerAck never sent before Version.
    #[cfg_attr(feature = "protocol-verification", spec_locked("10.2.1"))]
    pub(crate) async fn handle_version_received(
        &self,
        peer_addr: SocketAddr,
        version_msg: &VersionMessage,
    ) -> Result<()> {
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

        if let Some(transport_addr) = transport_addr_for_verack {
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
            ProtocolMessage::FeeFilter(_) => {
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

                use crate::network::inventory::MSG_BLOCK;
                let has_block_requests = getdata
                    .inventory
                    .iter()
                    .any(|inv| inv.inv_type == MSG_BLOCK);

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
                let _ = self.peer_tx().send(NetworkMessage::InventoryReceived(data));
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
            }
            ProtocolMessage::Block(block_msg) => {
                info!(
                    "Block message received from {} ({} bytes)",
                    peer_addr,
                    data.len()
                );
                use crate::storage::hashing::double_sha256;
                let header = &block_msg.block.header;
                let mut header_bytes = Vec::with_capacity(80);
                header_bytes.extend_from_slice(&(header.version as i32).to_le_bytes());
                header_bytes.extend_from_slice(&header.prev_block_hash);
                header_bytes.extend_from_slice(&header.merkle_root);
                header_bytes.extend_from_slice(&(header.timestamp as u32).to_le_bytes());
                header_bytes.extend_from_slice(&(header.bits as u32).to_le_bytes());
                header_bytes.extend_from_slice(&(header.nonce as u32).to_le_bytes());
                let block_hash = double_sha256(&header_bytes);
                if self.complete_block_request(
                    peer_addr,
                    block_hash,
                    block_msg.block.clone(),
                    block_msg.witnesses.clone(),
                ) {
                    info!(
                        "Block routed to pending request from {} (hash {})",
                        peer_addr,
                        hex::encode(block_hash)
                    );
                    return Ok(());
                }
                // Relay / post-IBD: main loop / process_block expect consensus block+witness bytes,
                // not a full P2P frame (magic/command/checksum wrapper).
                let wire = blvm_protocol::serialization::serialize_block_with_witnesses(
                    &block_msg.block,
                    &block_msg.witnesses,
                    true,
                );
                self.queue_block(wire);
                debug!(
                    "Block from {} (hash {}) queued for main loop (no IBD getdata pending)",
                    peer_addr,
                    hex::encode(block_hash)
                );
                return Ok(());
            }
            ProtocolMessage::CmpctBlock(cmpct_msg) => {
                use crate::network::compact_blocks::CompactBlock;

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
            _ => {}
        }

        Ok(())
    }
}
