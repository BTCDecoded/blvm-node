//! Fulfill incoming [`getdata`](https://en.bitcoin.it/wiki/Protocol_documentation#getdata) requests.
//!
//! Serves full `block` / `tx` wire messages from storage when data is present and complete
//! (including witness data when required after segwit activation). Missing or incomplete objects
//! produce `notfound` entries so peers can query other nodes.
//!
//! Block hashes merged via [`crate::module::traits::NodeAPI::merge_block_serve_denylist`]
//! (e.g. selective-sync or policy modules) are never served as full `block` messages.
//!
//! Mode T tip90:
//! - W5 `BLVW` bodies are framed without deserialize/re-serialize.
//! - Sequential load+send via spawn_blocking per inv (tc284 tip30≈120 BEST rematch).
//!   Sync-on-async (tc285) REVERT tip30≈23 — blocks runtime.
//! - **A4:** under `BLVM_SERVE_ONLY`, bounded load-ahead (`getdata_serve_pipe_depth`,
//!   default K=4) on the blocking pool with **strict inventory-order send**. Full-inventory
//!   fanout / dual-lane write already REVERT’d dens tip30.

use crate::config::getdata_serve_pipe_depth;
use crate::network::inventory::{
    MSG_BLOCK, MSG_TX, MSG_WITNESS_BLOCK, MSG_WITNESS_FLAG, MSG_WITNESS_TX,
};
use crate::network::network_manager::NetworkManager;
use crate::network::protocol::{
    BlockMessage, GetDataMessage, InventoryVector, NotFoundMessage, ProtocolMessage,
    ProtocolParser, TxMessage,
};
use crate::node::parallel_ibd::local_block::{cached_feature_registry, empty_witness_unacceptable};
use crate::storage::blockstore::{decode_wire_body_blob, is_wire_body_blob};
use anyhow::Result;
use blvm_protocol::ProtocolVersion;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

fn inventory_requests_tx(inv_type: u32) -> bool {
    inv_type == MSG_TX
        || inv_type == MSG_WITNESS_TX
        || inv_type == crate::network::inventory::MSG_WTX
}

impl NetworkManager {
    /// Answer a peer `getdata` using chain/mempool storage.
    pub(crate) async fn serve_getdata_request(
        &self,
        peer_addr: SocketAddr,
        getdata: &GetDataMessage,
        protocol_version: ProtocolVersion,
    ) -> Result<()> {
        if getdata.inventory.is_empty() {
            return Ok(());
        }

        let Some(storage) = self.storage().as_ref() else {
            return self
                .send_notfound_for_inventory(peer_addr, getdata.inventory.clone())
                .await;
        };

        let blockstore = storage.blocks();
        let txindex = storage.transactions();
        let mempool_mgr = self.mempool_manager();
        let maint = self.block_serve_maintenance_mode();
        let pipe = getdata_serve_pipe_depth();

        if pipe > 1 {
            return self
                .serve_getdata_pipelined(
                    peer_addr,
                    getdata,
                    protocol_version,
                    Arc::clone(&blockstore),
                    txindex,
                    mempool_mgr,
                    maint,
                    pipe,
                )
                .await;
        }

        let mut missing: Vec<InventoryVector> = Vec::new();

        for item in &getdata.inventory {
            match item.inv_type {
                MSG_BLOCK | MSG_WITNESS_BLOCK
                    if !(maint || self.is_block_serve_denied(&item.hash)) =>
                {
                    let bs = Arc::clone(&blockstore);
                    let hash = item.hash;
                    let include_witness = inventory_includes_witness(item.inv_type);
                    let join = tokio::task::spawn_blocking(move || {
                        build_block_wire(&bs, &hash, protocol_version, include_witness)
                    })
                    .await;
                    let res = match join {
                        Ok(inner) => inner,
                        Err(e) => Err(anyhow::anyhow!("getdata block join: {e}")),
                    };
                    match res {
                        Ok(Some(wire)) => {
                            if let Err(e) = self.send_to_peer(peer_addr, wire).await {
                                warn!(
                                    "getdata: failed to send block {} to {}: {}",
                                    hex::encode(item.hash),
                                    peer_addr,
                                    e
                                );
                            }
                        }
                        Ok(None) => missing.push(item.clone()),
                        Err(e) => {
                            warn!(
                                "getdata: error loading block {}: {}",
                                hex::encode(item.hash),
                                e
                            );
                            missing.push(item.clone());
                        }
                    }
                }
                MSG_BLOCK | MSG_WITNESS_BLOCK => missing.push(item.clone()),
                inv_type if inventory_requests_tx(inv_type) => {
                    let res = if self.is_tx_serve_denied(&item.hash) {
                        Ok(None)
                    } else {
                        build_tx_wire(
                            &txindex,
                            blockstore.as_ref(),
                            mempool_mgr,
                            &item.hash,
                            inventory_includes_witness(inv_type),
                        )
                    };
                    match res {
                        Ok(Some(wire)) => {
                            if let Err(e) = self.send_to_peer(peer_addr, wire).await {
                                warn!(
                                    "getdata: failed to send tx {} to {}: {}",
                                    hex::encode(item.hash),
                                    peer_addr,
                                    e
                                );
                            }
                        }
                        Ok(None) => missing.push(item.clone()),
                        Err(e) => {
                            warn!(
                                "getdata: error loading tx {}: {}",
                                hex::encode(item.hash),
                                e
                            );
                            missing.push(item.clone());
                        }
                    }
                }
                _ => missing.push(item.clone()),
            }
        }

        if !missing.is_empty() {
            self.send_notfound_for_inventory(peer_addr, missing).await?;
        }
        Ok(())
    }

    /// A4: start up to `pipe` block loads ahead; await+send strictly in inventory order.
    #[allow(clippy::too_many_arguments)]
    async fn serve_getdata_pipelined(
        &self,
        peer_addr: SocketAddr,
        getdata: &GetDataMessage,
        protocol_version: ProtocolVersion,
        blockstore: Arc<crate::storage::blockstore::BlockStore>,
        txindex: Arc<crate::storage::txindex::TxIndex>,
        mempool_mgr: Option<&std::sync::Arc<crate::node::mempool::MempoolManager>>,
        maint: bool,
        pipe: usize,
    ) -> Result<()> {
        static LOGGED: AtomicBool = AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::Relaxed) {
            info!(
                "[IBD_A4_SERVE_PIPE] depth={} (load-ahead; ordered send; SERVE_ONLY path)",
                pipe
            );
        }

        let n = getdata.inventory.len();
        let mut jobs: Vec<Option<tokio::task::JoinHandle<Result<Option<Vec<u8>>>>>> =
            (0..n).map(|_| None).collect();
        let mut launched = vec![false; n];
        let mut missing: Vec<InventoryVector> = Vec::new();

        for send_i in 0..n {
            let prefetch_end = (send_i + pipe).min(n);
            for j in send_i..prefetch_end {
                if launched[j] {
                    continue;
                }
                launched[j] = true;
                let item = &getdata.inventory[j];
                if matches!(item.inv_type, MSG_BLOCK | MSG_WITNESS_BLOCK)
                    && !(maint || self.is_block_serve_denied(&item.hash))
                {
                    let bs = Arc::clone(&blockstore);
                    let hash = item.hash;
                    let include_witness = inventory_includes_witness(item.inv_type);
                    jobs[j] = Some(tokio::task::spawn_blocking(move || {
                        build_block_wire(&bs, &hash, protocol_version, include_witness)
                    }));
                }
            }

            let item = &getdata.inventory[send_i];
            match item.inv_type {
                MSG_BLOCK | MSG_WITNESS_BLOCK
                    if !(maint || self.is_block_serve_denied(&item.hash)) =>
                {
                    let join = jobs[send_i].take().ok_or_else(|| {
                        anyhow::anyhow!("getdata A4: block job missing at inventory index {send_i}")
                    })?;
                    let res = match join.await {
                        Ok(inner) => inner,
                        Err(e) => Err(anyhow::anyhow!("getdata block join: {e}")),
                    };
                    match res {
                        Ok(Some(wire)) => {
                            if let Err(e) = self.send_to_peer(peer_addr, wire).await {
                                warn!(
                                    "getdata: failed to send block {} to {}: {}",
                                    hex::encode(item.hash),
                                    peer_addr,
                                    e
                                );
                            }
                        }
                        Ok(None) => missing.push(item.clone()),
                        Err(e) => {
                            warn!(
                                "getdata: error loading block {}: {}",
                                hex::encode(item.hash),
                                e
                            );
                            missing.push(item.clone());
                        }
                    }
                }
                MSG_BLOCK | MSG_WITNESS_BLOCK => missing.push(item.clone()),
                inv_type if inventory_requests_tx(inv_type) => {
                    let res = if self.is_tx_serve_denied(&item.hash) {
                        Ok(None)
                    } else {
                        build_tx_wire(
                            &txindex,
                            blockstore.as_ref(),
                            mempool_mgr,
                            &item.hash,
                            inventory_includes_witness(inv_type),
                        )
                    };
                    match res {
                        Ok(Some(wire)) => {
                            if let Err(e) = self.send_to_peer(peer_addr, wire).await {
                                warn!(
                                    "getdata: failed to send tx {} to {}: {}",
                                    hex::encode(item.hash),
                                    peer_addr,
                                    e
                                );
                            }
                        }
                        Ok(None) => missing.push(item.clone()),
                        Err(e) => {
                            warn!(
                                "getdata: error loading tx {}: {}",
                                hex::encode(item.hash),
                                e
                            );
                            missing.push(item.clone());
                        }
                    }
                }
                _ => missing.push(item.clone()),
            }
        }

        if !missing.is_empty() {
            self.send_notfound_for_inventory(peer_addr, missing).await?;
        }
        Ok(())
    }

    async fn send_notfound_for_inventory(
        &self,
        peer_addr: SocketAddr,
        inventory: Vec<InventoryVector>,
    ) -> Result<()> {
        let msg = ProtocolMessage::NotFound(NotFoundMessage { inventory });
        let wire = ProtocolParser::serialize_message(&msg)?;
        self.send_to_peer(peer_addr, wire).await
    }
}

fn inventory_includes_witness(inv_type: u32) -> bool {
    inv_type & MSG_WITNESS_FLAG != 0
}

fn block_payload_has_witness_marker(payload: &[u8]) -> bool {
    payload.len() >= 82 && payload[80] == 0x00 && payload[81] == 0x01
}

fn build_block_wire(
    blockstore: &crate::storage::blockstore::BlockStore,
    hash: &blvm_protocol::Hash,
    protocol_version: ProtocolVersion,
    include_witness: bool,
) -> Result<Option<Vec<u8>>> {
    let Some(data) = blockstore.load_block_blob(hash)? else {
        return Ok(None);
    };

    if is_wire_body_blob(&data) {
        let Some(payload) = decode_wire_body_blob(&data) else {
            return Ok(None);
        };
        if include_witness || !block_payload_has_witness_marker(payload) {
            return Ok(Some(ProtocolParser::serialize_command_payload(
                "block", payload,
            )?));
        }
        let Ok((block, _)) = blvm_protocol::wire::deserialize_block_with_witnesses_pair(payload)
        else {
            return Ok(None);
        };
        let stripped = blvm_protocol::wire::serialize_block_witnesses(&block, &[])?;
        return Ok(Some(ProtocolParser::serialize_command_payload(
            "block", &stripped,
        )?));
    }

    let block = crate::storage::blockstore::BlockStore::decode_block_blob(&data)?;
    let Some(n) = blockstore.get_height_by_hash(hash)? else {
        return Ok(None);
    };
    let ts = block.header.timestamp;
    let registry = cached_feature_registry(protocol_version);
    let segwit_on = registry.is_feature_active("segwit", n, ts);
    let witnesses = if include_witness {
        match blockstore.get_witness(hash)? {
            Some(w) => w,
            None if !segwit_on => Vec::new(),
            None => return Ok(None),
        }
    } else {
        Vec::new()
    };
    // Mirror IBD local load: commitment + empty stacks is a stale witness block.
    // Serving it forces the peer through EMPTY_WITNESS reject/re-getdata loops.
    if include_witness && empty_witness_unacceptable(&block, &witnesses, segwit_on) {
        return Ok(None);
    }

    let msg = ProtocolMessage::Block(BlockMessage { block, witnesses });
    Ok(Some(ProtocolParser::serialize_message(&msg)?))
}

fn witnesses_for_indexed_tx(
    txindex: &crate::storage::txindex::TxIndex,
    blockstore: &crate::storage::blockstore::BlockStore,
    hash: &blvm_protocol::Hash,
) -> Vec<blvm_protocol::segwit::Witness> {
    let Ok(Some(meta)) = txindex.get_metadata(hash) else {
        return Vec::new();
    };
    let Ok(Some(block_witnesses)) = blockstore.get_witness(&meta.block_hash) else {
        return Vec::new();
    };
    block_witnesses
        .get(meta.tx_index as usize)
        .cloned()
        .unwrap_or_default()
}

fn build_tx_wire(
    txindex: &crate::storage::txindex::TxIndex,
    blockstore: &crate::storage::blockstore::BlockStore,
    mempool_mgr: Option<&std::sync::Arc<crate::node::mempool::MempoolManager>>,
    hash: &blvm_protocol::Hash,
    include_witness: bool,
) -> Result<Option<Vec<u8>>> {
    if let Ok(Some(tx)) = txindex.get_transaction(hash) {
        let mut witnesses = if include_witness {
            witnesses_for_indexed_tx(txindex, blockstore, hash)
        } else {
            Vec::new()
        };
        if include_witness && witnesses.iter().all(|stack| stack.is_empty()) {
            if let Some(from_pool) = mempool_mgr.and_then(|mm| mm.get_transaction_witnesses(hash)) {
                if from_pool.iter().any(|stack| !stack.is_empty()) {
                    witnesses = from_pool;
                }
            }
        }
        let msg = ProtocolMessage::Tx(TxMessage {
            transaction: tx,
            witnesses,
        });
        return Ok(Some(ProtocolParser::serialize_message(&msg)?));
    }
    if let Some(mm) = mempool_mgr {
        if let Some(tx) = mm.get_transaction(hash) {
            let witnesses = if include_witness {
                mm.get_transaction_witnesses(hash).unwrap_or_default()
            } else {
                Vec::new()
            };
            let msg = ProtocolMessage::Tx(TxMessage {
                transaction: tx,
                witnesses,
            });
            return Ok(Some(ProtocolParser::serialize_message(&msg)?));
        }
    }
    Ok(None)
}

#[cfg(test)]
#[path = "getdata_serve_tests.rs"]
mod tests;
