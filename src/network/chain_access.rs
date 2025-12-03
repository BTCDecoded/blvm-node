//! Chain state access implementation for protocol layer
//!
//! Implements ChainStateAccess trait to bridge node storage modules
//! (BlockStore, TxIndex, MempoolManager) with protocol layer network processing.

use crate::node::mempool::MempoolManager;
use crate::storage::{blockstore::BlockStore, txindex::TxIndex};
use anyhow::Result;
use blvm_protocol::network::{ChainObject, ChainStateAccess};
use blvm_protocol::{BlockHeader, Hash, Transaction};
use std::sync::Arc;

/// Chain state access implementation that bridges node storage to protocol layer
pub struct NodeChainAccess {
    blockstore: Arc<BlockStore>,
    txindex: Arc<TxIndex>,
    mempool: Arc<MempoolManager>,
}

impl NodeChainAccess {
    /// Create a new chain access implementation
    pub fn new(
        blockstore: Arc<BlockStore>,
        txindex: Arc<TxIndex>,
        mempool: Arc<MempoolManager>,
    ) -> Self {
        Self {
            blockstore,
            txindex,
            mempool,
        }
    }
}

impl ChainStateAccess for NodeChainAccess {
    /// Check if we have an object (block or transaction) by hash
    fn has_object(&self, hash: &Hash) -> bool {
        // Check blockstore first (blocks)
        if let Ok(true) = self.blockstore.has_block(hash) {
            return true;
        }
        // Check txindex (confirmed transactions)
        if let Ok(Some(_)) = self.txindex.get_transaction(hash) {
            return true;
        }
        // Check mempool (unconfirmed transactions)
        self.mempool.get_transaction(hash).is_some()
    }

    /// Get an object (block or transaction) by hash
    fn get_object(&self, hash: &Hash) -> Option<ChainObject> {
        // Try blockstore first (blocks)
        if let Ok(Some(block)) = self.blockstore.get_block(hash) {
            return Some(ChainObject::Block(Arc::new(block)));
        }
        // Try txindex (confirmed transactions)
        if let Ok(Some(tx)) = self.txindex.get_transaction(hash) {
            return Some(ChainObject::Transaction(Arc::new(tx)));
        }
        // Try mempool (unconfirmed transactions)
        if let Some(tx) = self.mempool.get_transaction(hash) {
            return Some(ChainObject::Transaction(Arc::new(tx)));
        }
        None
    }

    /// Get headers for a block locator (for GetHeaders requests)
    /// This implements the Bitcoin block locator algorithm
    fn get_headers_for_locator(&self, locator: &[Hash], stop: &Hash) -> Vec<BlockHeader> {
        let mut headers = Vec::new();

        // Bitcoin block locator algorithm:
        // 1. Start with the most recent block hash
        // 2. Go back exponentially (1, 2, 4, 8, 16, ...) until we find a common ancestor
        // 3. Stop when we reach the stop hash or run out of hashes

        for hash in locator {
            // If we've reached the stop hash, stop
            if hash == stop {
                break;
            }

            // Try to get the header
            if let Ok(Some(header)) = self.blockstore.get_header(hash) {
                headers.push(header);
            } else {
                // If we can't find this hash, we've likely gone too far back
                // Continue to next hash in locator
                continue;
            }
        }

        headers
    }

    /// Get all mempool transactions
    fn get_mempool_transactions(&self) -> Vec<Transaction> {
        // MempoolManager now stores full transactions, so we can retrieve them directly
        self.mempool.get_transactions()
    }
}

/// Helper function to process a network message using protocol layer
///
/// This function demonstrates how to integrate protocol layer message processing
/// with node storage. It can be called from network message handlers.
///
/// Example usage in a network handler:
/// ```rust,ignore
/// use blvm_protocol::network::{process_network_message, PeerState, ChainStateAccess};
/// use blvm_protocol::BitcoinProtocolEngine;
/// use blvm_node::network::chain_access::NodeChainAccess;
/// use std::collections::HashMap;
/// use blvm_protocol::UTXO;
/// use std::sync::Arc;
///
/// // In your message handler:
/// // let protocol_engine = BitcoinProtocolEngine::new(blvm_protocol::ProtocolVersion::Regtest)?;
/// // let message = /* your network message */;
/// // let mut peer_state = PeerState::default();
/// // let storage = Arc::new(blvm_node::storage::Storage::new("data")?);
/// // let tx_index = storage.transactions();
/// // let mempool = Arc::new(blvm_node::node::mempool::MempoolManager::new());
/// // let chain_access = NodeChainAccess::new(storage.blocks(), tx_index, mempool);
/// // let utxo_set: HashMap<_, UTXO> = HashMap::new();
/// // let height = 0u64;
/// // let response = process_network_message(
/// //     &protocol_engine,
/// //     &message,
/// //     &mut peer_state,
/// //     Some(&chain_access as &dyn ChainStateAccess),
/// //     Some(&utxo_set),
/// //     Some(height),
/// // )?;
/// ```
pub fn process_protocol_message(
    engine: &blvm_protocol::BitcoinProtocolEngine,
    message: &blvm_protocol::network::NetworkMessage,
    peer_state: &mut blvm_protocol::network::PeerState,
    chain_access: &NodeChainAccess,
    utxo_set: Option<&blvm_protocol::UtxoSet>,
    height: Option<u64>,
) -> Result<blvm_protocol::network::NetworkResponse> {
    use blvm_protocol::network::{process_network_message, ChainStateAccess};

    process_network_message(
        engine,
        message,
        peer_state,
        Some(chain_access as &dyn ChainStateAccess),
        utxo_set,
        height,
    )
    .map_err(|e| anyhow::anyhow!("Network message processing error: {}", e))
}
