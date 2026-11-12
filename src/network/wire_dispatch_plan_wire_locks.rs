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
    BitcoinProtocolEngine, BlockHeader, OutPoint, ProtocolVersion, Transaction, TransactionInput,
    TransactionOutput, UTXO, UtxoSet,
};
use std::sync::Arc;

#[test]
fn easy_self_target_does_not_meet_the_next_height() {
    let parent_time = 1_600_000_000u64;
    let mut header = BlockHeader {
        version: 4,
        prev_block_hash: [1u8; 32],
        merkle_root: [2u8; 32],
        timestamp: parent_time + 1,
        bits: 0x207fffff,
        nonce: 0,
    };
    while !blvm_protocol::pow::check_proof_of_work(&header).unwrap_or(false) {
        header.nonce = header.nonce.wrapping_add(1);
        if header.nonce == 0 {
            panic!("easy target never met");
        }
    }
    let ancestor = |h: u64| {
        if h == 100 {
            Some((0x1d00ffffu64, parent_time))
        } else {
            None
        }
    };
    assert!(!crate::network::header_work::header_meets_required_work(
        &header,
        101,
        blvm_protocol::types::Network::Mainnet,
        &ancestor,
    ));
}

struct Wired {
    manager: NetworkManager,
    peer: SocketAddr,
    incoming: tokio::sync::mpsc::Receiver<Vec<u8>>,
    storage: Arc<Storage>,
    mempool: Arc<MempoolManager>,
    _dir: tempfile::TempDir,
}

async fn wired() -> Wired {
    let dir = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
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
    state.handshake_complete = true;
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

#[tokio::test]
async fn tx_block_and_getdata_before_verack_are_ignored() {
    use crate::network::inventory::MSG_TX;
    use crate::network::protocol::{BlockMessage, TxMessage};
    use blvm_protocol::Block;

    let mut node = wired().await;
    {
        let mut states = node.manager.peer_states().write().await;
        states.get_mut(&node.peer).unwrap().handshake_complete = false;
    }
    let _ = node.manager.process_pending_messages().await.unwrap();
    let tx = ProtocolMessage::Tx(TxMessage {
        transaction: spend(
            blvm_protocol::OutPoint {
                hash: [3u8; 32],
                index: 0,
            },
            1000,
        ),
        witnesses: Vec::new(),
    });
    node.manager
        .dispatch_protocol_message(node.peer, &tx, vec![1])
        .await
        .unwrap();
    assert_eq!(node.manager.process_pending_messages().await.unwrap(), 0);
    let getdata = ProtocolMessage::GetData(GetDataMessage {
        inventory: vec![InventoryVector {
            inv_type: MSG_TX,
            hash: [9u8; 32],
        }],
    });
    node.manager
        .dispatch_protocol_message(node.peer, &getdata, Vec::new())
        .await
        .unwrap();
    assert!(node.incoming.try_recv().is_err());
    let block = Block {
        header: BlockHeader {
            version: 4,
            prev_block_hash: [0u8; 32],
            merkle_root: [5u8; 32],
            timestamp: 1_700_000_011,
            bits: 0x207fffff,
            nonce: 4,
        },
        transactions: vec![spend(
            blvm_protocol::OutPoint {
                hash: [8u8; 32],
                index: 0,
            },
            1000,
        )]
        .into(),
    };
    let mut frame = vec![0u8; 28];
    frame[16..20].copy_from_slice(&4u32.to_le_bytes());
    node.manager
        .handle_block_wire_message(
            node.peer,
            BlockMessage {
                block,
                witnesses: Vec::new(),
            },
            frame,
        )
        .await
        .unwrap();
    assert!(node.manager.try_recv_block().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsolicited_blocks_stop_at_the_in_flight_window() {
    use crate::network::protocol::BlockMessage;
    use blvm_protocol::Block;

    let node = wired().await;
    for nonce in 0..17u64 {
        let block = Block {
            header: BlockHeader {
                version: 4,
                prev_block_hash: [0u8; 32],
                merkle_root: [5u8; 32],
                timestamp: 1_700_000_011,
                bits: 0x207fffff,
                nonce,
            },
            transactions: vec![spend(
                blvm_protocol::OutPoint {
                    hash: [8u8; 32],
                    index: nonce as u32,
                },
                1000,
            )]
            .into(),
        };
        let mut frame = vec![0u8; 28];
        frame[16..20].copy_from_slice(&4u32.to_le_bytes());
        node.manager
            .handle_block_wire_message(
                node.peer,
                BlockMessage {
                    block,
                    witnesses: Vec::new(),
                },
                frame,
            )
            .await
            .unwrap();
    }
    let mut queued = Vec::new();
    while let Some(block) = node.manager.try_recv_block() {
        queued.push(block);
    }
    assert_eq!(
        queued.len(),
        16,
        "17 unsolicited blocks must leave 16 queued"
    );

    node.manager.queue_block(b"compact-completion".to_vec());
    assert_eq!(
        node.manager.try_recv_block().as_deref(),
        Some(b"compact-completion".as_slice()),
        "a direct queue_block must still append"
    );

    let requested = Block {
        header: BlockHeader {
            version: 4,
            prev_block_hash: [1u8; 32],
            merkle_root: [9u8; 32],
            timestamp: 1_700_000_099,
            bits: 0x207fffff,
            nonce: 99,
        },
        transactions: vec![spend(
            blvm_protocol::OutPoint {
                hash: [4u8; 32],
                index: 0,
            },
            1000,
        )]
        .into(),
    };
    let hash = block_hash_from_header(&requested.header);
    let mut waiting = node.manager.register_block_request(node.peer, hash);
    for nonce in 0..16u64 {
        let block = Block {
            header: BlockHeader {
                version: 4,
                prev_block_hash: [2u8; 32],
                merkle_root: [6u8; 32],
                timestamp: 1_700_000_200,
                bits: 0x207fffff,
                nonce,
            },
            transactions: vec![spend(
                blvm_protocol::OutPoint {
                    hash: [7u8; 32],
                    index: nonce as u32,
                },
                1000,
            )]
            .into(),
        };
        let mut frame = vec![0u8; 28];
        frame[16..20].copy_from_slice(&4u32.to_le_bytes());
        node.manager
            .handle_block_wire_message(
                node.peer,
                BlockMessage {
                    block,
                    witnesses: Vec::new(),
                },
                frame,
            )
            .await
            .unwrap();
    }
    let mut frame = vec![0u8; 28];
    frame[16..20].copy_from_slice(&4u32.to_le_bytes());
    node.manager
        .handle_block_wire_message(
            node.peer,
            BlockMessage {
                block: requested,
                witnesses: Vec::new(),
            },
            frame,
        )
        .await
        .unwrap();
    assert!(
        waiting.try_recv().is_ok(),
        "a requested block must not be dropped when the unsolicited queue is full"
    );
    let mut still_queued = 0;
    while node.manager.try_recv_block().is_some() {
        still_queued += 1;
    }
    assert_eq!(still_queued, 16);
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
    policy.require_standard = false;
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
    assert!(
        inventory
            .iter()
            .any(|item| { item.hash == rich_id && item.inv_type == MSG_WITNESS_TX })
    );
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

fn numbered_hash(n: u16) -> [u8; 32] {
    let mut hash = [0u8; 32];
    hash[0] = (n >> 8) as u8;
    hash[1] = n as u8;
    hash
}

#[tokio::test]
async fn a_peer_holds_one_hundred_requests_and_notfound_releases_the_next() {
    use crate::network::inventory::MSG_WITNESS_TX;
    use crate::network::protocol::NotFoundMessage;
    use crate::network::tx_inflight::MAX_PEER_IN_FLIGHT;

    let mut node = wired().await;
    let inventory: Vec<_> = (0..=MAX_PEER_IN_FLIGHT as u16)
        .map(|n| InventoryVector {
            inv_type: MSG_TX,
            hash: numbered_hash(n),
        })
        .collect();
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::Inv(InvMessage { inventory }),
            Vec::new(),
        )
        .await
        .unwrap();
    let bytes = node.incoming.recv().await.unwrap();
    let ProtocolMessage::GetData(GetDataMessage { inventory }) =
        ProtocolParser::parse_message(&bytes).unwrap()
    else {
        panic!("inv reply was not getdata");
    };
    assert_eq!(inventory.len(), MAX_PEER_IN_FLIGHT);
    assert!(inventory.iter().all(|item| item.inv_type == MSG_WITNESS_TX));
    assert!(
        inventory
            .iter()
            .all(|item| item.hash != numbered_hash(MAX_PEER_IN_FLIGHT as u16))
    );
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::NotFound(NotFoundMessage {
                inventory: vec![InventoryVector {
                    inv_type: MSG_WITNESS_TX,
                    hash: numbered_hash(0),
                }],
            }),
            Vec::new(),
        )
        .await
        .unwrap();
    let bytes = node.incoming.recv().await.unwrap();
    let ProtocolMessage::GetData(GetDataMessage { inventory }) =
        ProtocolParser::parse_message(&bytes).unwrap()
    else {
        panic!("notfound follow-up was not getdata");
    };
    assert_eq!(inventory.len(), 1);
    assert_eq!(inventory[0].hash, numbered_hash(MAX_PEER_IN_FLIGHT as u16));
    assert_eq!(inventory[0].inv_type, MSG_WITNESS_TX);
}

#[tokio::test]
async fn a_hash_already_requested_is_not_requested_again() {
    let mut node = wired().await;
    let first = [7u8; 32];
    let second = [8u8; 32];
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::Inv(InvMessage {
                inventory: vec![InventoryVector {
                    inv_type: MSG_TX,
                    hash: first,
                }],
            }),
            Vec::new(),
        )
        .await
        .unwrap();
    let _ = node.incoming.recv().await.unwrap();
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::Inv(InvMessage {
                inventory: vec![
                    InventoryVector {
                        inv_type: MSG_TX,
                        hash: first,
                    },
                    InventoryVector {
                        inv_type: MSG_WITNESS_TX,
                        hash: second,
                    },
                ],
            }),
            Vec::new(),
        )
        .await
        .unwrap();
    let bytes = node.incoming.recv().await.unwrap();
    let ProtocolMessage::GetData(GetDataMessage { inventory }) =
        ProtocolParser::parse_message(&bytes).unwrap()
    else {
        panic!("second inv reply was not getdata");
    };
    assert_eq!(inventory.len(), 1);
    assert_eq!(inventory[0].hash, second);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transaction_and_a_block_notfound_do_not_share_the_set() {
    use crate::network::inventory::MSG_BLOCK;
    use crate::network::protocol::{NotFoundMessage, TxMessage};

    let mut node = wired().await;
    let tx = spend(
        OutPoint {
            hash: [3u8; 32],
            index: 0,
        },
        1_000,
    );
    let txid = blvm_protocol::block::calculate_tx_id(&tx);
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::Inv(InvMessage {
                inventory: vec![InventoryVector {
                    inv_type: MSG_TX,
                    hash: txid,
                }],
            }),
            Vec::new(),
        )
        .await
        .unwrap();
    let _ = node.incoming.recv().await.unwrap();
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::NotFound(NotFoundMessage {
                inventory: vec![InventoryVector {
                    inv_type: MSG_BLOCK,
                    hash: txid,
                }],
            }),
            Vec::new(),
        )
        .await
        .unwrap();
    let other = [6u8; 32];
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::Inv(InvMessage {
                inventory: vec![
                    InventoryVector {
                        inv_type: MSG_TX,
                        hash: txid,
                    },
                    InventoryVector {
                        inv_type: MSG_TX,
                        hash: other,
                    },
                ],
            }),
            Vec::new(),
        )
        .await
        .unwrap();
    let bytes = node.incoming.recv().await.unwrap();
    let ProtocolMessage::GetData(GetDataMessage { inventory }) =
        ProtocolParser::parse_message(&bytes).unwrap()
    else {
        panic!("inv after block notfound was not getdata");
    };
    assert_eq!(inventory.len(), 1);
    assert_eq!(inventory[0].hash, other);
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::Tx(TxMessage {
                transaction: tx,
                witnesses: Vec::new(),
            }),
            Vec::new(),
        )
        .await
        .unwrap();
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::Inv(InvMessage {
                inventory: vec![InventoryVector {
                    inv_type: MSG_TX,
                    hash: txid,
                }],
            }),
            Vec::new(),
        )
        .await
        .unwrap();
    let bytes = node.incoming.recv().await.unwrap();
    let ProtocolMessage::GetData(GetDataMessage { inventory }) =
        ProtocolParser::parse_message(&bytes).unwrap()
    else {
        panic!("inv after tx was not getdata");
    };
    assert_eq!(inventory.len(), 1);
    assert_eq!(inventory[0].hash, txid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsolicited_header_on_the_tip_extends_the_header_chain() {
    let node = wired().await;
    let genesis = BlockHeader {
        version: 4,
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
        version: 4,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn getblocktxn_lock_reply_includes_the_stored_witness() {
    use blvm_protocol::Block;
    let mut node = wired().await;
    let tx = spend(
        OutPoint {
            hash: [9u8; 32],
            index: 0,
        },
        1000,
    );
    let block = Block {
        header: BlockHeader {
            version: 4,
            prev_block_hash: [0u8; 32],
            merkle_root: [4u8; 32],
            timestamp: 1_700_000_010,
            bits: 0x207fffff,
            nonce: 3,
        },
        transactions: vec![tx].into(),
    };
    let hash = node.storage.blocks().get_block_hash(&block);
    node.storage.blocks().store_block(&block).unwrap();
    let tx_witnesses = vec![vec![vec![0x09u8, 0x08, 0x07]]];
    let stacks = vec![tx_witnesses];
    node.storage.blocks().store_witness(&hash, &stacks).unwrap();
    node.manager
        .handle_getblocktxn(
            node.peer,
            &crate::network::protocol::GetBlockTxnMessage {
                block_hash: hash,
                indices: vec![0],
            },
        )
        .await
        .unwrap();
    let bytes = node.incoming.recv().await.unwrap();
    match ProtocolParser::parse_message(&bytes).unwrap() {
        ProtocolMessage::BlockTxn(msg) => {
            assert_eq!(msg.witnesses.as_ref(), Some(&stacks));
        }
        other => panic!("expected blocktxn, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tip_getdata_lock_serves_two_blocks_inside_the_hour() {
    use crate::network::inventory::MSG_BLOCK;
    use blvm_protocol::Block;
    let mut node = wired().await;
    let block = Block {
        header: BlockHeader {
            version: 4,
            prev_block_hash: [0u8; 32],
            merkle_root: [5u8; 32],
            timestamp: 1_700_000_011,
            bits: 0x207fffff,
            nonce: 4,
        },
        transactions: vec![spend(
            OutPoint {
                hash: [8u8; 32],
                index: 0,
            },
            1000,
        )]
        .into(),
    };
    let hash = node.storage.blocks().get_block_hash(&block);
    node.storage.blocks().store_block(&block).unwrap();
    node.storage.blocks().store_height(0, &hash).unwrap();
    node.storage.blocks().store_witness(&hash, &[]).unwrap();
    let getdata = ProtocolMessage::GetData(GetDataMessage {
        inventory: vec![InventoryVector {
            inv_type: MSG_BLOCK,
            hash,
        }],
    });
    for _ in 0..2 {
        node.manager
            .dispatch_protocol_message(node.peer, &getdata, Vec::new())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let bytes = node.incoming.recv().await.unwrap();
        match ProtocolParser::parse_message(&bytes).unwrap() {
            ProtocolMessage::Block(_) => {}
            other => panic!("expected a block, got {other:?}"),
        }
    }
    let reputation = node
        .manager
        .ibd_protection()
        .get_peer_reputation(node.peer)
        .await
        .unwrap_or(0);
    assert_eq!(reputation, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn getblocks_lock_69_byte_locator_returns_the_next_header() {
    use crate::network::inventory::MSG_BLOCK;
    use crate::network::protocol::GetBlocksMessage;
    let mut node = wired().await;
    let header_zero = BlockHeader {
        version: 4,
        prev_block_hash: [0u8; 32],
        merkle_root: [6u8; 32],
        timestamp: 1_700_000_012,
        bits: 0x207fffff,
        nonce: 5,
    };
    let header_one = BlockHeader {
        version: 4,
        prev_block_hash: [0x11; 32],
        merkle_root: [7u8; 32],
        timestamp: 1_700_000_013,
        bits: 0x207fffff,
        nonce: 6,
    };
    node.storage
        .blocks()
        .store_headers_batch(&[
            ([0x11; 32], header_zero.clone(), 0),
            ([0x33; 32], header_one.clone(), 1),
        ])
        .unwrap();
    // GetBlocks stops at the first header that has no body. This lock is the
    // 69-byte locator, so both headers need a stored body.
    for header in [header_zero, header_one.clone()] {
        let block = blvm_protocol::Block {
            header,
            transactions: vec![spend(
                OutPoint {
                    hash: [9u8; 32],
                    index: 0,
                },
                1000,
            )]
            .into(),
        };
        node.storage.blocks().store_block(&block).unwrap();
    }
    let mut payload = vec![0x80, 0x11, 0x01, 0x00, 0x01];
    payload.extend(std::iter::repeat(0x11).take(32));
    payload.extend(std::iter::repeat(0x22).take(32));
    assert_eq!(payload.len(), 69);
    let framed = ProtocolParser::serialize_message(&ProtocolMessage::GetBlocks(GetBlocksMessage {
        version: 70016,
        block_locator_hashes: vec![[0x11; 32]],
        hash_stop: [0x22; 32],
    }))
    .unwrap();
    assert_eq!(&framed[24..], payload.as_slice());
    let parsed = ProtocolParser::parse_message(&framed).unwrap();
    node.manager
        .dispatch_protocol_message(node.peer, &parsed, Vec::new())
        .await
        .unwrap();
    let bytes = node.incoming.recv().await.unwrap();
    let next_hash = blvm_consensus::block::block_header_hash(&header_one);
    match ProtocolParser::parse_message(&bytes).unwrap() {
        ProtocolMessage::Inv(InvMessage { inventory }) => {
            assert!(
                inventory
                    .iter()
                    .any(|item| { item.inv_type == MSG_BLOCK && item.hash == next_hash })
            );
        }
        other => panic!("expected inv, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn getblocks_reply_for_a_witness_block_uses_witness_inventory() {
    use crate::network::inventory::{MSG_BLOCK, MSG_WITNESS_BLOCK};
    use crate::network::protocol::GetBlocksMessage;
    let mut node = wired().await;
    let header = BlockHeader {
        version: 4,
        prev_block_hash: [0u8; 32],
        merkle_root: [7u8; 32],
        timestamp: 1_700_000_013,
        bits: 0x207fffff,
        nonce: 6,
    };
    let block = blvm_protocol::Block {
        header: header.clone(),
        transactions: vec![spend(
            OutPoint {
                hash: [9u8; 32],
                index: 0,
            },
            1000,
        )]
        .into(),
    };
    let hash = blvm_consensus::block::block_header_hash(&header);
    node.storage.blocks().store_block(&block).unwrap();
    node.storage.blocks().store_height(0, &hash).unwrap();
    node.storage
        .blocks()
        .store_witness(&hash, &[vec![vec![vec![0x01]]]])
        .unwrap();
    let parsed = ProtocolMessage::GetBlocks(GetBlocksMessage {
        version: 70016,
        block_locator_hashes: vec![],
        hash_stop: hash,
    });
    node.manager
        .dispatch_protocol_message(node.peer, &parsed, Vec::new())
        .await
        .unwrap();
    let bytes = node.incoming.recv().await.unwrap();
    match ProtocolParser::parse_message(&bytes).unwrap() {
        ProtocolMessage::Inv(InvMessage { inventory }) => {
            assert!(
                inventory
                    .iter()
                    .any(|item| item.inv_type == MSG_WITNESS_BLOCK && item.hash == hash)
            );
            assert!(inventory.iter().all(|item| item.inv_type != MSG_BLOCK));
        }
        other => panic!("expected inv, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn announce_lock_block_and_tx_leave_as_inv() {
    use crate::network::inventory::MSG_WITNESS_TX;
    use crate::network::protocol::HeadersMessage;
    let mut node = wired().await;
    let header = BlockHeader {
        version: 4,
        prev_block_hash: [0u8; 32],
        merkle_root: [8u8; 32],
        timestamp: 1_700_000_014,
        bits: 0x207fffff,
        nonce: 7,
    };
    let block_hash = [0x44; 32];
    node.storage.blocks().store_height(1, &block_hash).unwrap();
    node.manager
        .peer_states()
        .write()
        .await
        .get_mut(&node.peer)
        .unwrap()
        .known_header_height = Some(0);
    node.manager
        .announce_connected_block(block_hash, header.clone())
        .await;
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(5), node.incoming.recv())
        .await
        .expect("headers announcement")
        .unwrap();
    match ProtocolParser::parse_message(&bytes).unwrap() {
        ProtocolMessage::Headers(HeadersMessage { headers }) => {
            assert_eq!(headers, vec![header]);
        }
        other => panic!("expected headers, got {other:?}"),
    }
    let txid = [0x55; 32];
    node.manager.announce_accepted_transaction(txid, None).await;
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(5), node.incoming.recv())
        .await
        .expect("tx inv")
        .unwrap();
    match ProtocolParser::parse_message(&bytes).unwrap() {
        ProtocolMessage::Inv(InvMessage { inventory }) => {
            assert!(
                inventory
                    .iter()
                    .any(|item| item.inv_type == MSG_WITNESS_TX && item.hash == txid)
            );
        }
        other => panic!("expected tx inv, got {other:?}"),
    }
}

#[tokio::test]
async fn rejected_child_before_parent_does_not_enter_the_mempool() {
    use crate::network::protocol::PkgTxnMessage;
    use blvm_protocol::block::calculate_tx_id;

    let node = wired().await;
    let mut policy = crate::config::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_relay_fee_rate = 0;
    policy.min_tx_fee = 0;
    node.mempool.set_policy_config(Some(policy));
    let prev = OutPoint {
        hash: [7u8; 32],
        index: 0,
    };
    let mut parent = spend(prev, 40_000);
    parent.inputs[0].script_sig = vec![OP_1];
    parent.outputs[0].script_pubkey = vec![OP_1];
    let parent_id = calculate_tx_id(&parent);
    let mut child = spend(
        OutPoint {
            hash: parent_id,
            index: 0,
        },
        20_000,
    );
    child.inputs[0].script_sig = vec![OP_1];
    let mut set = UtxoSet::default();
    set.insert(
        prev,
        Arc::new(UTXO {
            value: 50_000,
            script_pubkey: vec![OP_1].into(),
            height: 1,
            is_coinbase: false,
        }),
    );
    node.mempool.set_utxo_set_arc(node.manager.utxo_set_arc());
    node.manager.replace_utxo_snapshot(set).await;
    let msg = PkgTxnMessage {
        package_id: vec![1u8; 32],
        transactions: vec![
            bincode::serialize(&child).unwrap(),
            bincode::serialize(&parent).unwrap(),
        ],
    };
    let wire = ProtocolParser::serialize_message(&ProtocolMessage::PkgTxn(msg)).unwrap();
    node.manager
        .handle_pkgtxn_request(wire, node.peer)
        .await
        .unwrap();
    assert!(node.mempool.get_transaction(&parent_id).is_none());
    assert!(node.mempool.add_transaction(parent).unwrap());
}

#[tokio::test]
async fn empty_locator_getheaders_does_not_take_an_ibd_slot() {
    use crate::network::protocol::{GetHeadersMessage, HeadersMessage};

    let mut node = wired().await;
    let genesis = BlockHeader {
        version: 4,
        prev_block_hash: [0u8; 32],
        merkle_root: [1u8; 32],
        timestamp: 1_700_000_000,
        bits: 0x207fffff,
        nonce: 1,
    };
    let genesis_hash = blvm_consensus::block::block_header_hash(&genesis);
    let next = BlockHeader {
        version: 4,
        prev_block_hash: genesis_hash,
        merkle_root: [2u8; 32],
        timestamp: 1_700_000_001,
        bits: 0x207fffff,
        nonce: 2,
    };
    let next_hash = blvm_consensus::block::block_header_hash(&next);
    node.storage
        .blocks()
        .store_headers_batch(&[(genesis_hash, genesis.clone(), 0), (next_hash, next, 1)])
        .unwrap();
    for _ in 0..3 {
        let msg = ProtocolMessage::GetHeaders(GetHeadersMessage {
            version: 70015,
            block_locator_hashes: vec![],
            hash_stop: [0u8; 32],
        });
        node.manager
            .dispatch_protocol_message(node.peer, &msg, Vec::new())
            .await
            .unwrap();
    }
    assert_eq!(
        node.manager
            .ibd_protection()
            .concurrent_ibd_serving_count()
            .await,
        0
    );
    assert_eq!(
        node.manager
            .ibd_protection()
            .get_peer_reputation(node.peer)
            .await
            .unwrap_or(0),
        0
    );
    assert!(
        node.manager
            .ibd_protection()
            .can_serve_ibd(node.peer)
            .await
            .unwrap()
    );
    for _ in 0..3 {
        let bytes = node.incoming.recv().await.unwrap();
        match ProtocolParser::parse_message(&bytes).unwrap() {
            ProtocolMessage::Headers(HeadersMessage { headers }) => {
                assert!(
                    headers.is_empty(),
                    "an empty locator with a zero hash_stop returns no headers"
                );
            }
            other => panic!("expected headers, got {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unsolicited_header_without_proof_is_not_stored() {
    use crate::network::protocol::HeadersMessage;

    let node = wired().await;
    let tip_hash = [4u8; 32];
    let tip = BlockHeader {
        version: 4,
        prev_block_hash: [0u8; 32],
        merkle_root: [1u8; 32],
        timestamp: 1_700_000_000,
        bits: 0x207fffff,
        nonce: 3,
    };
    node.storage
        .chain()
        .cache_block_chainwork(&tip.prev_block_hash, &tip, 0)
        .unwrap();
    node.storage.chain().update_tip(&tip_hash, &tip, 5).unwrap();
    let mut indexed = Vec::new();
    let mut prev_hash = [0u8; 32];
    for height in 0..5u64 {
        let header = BlockHeader {
            version: 4,
            prev_block_hash: prev_hash,
            merkle_root: [height as u8; 32],
            timestamp: 1_700_000_000 - (5 - height) * 600,
            bits: 0x207fffff,
            nonce: height,
        };
        let hash = blvm_consensus::block::block_header_hash(&header);
        indexed.push((hash, header, height));
        prev_hash = hash;
    }
    indexed.push((tip_hash, tip.clone(), 5));
    node.storage.blocks().store_headers_batch(&indexed).unwrap();
    let bad = BlockHeader {
        version: 4,
        prev_block_hash: tip_hash,
        merkle_root: [8u8; 32],
        timestamp: 1_700_000_010,
        bits: 1,
        nonce: 0,
    };
    let msg = ProtocolMessage::Headers(HeadersMessage { headers: vec![bad] });
    node.manager
        .dispatch_protocol_message(node.peer, &msg, Vec::new())
        .await
        .unwrap();
    assert!(
        node.storage
            .blocks()
            .get_hash_by_height(6)
            .unwrap()
            .is_none()
    );

    let mut good = BlockHeader {
        version: 4,
        prev_block_hash: tip_hash,
        merkle_root: [9u8; 32],
        timestamp: 1_700_000_011,
        bits: 0x207fffff,
        nonce: 0,
    };
    for nonce in 0..200_000 {
        good.nonce = nonce;
        if blvm_protocol::pow::check_proof_of_work(&good).unwrap_or(false) {
            break;
        }
    }
    let msg = ProtocolMessage::Headers(HeadersMessage {
        headers: vec![good.clone()],
    });
    node.manager
        .dispatch_protocol_message(node.peer, &msg, Vec::new())
        .await
        .unwrap();
    let stored = node
        .storage
        .blocks()
        .get_header_at_height(6)
        .unwrap()
        .expect("header with proof");
    assert_eq!(stored.merkle_root, good.merkle_root);
}

#[test]
fn fee_filter_keeps_a_fractional_satoshi() {
    assert!(fee_rate_meets_filter(1999, 1000, 1_500));
    assert!(!fee_rate_meets_filter(1000, 1000, 1_500));
    assert!(fee_rate_meets_filter(1000, 1000, 1_000));
    assert!(!fee_rate_meets_filter(0, 1000, 999));
}

#[test]
fn missing_fee_basis_is_omitted_from_a_filtered_mempool_inv() {
    assert!(!mempool_inv_includes(None, Some(1_000)));
    assert!(mempool_inv_includes(Some((1_000, 1)), Some(1_000)));
    assert!(mempool_inv_includes(None, None));
}

#[test]
fn mempool_inv_chunks_stay_within_fifty_thousand() {
    let items: Vec<u32> = (0..50_001).collect();
    let chunks = chunk_inventory(items.clone(), 50_000);
    assert!(chunks.iter().all(|chunk| chunk.len() <= 50_000));
    assert_eq!(chunks.iter().map(Vec::len).sum::<usize>(), 50_001);
    assert_eq!(chunks.into_iter().flatten().collect::<Vec<_>>(), items);
}

fn header() -> BlockHeader {
    BlockHeader {
        version: 4,
        prev_block_hash: [0u8; 32],
        merkle_root: [1u8; 32],
        timestamp: 1_700_000_000,
        bits: 0x207f_ffff,
        nonce: 1,
    }
}

#[tokio::test]
async fn compact_relay_accepts_twelve_thousand_and_rejects_past_fifty_thousand() {
    use crate::network::protocol::{BlockTxnMessage, CompactBlockMessage, GetBlockTxnMessage};
    let node = wired().await;
    let compact = blvm_protocol::bip152::CompactBlock {
        header: header(),
        nonce: 1,
        short_ids: vec![[7u8; 6]; 12_239],
        prefilled_txs: Vec::new(),
    };
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::CmpctBlock(CompactBlockMessage {
                compact_block: compact,
            }),
            Vec::new(),
        )
        .await
        .expect("12239 short ids stay connected");
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::GetBlockTxn(GetBlockTxnMessage {
                block_hash: [3u8; 32],
                indices: vec![1u16; 12_239],
            }),
            Vec::new(),
        )
        .await
        .expect("12239 indexes stay connected");
    let tx = spend(
        OutPoint {
            hash: [4u8; 32],
            index: 0,
        },
        1,
    );
    let transactions = vec![tx; 12_239];
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::BlockTxn(BlockTxnMessage {
                block_hash: [5u8; 32],
                transactions,
                witnesses: None,
            }),
            Vec::new(),
        )
        .await
        .expect("12239 blocktxn transactions stay connected");

    let too_many = blvm_protocol::bip152::CompactBlock {
        header: header(),
        nonce: 2,
        short_ids: vec![[8u8; 6]; 50_001],
        prefilled_txs: Vec::new(),
    };
    let err = node
        .manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::CmpctBlock(CompactBlockMessage {
                compact_block: too_many,
            }),
            Vec::new(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("too many"), "{err}");
}

#[tokio::test]
async fn fee_filter_rates_gate_the_one_sat_transaction() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::segwit::transaction_weight_from_stacks;

    let mut node = wired().await;
    let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 1;
    node.mempool.set_policy_config(Some(policy));
    let prev = OutPoint {
        hash: [1u8; 32],
        index: 0,
    };
    let mut tx = spend(prev, 50_000);
    tx.inputs[0].script_sig = vec![OP_1];
    let vsize = transaction_weight_from_stacks(&tx, None)
        .unwrap()
        .div_ceil(4)
        .max(1) as i64;
    tx.outputs[0].value = 50_000;
    let mut set = UtxoSet::default();
    set.insert(
        prev,
        Arc::new(UTXO {
            value: 50_000 + vsize,
            script_pubkey: vec![OP_1].into(),
            height: 1,
            is_coinbase: false,
        }),
    );
    node.mempool
        .set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
    assert!(node.mempool.add_transaction(tx.clone()).unwrap());
    let txid = calculate_tx_id(&tx);
    for (feerate, offered) in [(1_500u64, false), (1_000, true), (999, true)] {
        node.manager
            .dispatch_protocol_message(
                node.peer,
                &ProtocolMessage::FeeFilter(FeeFilterMessage { feerate }),
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
        assert!(inventory.len() <= 50_000);
        assert_eq!(inventory.iter().any(|item| item.hash == txid), offered);
    }
}

#[tokio::test]
async fn getaddr_reply_stops_at_one_thousand() {
    use crate::network::protocol::NetworkAddress;
    let mut node = wired().await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;
    {
        let mut db = node.manager.address_database().write().await;
        for i in 1u32..=2500 {
            let mut ip = [0u8; 16];
            ip[10] = 0xff;
            ip[11] = 0xff;
            ip[12] = 1;
            ip[13] = 2;
            ip[14] = (i >> 8) as u8;
            ip[15] = i as u8;
            db.add_address(
                NetworkAddress {
                    services: 1,
                    ip,
                    port: 8333,
                    time: now,
                },
                1,
            );
        }
    }
    node.manager
        .dispatch_protocol_message(node.peer, &ProtocolMessage::GetAddr, Vec::new())
        .await
        .unwrap();
    let bytes = node.incoming.recv().await.unwrap();
    let parsed = ProtocolParser::parse_message(&bytes).unwrap();
    let ProtocolMessage::Addr(msg) = parsed else {
        panic!("getaddr reply was not addr");
    };
    assert_eq!(msg.addresses.len(), 1000);
}

#[tokio::test]
async fn getblocks_stops_at_the_first_header_without_a_body() {
    use crate::network::protocol::GetBlocksMessage;
    let mut node = wired().await;
    let mut prev = [0u8; 32];
    let mut indexed = Vec::new();
    for height in 0u64..=6 {
        let header = BlockHeader {
            version: 4,
            prev_block_hash: prev,
            merkle_root: [height as u8; 32],
            timestamp: 1_700_000_000 + height,
            bits: 0x207f_ffff,
            nonce: height,
        };
        let hash = blvm_consensus::block::block_header_hash(&header);
        if height != 5 {
            let block = blvm_protocol::Block {
                header: header.clone(),
                transactions: Vec::new().into(),
            };
            node.storage.blocks().store_block(&block).unwrap();
        }
        indexed.push((hash, header, height));
        prev = hash;
    }
    node.storage.blocks().store_headers_batch(&indexed).unwrap();
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::GetBlocks(GetBlocksMessage {
                version: 70015,
                block_locator_hashes: vec![indexed[0].0],
                hash_stop: [0u8; 32],
            }),
            Vec::new(),
        )
        .await
        .unwrap();
    let bytes = node.incoming.recv().await.unwrap();
    let parsed = ProtocolParser::parse_message(&bytes).unwrap();
    let ProtocolMessage::Inv(InvMessage { inventory }) = parsed else {
        panic!("getblocks reply was not an inv");
    };
    let hashes: Vec<[u8; 32]> = inventory.iter().map(|item| item.hash).collect();
    assert!(hashes.contains(&indexed[4].0));
    assert!(!hashes.contains(&indexed[5].0));
    assert!(!hashes.contains(&indexed[6].0));
}

#[tokio::test]
async fn verack_asks_the_peer_for_headers() {
    let mut node = wired().await;
    node.manager
        .dispatch_protocol_message(node.peer, &ProtocolMessage::Verack, Vec::new())
        .await
        .unwrap();
    let mut saw_headers = false;
    for _ in 0..4 {
        let bytes = tokio::time::timeout(std::time::Duration::from_secs(2), node.incoming.recv())
            .await
            .expect("handshake reply")
            .expect("message");
        if matches!(
            ProtocolParser::parse_message(&bytes),
            Ok(ProtocolMessage::SendHeaders)
        ) {
            saw_headers = true;
            break;
        }
    }
    assert!(saw_headers, "verack sends sendheaders");
}

#[tokio::test]
async fn accepted_tx_is_announced_and_a_rejected_tx_is_not() {
    use blvm_protocol::block::calculate_tx_id;

    let mut node = wired().await;
    let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;
    node.mempool.set_policy_config(Some(policy));
    let prev = OutPoint {
        hash: [7u8; 32],
        index: 0,
    };
    let mut set = UtxoSet::default();
    set.insert(
        prev,
        Arc::new(UTXO {
            value: 50_000,
            script_pubkey: vec![OP_1].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    node.mempool
        .set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
    let tx = spend(prev, 40_000);
    node.manager
        .submit_transaction_with_witness(tx.clone(), None)
        .await
        .unwrap();
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(2), node.incoming.recv())
        .await
        .expect("announcement")
        .expect("inv");
    match ProtocolParser::parse_message(&bytes).unwrap() {
        ProtocolMessage::Inv(InvMessage { inventory }) => {
            assert!(inventory.iter().any(|item| {
                item.inv_type == MSG_WITNESS_TX && item.hash == calculate_tx_id(&tx)
            }));
        }
        other => panic!("expected inv, got {other:?}"),
    }
    let missing = spend(
        OutPoint {
            hash: [8u8; 32],
            index: 0,
        },
        1_000,
    );
    node.manager
        .submit_transaction_with_witness(missing, None)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), node.incoming.recv())
            .await
            .is_err(),
        "a rejected transaction is not announced"
    );
}

#[tokio::test]
async fn sendrawtransaction_announces_an_accepted_transaction() {
    use blvm_protocol::block::calculate_tx_id;

    let dir = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;
    mempool.set_policy_config(Some(policy));
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let listen: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let manager = Arc::new(NetworkManager::new(listen).with_dependencies(
        protocol,
        Arc::clone(&storage),
        Arc::clone(&mempool),
    ));
    let peer: SocketAddr = "127.0.0.1:18445".parse().unwrap();
    let (peer_conn, mut incoming) = Peer::pair_for_testing(peer);
    manager
        .peer_manager_mutex()
        .lock()
        .await
        .add_peer(TransportAddr::Tcp(peer), peer_conn)
        .unwrap();
    let mut state = PeerState::new();
    state.version = 70015;
    manager.peer_states().write().await.insert(peer, state);

    let prev = OutPoint {
        hash: [4u8; 32],
        index: 0,
    };
    let utxo = UTXO {
        value: 50_000,
        script_pubkey: vec![OP_1].into(),
        height: 0,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&prev, &utxo).unwrap();
    let mut set = UtxoSet::default();
    set.insert(prev, Arc::new(utxo));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
    let tx = spend(prev, 40_000);
    let raw = blvm_protocol::serialization::serialize_transaction(&tx);
    let rpc =
        crate::rpc::rawtx::RawTxRpc::with_dependencies(Arc::clone(&storage), mempool, None, None)
            .with_network(Some(manager));
    rpc.sendrawtransaction(&serde_json::json!([hex::encode(raw)]))
        .await
        .unwrap();
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(2), incoming.recv())
        .await
        .expect("announcement")
        .expect("inv");
    match ProtocolParser::parse_message(&bytes).unwrap() {
        ProtocolMessage::Inv(InvMessage { inventory }) => {
            assert!(inventory.iter().any(|item| {
                item.inv_type == MSG_WITNESS_TX && item.hash == calculate_tx_id(&tx)
            }));
        }
        other => panic!("expected inv, got {other:?}"),
    }
}

#[tokio::test]
async fn verack_sends_the_relay_fee_floor() {
    let mut node = wired().await;
    node.manager
        .dispatch_protocol_message(node.peer, &ProtocolMessage::Verack, Vec::new())
        .await
        .unwrap();
    let rate = next_feefilter(&mut node.incoming).await;
    assert_eq!(rate, 100);
    let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_relay_fee_rate = 3_000;
    node.mempool.set_policy_config(Some(policy));
    node.manager
        .dispatch_protocol_message(node.peer, &ProtocolMessage::Verack, Vec::new())
        .await
        .unwrap();
    let rate = next_feefilter(&mut node.incoming).await;
    assert_eq!(rate, 3_000);
}

async fn next_feefilter(incoming: &mut tokio::sync::mpsc::Receiver<Vec<u8>>) -> u64 {
    for _ in 0..4 {
        let bytes = incoming.recv().await.expect("handshake reply");
        if let Ok(ProtocolMessage::FeeFilter(msg)) = ProtocolParser::parse_message(&bytes) {
            return msg.feerate;
        }
    }
    panic!("no feefilter after verack");
}

fn version_message(relay: bool) -> crate::network::protocol::VersionMessage {
    use crate::network::protocol::{NetworkAddress, VersionMessage};
    let addr = NetworkAddress {
        time: 0,
        services: 0,
        ip: [0u8; 16],
        port: 0,
    };
    VersionMessage {
        version: 70016,
        services: 0,
        timestamp: 0,
        addr_recv: addr.clone(),
        addr_from: addr,
        nonce: 9,
        user_agent: String::new(),
        start_height: 0,
        relay,
    }
}

async fn announce_reaches(node: &mut Wired) -> bool {
    while node.incoming.try_recv().is_ok() {}
    node.manager
        .announce_accepted_transaction([6u8; 32], Some((1, 1)))
        .await;
    loop {
        match node.incoming.try_recv() {
            Ok(bytes) => {
                if let Ok(ProtocolMessage::Inv(_)) = ProtocolParser::parse_message(&bytes) {
                    return true;
                }
            }
            Err(_) => return false,
        }
    }
}

#[tokio::test]
async fn version_relay_byte_gates_transaction_announcements() {
    let mut silent = wired().await;
    {
        let mut states = silent.manager.peer_states().write().await;
        states.get_mut(&silent.peer).unwrap().version = 0;
    }
    silent
        .manager
        .dispatch_protocol_message(
            silent.peer,
            &ProtocolMessage::Version(version_message(false)),
            Vec::new(),
        )
        .await
        .unwrap();
    assert!(!announce_reaches(&mut silent).await);

    let mut speaking = wired().await;
    {
        let mut states = speaking.manager.peer_states().write().await;
        states.get_mut(&speaking.peer).unwrap().version = 0;
    }
    speaking
        .manager
        .dispatch_protocol_message(
            speaking.peer,
            &ProtocolMessage::Version(version_message(true)),
            Vec::new(),
        )
        .await
        .unwrap();
    assert!(announce_reaches(&mut speaking).await);

    let mut omitted = wired().await;
    {
        let mut states = omitted.manager.peer_states().write().await;
        states.get_mut(&omitted.peer).unwrap().version = 0;
    }
    let mut raw = blvm_protocol::wire::serialize_version(&blvm_protocol::network::VersionMessage {
        version: 70016,
        services: 0,
        timestamp: 0,
        addr_recv: blvm_protocol::network::NetworkAddress {
            services: 0,
            ip: [0u8; 16],
            port: 0,
            time: 0,
        },
        addr_from: blvm_protocol::network::NetworkAddress {
            services: 0,
            ip: [0u8; 16],
            port: 0,
            time: 0,
        },
        nonce: 4,
        user_agent: String::new(),
        start_height: 0,
        relay: false,
    })
    .unwrap();
    raw.pop();
    let parsed = blvm_protocol::wire::deserialize_version(&raw).unwrap();
    assert!(parsed.relay);
    omitted
        .manager
        .dispatch_protocol_message(
            omitted.peer,
            &ProtocolMessage::Version(version_message(parsed.relay)),
            Vec::new(),
        )
        .await
        .unwrap();
    assert!(announce_reaches(&mut omitted).await);
}

#[tokio::test]
async fn a_block_relay_only_peer_is_not_sent_transaction_inv() {
    use crate::network::peer::OutboundRole;
    let mut node = wired().await;
    {
        let mut pm = node.manager.peer_manager_mutex().lock().await;
        pm.get_peer_mut(&TransportAddr::Tcp(node.peer))
            .unwrap()
            .set_outbound_role(Some(OutboundRole::BlockRelayOnly));
    }
    {
        let mut states = node.manager.peer_states().write().await;
        states.get_mut(&node.peer).unwrap().version = 0;
    }
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::Version(version_message(true)),
            Vec::new(),
        )
        .await
        .unwrap();
    let relay_tx = node
        .manager
        .peer_states()
        .read()
        .await
        .get(&node.peer)
        .unwrap()
        .relay_tx;
    assert!(!relay_tx);
    while node.incoming.try_recv().is_ok() {}
    node.manager
        .dispatch_protocol_message(node.peer, &ProtocolMessage::MemPool, Vec::new())
        .await
        .unwrap();
    assert!(node.incoming.try_recv().is_err());
    assert!(!announce_reaches(&mut node).await);
}

#[tokio::test]
async fn wtxidrelay_selects_type_5_and_a_peer_without_it_stays_on_witness_tx() {
    use crate::network::inventory::{MSG_WITNESS_TX, MSG_WTX, tx_announce_inv_type};
    use crate::network::protocol::cmd;

    assert_eq!(MSG_WTX, 5);
    assert_eq!(tx_announce_inv_type(false), MSG_WITNESS_TX);
    assert_eq!(tx_announce_inv_type(true), MSG_WTX);
    let empty = ProtocolParser::serialize_message(&ProtocolMessage::WtxidRelay).unwrap();
    assert!(matches!(
        ProtocolParser::parse_message(&empty).unwrap(),
        ProtocolMessage::WtxidRelay
    ));
    let nonempty = ProtocolParser::serialize_command_payload(cmd::WTXIDRELAY, &[1]).unwrap();
    let err = ProtocolParser::parse_message(&nonempty).unwrap_err();
    assert!(err.to_string().contains("Unknown command"));

    let mut node = wired().await;
    {
        let mut states = node.manager.peer_states().write().await;
        let state = states.get_mut(&node.peer).unwrap();
        state.version = 0;
        state.handshake_complete = false;
        state.wtxid_relay = false;
    }
    node.manager
        .dispatch_protocol_message(node.peer, &ProtocolMessage::WtxidRelay, Vec::new())
        .await
        .unwrap();
    assert!(!wtxid_flag(&node).await);

    {
        let mut states = node.manager.peer_states().write().await;
        let state = states.get_mut(&node.peer).unwrap();
        state.version = 70016;
        state.handshake_complete = true;
    }
    node.manager
        .dispatch_protocol_message(node.peer, &ProtocolMessage::WtxidRelay, Vec::new())
        .await
        .unwrap();
    assert!(!wtxid_flag(&node).await);

    {
        let mut states = node.manager.peer_states().write().await;
        states.get_mut(&node.peer).unwrap().handshake_complete = false;
    }
    node.manager
        .dispatch_protocol_message(node.peer, &ProtocolMessage::WtxidRelay, Vec::new())
        .await
        .unwrap();
    assert!(wtxid_flag(&node).await);
    node.manager
        .dispatch_protocol_message(node.peer, &ProtocolMessage::WtxidRelay, Vec::new())
        .await
        .unwrap();
    assert!(wtxid_flag(&node).await);

    let missing = [9u8; 32];
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::Inv(InvMessage {
                inventory: vec![InventoryVector {
                    inv_type: MSG_TX,
                    hash: missing,
                }],
            }),
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(next_inv_type(&mut node.incoming).await, MSG_WTX);
    while node.incoming.try_recv().is_ok() {}
    node.manager
        .announce_accepted_transaction([4u8; 32], None)
        .await;
    assert_eq!(next_inv_type(&mut node.incoming).await, MSG_WTX);

    let mut plain = wired().await;
    plain
        .manager
        .dispatch_protocol_message(
            plain.peer,
            &ProtocolMessage::Inv(InvMessage {
                inventory: vec![InventoryVector {
                    inv_type: MSG_WITNESS_TX,
                    hash: [8u8; 32],
                }],
            }),
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(next_inv_type(&mut plain.incoming).await, MSG_WITNESS_TX);
    while plain.incoming.try_recv().is_ok() {}
    plain
        .manager
        .announce_accepted_transaction([5u8; 32], None)
        .await;
    assert_eq!(next_inv_type(&mut plain.incoming).await, MSG_WITNESS_TX);
}

async fn wtxid_flag(node: &Wired) -> bool {
    node.manager
        .peer_states()
        .read()
        .await
        .get(&node.peer)
        .unwrap()
        .wtxid_relay
}

async fn next_inv_type(incoming: &mut tokio::sync::mpsc::Receiver<Vec<u8>>) -> u32 {
    let bytes = incoming.recv().await.expect("inventory");
    match ProtocolParser::parse_message(&bytes).unwrap() {
        ProtocolMessage::GetData(GetDataMessage { inventory })
        | ProtocolMessage::Inv(InvMessage { inventory }) => inventory[0].inv_type,
        _ => panic!("reply was not inventory"),
    }
}

#[tokio::test]
async fn sendpackages_is_accepted_only_before_verack() {
    use crate::network::package_messages::{PACKAGE_TX, PACKAGE_VERSION_LOCAL, package_enabled};
    let mut node = wired().await;
    {
        let mut states = node.manager.peer_states().write().await;
        let state = states.get_mut(&node.peer).unwrap();
        state.version = 0;
        state.handshake_complete = false;
        state.package_remote = None;
    }
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::SendPackages(PACKAGE_VERSION_LOCAL),
            Vec::new(),
        )
        .await
        .unwrap();
    assert!(package_remote(&node).await.is_none());
    {
        let mut states = node.manager.peer_states().write().await;
        let state = states.get_mut(&node.peer).unwrap();
        state.version = 70016;
        state.handshake_complete = true;
    }
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::SendPackages(PACKAGE_TX),
            Vec::new(),
        )
        .await
        .unwrap();
    assert!(package_remote(&node).await.is_none());
    {
        let mut states = node.manager.peer_states().write().await;
        states.get_mut(&node.peer).unwrap().handshake_complete = false;
    }
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::SendPackages(PACKAGE_TX),
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(package_remote(&node).await, Some(PACKAGE_TX));
    assert_eq!(package_enabled(PACKAGE_TX), PACKAGE_TX);
    node.manager
        .dispatch_protocol_message(
            node.peer,
            &ProtocolMessage::SendPackages(PACKAGE_VERSION_LOCAL),
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(package_remote(&node).await, Some(PACKAGE_TX));
}

async fn package_remote(node: &Wired) -> Option<u64> {
    node.manager
        .peer_states()
        .read()
        .await
        .get(&node.peer)
        .unwrap()
        .package_remote
}
