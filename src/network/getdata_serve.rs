//! Fulfill incoming [`getdata`](https://en.bitcoin.it/wiki/Protocol_documentation#getdata) requests.
//!
//! Serves full `block` / `tx` wire messages from storage when data is present and complete
//! (including witness data when required after segwit activation). Missing or incomplete objects
//! produce `notfound` entries so peers can query other nodes.
//!
//! Block hashes merged via [`crate::module::traits::NodeAPI::merge_block_serve_denylist`]
//! (e.g. selective-sync or policy modules) are never served as full `block` messages.

use crate::network::inventory::{MSG_BLOCK, MSG_TX};
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
                MSG_BLOCK => {
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
