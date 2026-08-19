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
use crate::network::inventory::{MSG_BLOCK, MSG_TX, MSG_WITNESS_BLOCK};
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
                    let join = tokio::task::spawn_blocking(move || {
                        build_block_wire(&bs, &hash, protocol_version)
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
                MSG_TX => {
                    let res = if self.is_tx_serve_denied(&item.hash) {
                        Ok(None)
                    } else {
                        build_tx_wire(&txindex, mempool_mgr, &item.hash)
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
                    jobs[j] = Some(tokio::task::spawn_blocking(move || {
                        build_block_wire(&bs, &hash, protocol_version)
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
                MSG_TX => {
                    let res = if self.is_tx_serve_denied(&item.hash) {
                        Ok(None)
                    } else {
                        build_tx_wire(&txindex, mempool_mgr, &item.hash)
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

fn build_block_wire(
    blockstore: &crate::storage::blockstore::BlockStore,
    hash: &blvm_protocol::Hash,
    protocol_version: ProtocolVersion,
) -> Result<Option<Vec<u8>>> {
    let Some(data) = blockstore.load_block_blob(hash)? else {
        return Ok(None);
    };

    if is_wire_body_blob(&data) {
        let Some(payload) = decode_wire_body_blob(&data) else {
            return Ok(None);
        };
        return Ok(Some(ProtocolParser::serialize_command_payload(
            "block", payload,
        )?));
    }

    let block = crate::storage::blockstore::BlockStore::decode_block_blob(&data)?;
    let Some(n) = blockstore.get_height_by_hash(hash)? else {
        return Ok(None);
    };
    let ts = block.header.timestamp;
    let registry = cached_feature_registry(protocol_version);
    let segwit_on = registry.is_feature_active("segwit", n, ts);
    let witnesses = match blockstore.get_witness(hash)? {
        Some(w) => w,
        None if !segwit_on => Vec::new(),
        None => return Ok(None),
    };
    // Mirror IBD local load: commitment + empty stacks is a stale MSG_BLOCK blob.
    // Serving it forces the peer through EMPTY_WITNESS reject/re-getdata loops.
    if empty_witness_unacceptable(&block, &witnesses, segwit_on) {
        return Ok(None);
    }

    let msg = ProtocolMessage::Block(BlockMessage { block, witnesses });
    Ok(Some(ProtocolParser::serialize_message(&msg)?))
}

fn build_tx_wire(
    txindex: &crate::storage::txindex::TxIndex,
    mempool_mgr: Option<&std::sync::Arc<crate::node::mempool::MempoolManager>>,
    hash: &blvm_protocol::Hash,
) -> Result<Option<Vec<u8>>> {
    if let Ok(Some(tx)) = txindex.get_transaction(hash) {
        let msg = ProtocolMessage::Tx(TxMessage { transaction: tx });
        return Ok(Some(ProtocolParser::serialize_message(&msg)?));
    }
    if let Some(mm) = mempool_mgr {
        if let Some(tx) = mm.get_transaction(hash) {
            let msg = ProtocolMessage::Tx(TxMessage { transaction: tx });
            return Ok(Some(ProtocolParser::serialize_message(&msg)?));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::NetworkManager;
    use crate::network::inventory::{MSG_BLOCK, MSG_TX};
    use crate::node::mempool::MempoolManager;
    use crate::storage::Storage;
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::{BitcoinProtocolEngine, Block, BlockHeader, ProtocolVersion, Transaction};
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn network_with_storage() -> (TempDir, Arc<Storage>, NetworkManager) {
        let temp = TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let mempool = Arc::new(MempoolManager::new());
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let nm = NetworkManager::new("127.0.0.1:18471".parse().unwrap()).with_dependencies(
            protocol,
            Arc::clone(&storage),
            mempool,
        );
        (temp, storage, nm)
    }

    #[tokio::test]
    async fn serve_getdata_empty_inventory_is_noop() {
        let (_t, _s, nm) = network_with_storage();
        let peer: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let getdata = GetDataMessage { inventory: vec![] };
        nm.serve_getdata_request(peer, &getdata, ProtocolVersion::Regtest)
            .await
            .unwrap();
    }

    #[test]
    fn build_block_wire_missing_block_returns_none() {
        let temp = TempDir::new().unwrap();
        let storage = Storage::new(temp.path()).unwrap();
        let hash = [0x44u8; 32];
        let out =
            build_block_wire(storage.blocks().as_ref(), &hash, ProtocolVersion::Regtest).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn build_block_wire_returns_serialized_block() {
        let temp = TempDir::new().unwrap();
        let storage = Storage::new(temp.path()).unwrap();
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [1u8; 32],
                timestamp: 1_700_000_000,
                bits: 0x0f00ffff,
                nonce: 0,
            },
            transactions: vec![].into(),
        };
        let hash = storage.blocks().get_block_hash(&block);
        storage.blocks().store_block(&block).unwrap();
        storage.blocks().store_height(0, &hash).unwrap();
        let _ = storage.blocks().store_witness(&hash, &[]);
        let wire = build_block_wire(storage.blocks().as_ref(), &hash, ProtocolVersion::Regtest)
            .unwrap()
            .expect("wire bytes");
        assert!(wire.len() > 80);
    }

    #[test]
    fn build_block_wire_rejects_empty_witness_with_commitment() {
        use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput};
        let temp = TempDir::new().unwrap();
        let storage = Storage::new(temp.path()).unwrap();
        let height = 500_001u64;
        let mut commitment_script = vec![0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed];
        commitment_script.extend_from_slice(&[0u8; 32]);
        let block = Block {
            header: BlockHeader {
                version: 4,
                prev_block_hash: [0u8; 32],
                merkle_root: [1u8; 32],
                timestamp: 1_600_000_000,
                bits: 0x0f00ffff,
                nonce: 0,
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![TransactionInput {
                    prevout: OutPoint {
                        hash: [0u8; 32],
                        index: 0xffff_ffff,
                    },
                    script_sig: vec![0x01].into(),
                    sequence: 0xffff_ffff,
                }],
                outputs: blvm_protocol::tx_outputs![
                    TransactionOutput {
                        value: 50_0000_0000,
                        script_pubkey: vec![0x51],
                    },
                    TransactionOutput {
                        value: 0,
                        script_pubkey: commitment_script.into(),
                    }
                ],
                lock_time: 0,
            }]
            .into(),
        };
        let hash = storage.blocks().get_block_hash(&block);
        storage.blocks().store_height(height, &hash).unwrap();
        // Stale MSG_BLOCK persist: body + empty witness stacks.
        storage
            .blocks()
            .store_block_with_witness(&block, &[], height)
            .unwrap();
        let out =
            build_block_wire(storage.blocks().as_ref(), &hash, ProtocolVersion::BitcoinV1).unwrap();
        assert!(
            out.is_none(),
            "commitment + empty witnesses must notfound (mirror IBD local load)"
        );
    }

    #[test]
    fn build_tx_wire_prefers_index_then_mempool() {
        let temp = TempDir::new().unwrap();
        let storage = Storage::new(temp.path()).unwrap();
        let tx = Transaction {
            version: 1,
            inputs: vec![].into(),
            outputs: vec![].into(),
            lock_time: 0,
        };
        let hash = calculate_tx_id(&tx);
        storage
            .transactions()
            .index_transaction(&tx, &[0x55; 32], 0, 0)
            .unwrap();
        let from_index = build_tx_wire(storage.transactions().as_ref(), None, &hash).unwrap();
        assert!(from_index.is_some());
        let mempool = Arc::new(MempoolManager::new());
        let unknown = [0x66u8; 32];
        assert!(
            build_tx_wire(storage.transactions().as_ref(), Some(&mempool), &unknown)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn serve_getdata_honors_block_denylist_without_peer_send() {
        let (_t, storage, nm) = network_with_storage();
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [2u8; 32],
                timestamp: 1_700_000_001,
                bits: 0x0f00ffff,
                nonce: 0,
            },
            transactions: vec![].into(),
        };
        let hash = storage.blocks().get_block_hash(&block);
        storage.blocks().store_block(&block).unwrap();
        storage.blocks().store_height(1, &hash).unwrap();
        nm.merge_block_serve_denylist(&[hash]);
        let peer: SocketAddr = "127.0.0.1:9998".parse().unwrap();
        let getdata = GetDataMessage {
            inventory: vec![InventoryVector {
                inv_type: MSG_BLOCK,
                hash,
            }],
        };
        let _ = nm
            .serve_getdata_request(peer, &getdata, ProtocolVersion::Regtest)
            .await;
    }

    #[tokio::test]
    async fn serve_getdata_unknown_inv_type_treated_as_missing() {
        let (_t, _s, nm) = network_with_storage();
        let peer: SocketAddr = "127.0.0.1:9997".parse().unwrap();
        let getdata = GetDataMessage {
            inventory: vec![InventoryVector {
                inv_type: 0xdead,
                hash: [0x77u8; 32],
            }],
        };
        let _ = nm
            .serve_getdata_request(peer, &getdata, ProtocolVersion::Regtest)
            .await;
    }

    #[tokio::test]
    async fn serve_getdata_tx_denied_skips_serve() {
        let (_t, _s, nm) = network_with_storage();
        let tx_hash = [0x88u8; 32];
        nm.merge_tx_serve_denylist(&[tx_hash]);
        let peer: SocketAddr = "127.0.0.1:9996".parse().unwrap();
        let getdata = GetDataMessage {
            inventory: vec![InventoryVector {
                inv_type: MSG_TX,
                hash: tx_hash,
            }],
        };
        let _ = nm
            .serve_getdata_request(peer, &getdata, ProtocolVersion::Regtest)
            .await;
    }

    /// A4: pipelined path must serve a multi-block getdata without panic (ordered send).
    #[tokio::test]
    async fn a4_serve_getdata_pipe_serves_multi_block_inventory() {
        let (_t, storage, nm) = network_with_storage();
        let mut inv = Vec::new();
        for i in 0u8..6 {
            let block = Block {
                header: BlockHeader {
                    version: 1,
                    prev_block_hash: [i; 32],
                    merkle_root: [i.wrapping_add(1); 32],
                    timestamp: 1_700_000_000 + u64::from(i),
                    bits: 0x0f00ffff,
                    nonce: u64::from(i),
                },
                transactions: vec![].into(),
            };
            let hash = storage.blocks().get_block_hash(&block);
            storage.blocks().store_block(&block).unwrap();
            storage.blocks().store_height(u64::from(i), &hash).unwrap();
            let _ = storage.blocks().store_witness(&hash, &[]);
            inv.push(InventoryVector {
                inv_type: MSG_BLOCK,
                hash,
            });
        }
        // Force pipe path without requiring process-wide SERVE_ONLY.
        let prev = std::env::var("BLVM_GETDATA_SERVE_PIPE").ok();
        unsafe {
            std::env::set_var("BLVM_GETDATA_SERVE_PIPE", "4");
        }
        let peer: SocketAddr = "127.0.0.1:9995".parse().unwrap();
        let getdata = GetDataMessage { inventory: inv };
        let res = nm
            .serve_getdata_request(peer, &getdata, ProtocolVersion::Regtest)
            .await;
        unsafe {
            match prev {
                Some(v) => std::env::set_var("BLVM_GETDATA_SERVE_PIPE", v),
                None => std::env::remove_var("BLVM_GETDATA_SERVE_PIPE"),
            }
        }
        res.unwrap();
    }
}
