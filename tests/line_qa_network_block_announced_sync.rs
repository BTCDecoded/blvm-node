//! Test-line QA: an announced block is not fetched when the peer is absent.

use std::sync::Arc;

use blvm_node::network::inventory::MSG_BLOCK;
use blvm_node::network::network_manager::NetworkManager;
use blvm_node::network::protocol::{InvMessage, InventoryVector, ProtocolMessage, ProtocolParser};
use blvm_node::node::mempool::MempoolManager;
use blvm_node::storage::Storage;
use blvm_protocol::{BitcoinProtocolEngine, BlockHeader, ProtocolVersion};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn announced_block_rejects_a_missing_peer() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let storage = Arc::new(Storage::new(dir.path()).expect("storage"));
    let parent = [0x11; 32];
    let child = [0x22; 32];
    let header = BlockHeader {
        version: 1,
        prev_block_hash: [0; 32],
        merkle_root: [0; 32],
        timestamp: 0,
        bits: 0x1d00ffff,
        nonce: 0,
    };
    storage
        .chain()
        .update_tip(&parent, &header, 0)
        .expect("height 0");
    let next = BlockHeader {
        prev_block_hash: parent,
        nonce: 1,
        ..header
    };
    storage
        .chain()
        .update_tip(&child, &next, 1)
        .expect("height 1");

    let protocol =
        Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).expect("protocol"));
    let manager = NetworkManager::new("127.0.0.1:0".parse().expect("addr")).with_dependencies(
        protocol,
        storage,
        Arc::new(MempoolManager::new()),
    );
    let wire = ProtocolParser::serialize_message(&ProtocolMessage::Inv(InvMessage {
        inventory: vec![InventoryVector {
            inv_type: MSG_BLOCK,
            hash: [0x33; 32],
        }],
    }))
    .expect("inv");
    let peer = "127.0.0.1:8333".parse().expect("peer");

    match manager.process_announced_block_inventory(&wire, peer).await {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("not found"),
                "process_announced_block_inventory returned {message}"
            );
        }
        Ok(()) => panic!("announced block was accepted for a peer that is not connected"),
    }
}
