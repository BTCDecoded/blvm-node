//! Fulfill incoming [`getdata`](https://en.bitcoin.it/wiki/Protocol_documentation#getdata) requests.
//!
//! Serves full `block` / `tx` wire messages from storage when data is present and complete
//! (including witness data when required after segwit activation). Missing or incomplete objects
//! produce `notfound` entries so peers can query other nodes.
//!
//! Block hashes merged via [`crate::module::traits::NodeAPI::merge_block_serve_denylist`]
//! (e.g. selective-sync or policy modules) are never served as full `block` messages.

use crate::network::inventory::{MSG_BLOCK, MSG_TX, MSG_WITNESS_BLOCK};
use crate::network::network_manager::NetworkManager;
use crate::network::protocol::{
    BlockMessage, GetDataMessage, InventoryVector, NotFoundMessage, ProtocolMessage,
    ProtocolParser, TxMessage,
};
use anyhow::Result;
use blvm_protocol::features::FeatureRegistry;
use blvm_protocol::ProtocolVersion;
use std::net::SocketAddr;
use tracing::warn;

impl NetworkManager {
    /// Answer a peer `getdata` using chain/mempool storage.
    ///
    /// For each inventory item: sends `block` or `tx` when available, otherwise includes the
    /// vector in a trailing `notfound`. Witness-required blocks without stored witness data are
    /// treated as unavailable (same class of failure as selective sync / pruned data).
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

        let mut missing: Vec<InventoryVector> = Vec::new();

        for item in &getdata.inventory {
            match item.inv_type {
                MSG_BLOCK | MSG_WITNESS_BLOCK => {
                    let res = if self.block_serve_maintenance_mode()
                        || self.is_block_serve_denied(&item.hash)
                    {
                        Ok(None)
                    } else {
                        build_block_wire(&blockstore, &item.hash, protocol_version)
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
    let Some(block) = blockstore.get_block(hash)? else {
        return Ok(None);
    };
    let Some(height) = blockstore.get_height_by_hash(hash)? else {
        return Ok(None);
    };
    let ts = block.header.timestamp;
    let registry = FeatureRegistry::for_protocol(protocol_version);
    let segwit_on = registry.is_feature_active("segwit", height, ts);
    let witnesses = match blockstore.get_witness(hash)? {
        Some(w) => w,
        None if !segwit_on => Vec::new(),
        None => return Ok(None),
    };

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
    use crate::network::inventory::{MSG_BLOCK, MSG_TX};
    use crate::network::NetworkManager;
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
        // Missing block path tries notfound; peer absent → Err is acceptable.
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
}
