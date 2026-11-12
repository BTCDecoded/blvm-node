use super::*;
use crate::network::NetworkManager;
use crate::network::inventory::{
    MSG_BLOCK, MSG_TX, MSG_WITNESS_BLOCK, MSG_WITNESS_FLAG, MSG_WITNESS_TX,
};
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
    let out = build_block_wire(
        storage.blocks().as_ref(),
        &hash,
        ProtocolVersion::Regtest,
        true,
    )
    .unwrap();
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
    let wire = build_block_wire(
        storage.blocks().as_ref(),
        &hash,
        ProtocolVersion::Regtest,
        true,
    )
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
    let out = build_block_wire(
        storage.blocks().as_ref(),
        &hash,
        ProtocolVersion::BitcoinV1,
        true,
    )
    .unwrap();
    assert!(
        out.is_none(),
        "commitment + empty witnesses must notfound (mirror IBD local load)"
    );
}

#[test]
fn witness_tx_getdata_is_a_transaction_request() {
    assert_eq!(MSG_WITNESS_TX, MSG_TX | MSG_WITNESS_FLAG);
    assert!(inventory_requests_tx(MSG_WITNESS_TX));
    assert!(inventory_requests_tx(MSG_TX));
    assert!(!inventory_requests_tx(MSG_WITNESS_BLOCK));
    assert!(!inventory_requests_tx(MSG_BLOCK));
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
    let from_index = build_tx_wire(
        storage.transactions().as_ref(),
        storage.blocks().as_ref(),
        None,
        &hash,
        true,
    )
    .unwrap();
    assert!(from_index.is_some());
    let mempool = Arc::new(MempoolManager::new());
    let unknown = [0x66u8; 32];
    assert!(
        build_tx_wire(
            storage.transactions().as_ref(),
            storage.blocks().as_ref(),
            Some(&mempool),
            &unknown,
            true,
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn confirmed_tx_wire_includes_its_block_witness() {
    use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput};
    let temp = TempDir::new().unwrap();
    let storage = Storage::new(temp.path()).unwrap();
    let tx = Transaction {
        version: 2,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [4u8; 32],
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: 0xfffffffe,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 1000,
            script_pubkey: Vec::new(),
        }]
        .into(),
        lock_time: 0,
    };
    let hash = calculate_tx_id(&tx);
    let block_hash = [0x55u8; 32];
    storage
        .transactions()
        .index_transaction(&tx, &block_hash, 7, 1)
        .unwrap();
    let spend = vec![vec![vec![1u8, 2, 3]]];
    storage
        .blocks()
        .store_witness(&block_hash, &[Vec::new(), spend.clone()])
        .unwrap();
    let bytes = build_tx_wire(
        storage.transactions().as_ref(),
        storage.blocks().as_ref(),
        None,
        &hash,
        true,
    )
    .unwrap()
    .expect("indexed tx");
    match ProtocolParser::parse_message(&bytes).unwrap() {
        ProtocolMessage::Tx(msg) => assert_eq!(msg.witnesses, spend),
        other => panic!("expected a tx message, got {other:?}"),
    }
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

#[test]
fn plain_getdata_omits_the_witness_marker() {
    use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput};
    let temp = TempDir::new().unwrap();
    let storage = Storage::new(temp.path()).unwrap();
    let tx = Transaction {
        version: 2,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [9u8; 32],
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: 0xffff_ffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 1000,
            script_pubkey: vec![0x51],
        }]
        .into(),
        lock_time: 0,
    };
    let block = Block {
        header: BlockHeader {
            version: 2,
            prev_block_hash: [0u8; 32],
            merkle_root: [3u8; 32],
            timestamp: 1_700_000_000,
            bits: 0x207f_ffff,
            nonce: 1,
        },
        transactions: vec![tx.clone()].into(),
    };
    let hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();
    storage.blocks().store_height(1, &hash).unwrap();
    storage
        .blocks()
        .store_witness(&hash, &[vec![vec![vec![0xab]]]])
        .unwrap();
    let plain = build_block_wire(
        storage.blocks().as_ref(),
        &hash,
        ProtocolVersion::Regtest,
        false,
    )
    .unwrap()
    .expect("plain block");
    let with_witness = build_block_wire(
        storage.blocks().as_ref(),
        &hash,
        ProtocolVersion::Regtest,
        true,
    )
    .unwrap()
    .expect("witness block");
    // Message header, block header, tx-count varint, then the transaction version.
    let marker_at = 24 + 80 + 1 + 4;
    assert_eq!(plain[marker_at], 0x01, "MSG_BLOCK has no witness marker");
    assert_eq!(with_witness[marker_at], 0x00);
    assert_eq!(with_witness[marker_at + 1], 0x01);
    assert!(with_witness.len() > plain.len());

    let txid = calculate_tx_id(&tx);
    storage
        .transactions()
        .index_transaction(&tx, &hash, 1, 0)
        .unwrap();
    let plain_tx = build_tx_wire(
        storage.transactions().as_ref(),
        storage.blocks().as_ref(),
        None,
        &txid,
        false,
    )
    .unwrap()
    .expect("plain tx");
    let witness_tx = build_tx_wire(
        storage.transactions().as_ref(),
        storage.blocks().as_ref(),
        None,
        &txid,
        true,
    )
    .unwrap()
    .expect("witness tx");
    assert_eq!(plain_tx[28], 0x01);
    assert_eq!(witness_tx[28], 0x00);
    assert_eq!(witness_tx[29], 0x01);
}
