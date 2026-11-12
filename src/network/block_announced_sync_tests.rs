use super::*;
use crate::network::transport::TransportAddr;
use crate::node::mempool::MempoolManager;
use crate::storage::Storage;
use blvm_protocol::{BitcoinProtocolEngine, BlockHeader, ProtocolVersion};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

#[test]
fn a_header_gap_past_eight_is_not_announced() {
    assert_eq!(header_announce_len(100, None), None);
    assert_eq!(header_announce_len(100, Some(100)), None);
    assert_eq!(header_announce_len(100, Some(99)), Some(1));
    assert_eq!(header_announce_len(100, Some(92)), Some(8));
    assert_eq!(header_announce_len(100, Some(91)), None);
}

#[test]
fn high_bandwidth_keeps_three_and_does_not_clear_a_peer_that_was_never_selected() {
    use blvm_protocol::network::PeerState;
    use std::collections::HashMap;
    let mut states = HashMap::new();
    for host in 1u8..=5 {
        let mut state = PeerState::new();
        state.handshake_complete = true;
        states.insert(SocketAddr::from(([10, 0, 0, host], 8333)), state);
    }
    let picked = high_bandwidth_candidates(&states);
    assert_eq!(picked.len(), MAX_HIGH_BANDWIDTH_PEERS);
    for addr in &picked {
        states.get_mut(addr).unwrap().high_bandwidth = true;
    }
    assert!(high_bandwidth_candidates(&states).is_empty());
    let never = states
        .keys()
        .copied()
        .find(|addr| !picked.contains(addr))
        .unwrap();
    assert!(!revoke_high_bandwidth(states.get_mut(&never).unwrap()));
    assert!(revoke_high_bandwidth(states.get_mut(&picked[0]).unwrap()));
    assert!(!states.get(&picked[0]).unwrap().high_bandwidth);
    assert_eq!(high_bandwidth_candidates(&states).len(), 1);
}

async fn read_frame(sock: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut header = [0u8; 24];
    sock.read_exact(&mut header).await.unwrap();
    let len = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
    let mut frame = header.to_vec();
    let mut payload = vec![0u8; len];
    sock.read_exact(&mut payload).await.unwrap();
    frame.extend(payload);
    frame
}

fn connect_tip(storage: &Storage, tip_hash: &Hash, tip_header: &BlockHeader, height: u64) {
    if height > 0 {
        storage
            .chain()
            .cache_block_chainwork(&tip_header.prev_block_hash, tip_header, 0)
            .unwrap();
        // Regtest walks minimum-difficulty bits back to the period start. The tip
        // alone does not answer that lookup. The height-0 row is not the tip's prev.
        let root = BlockHeader {
            version: 4,
            prev_block_hash: [0u8; 32],
            merkle_root: [4u8; 32],
            timestamp: tip_header.timestamp.saturating_sub(1),
            bits: tip_header.bits,
            nonce: 0,
        };
        storage
            .blocks()
            .store_headers_batch(&[
                ([0x5a; 32], root, 0),
                (*tip_hash, tip_header.clone(), height),
            ])
            .unwrap();
    }
    storage
        .chain()
        .update_tip(tip_hash, tip_header, height)
        .unwrap();
}

fn with_proof(mut header: BlockHeader) -> BlockHeader {
    for nonce in 0..200_000 {
        header.nonce = nonce;
        if blvm_protocol::pow::check_proof_of_work(&header).unwrap_or(false) {
            return header;
        }
    }
    panic!("header did not meet the regtest target");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inv_lock_two_headers_stores_and_requests_first_block() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let tip_hash = [9u8; 32];
    let tip_header = BlockHeader {
        version: 4,
        prev_block_hash: [0u8; 32],
        merkle_root: [1u8; 32],
        timestamp: 1_700_000_000,
        bits: 0x207fffff,
        nonce: 0,
    };
    connect_tip(&storage, &tip_hash, &tip_header, 1);

    let header_one = with_proof(BlockHeader {
        version: 4,
        prev_block_hash: tip_hash,
        merkle_root: [2u8; 32],
        timestamp: 1_700_000_001,
        bits: 0x207fffff,
        nonce: 0,
    });
    let hash_one = blvm_consensus::block::block_header_hash(&header_one);
    let header_two = with_proof(BlockHeader {
        version: 4,
        prev_block_hash: hash_one,
        merkle_root: [3u8; 32],
        timestamp: 1_700_000_002,
        bits: 0x207fffff,
        nonce: 0,
    });
    let hash_two = blvm_consensus::block::block_header_hash(&header_two);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = listener.local_addr().unwrap();
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let nm = Arc::new(
        NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
            protocol,
            Arc::clone(&storage),
            Arc::new(MempoolManager::new()),
        ),
    );

    let nm_read = Arc::clone(&nm);
    let announced = vec![header_one.clone(), header_two.clone()];
    let reader = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut getdata = None;
        for _ in 0..4 {
            let frame = read_frame(&mut sock).await;
            match ProtocolParser::parse_message(&frame).unwrap() {
                ProtocolMessage::GetHeaders(_) => {
                    assert!(nm_read.complete_headers_request(peer_addr, announced.clone()));
                }
                ProtocolMessage::GetData(msg) => {
                    getdata = Some(msg);
                    break;
                }
                _ => {}
            }
        }
        getdata.expect("getdata")
    });

    nm.connect_to_peer(peer_addr).await.unwrap();
    let inv = ProtocolMessage::Inv(crate::network::protocol::InvMessage {
        inventory: vec![
            InventoryVector {
                inv_type: MSG_BLOCK,
                hash: hash_one,
            },
            InventoryVector {
                inv_type: MSG_BLOCK,
                hash: hash_two,
            },
        ],
    });
    let inv_bytes = ProtocolParser::serialize_message(&inv).unwrap();
    timeout(
        Duration::from_secs(10),
        nm.process_announced_block_inventory(&inv_bytes, peer_addr),
    )
    .await
    .expect("headers probe")
    .unwrap();

    let getdata = timeout(Duration::from_secs(5), reader)
        .await
        .expect("reader")
        .unwrap();
    assert_eq!(getdata.inventory.len(), 2);
    assert_eq!(getdata.inventory[0].hash, hash_one);
    assert_eq!(getdata.inventory[1].hash, hash_two);
    assert_eq!(getdata.inventory[0].inv_type, MSG_WITNESS_BLOCK);

    let stored_one = storage
        .blocks()
        .get_header_at_height(2)
        .unwrap()
        .expect("first header");
    let stored_two = storage
        .blocks()
        .get_header_at_height(3)
        .unwrap()
        .expect("second header");
    assert_eq!(stored_one.prev_block_hash, tip_hash);
    assert_eq!(stored_two.prev_block_hash, hash_one);
    assert!(
        !crate::node::parallel_ibd::PARALLEL_IBD_SESSION_ACTIVE
            .load(std::sync::atomic::Ordering::Acquire),
        "parallel IBD stays idle for this gap"
    );
}

async fn best_height(nm: &NetworkManager, peer: SocketAddr) -> Option<u64> {
    let pm = nm.peer_manager().await;
    let transport = pm
        .find_transport_addr_by_socket(peer)
        .unwrap_or(TransportAddr::Tcp(peer));
    pm.get_peer(&transport)
        .and_then(|peer| peer.best_block_height())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_height_uses_the_stored_header_count() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let tip_hash = [9u8; 32];
    let tip_header = BlockHeader {
        version: 4,
        prev_block_hash: [0u8; 32],
        merkle_root: [1u8; 32],
        timestamp: 1_700_000_000,
        bits: 0x207fffff,
        nonce: 0,
    };
    connect_tip(&storage, &tip_hash, &tip_header, 1);
    let linking = with_proof(BlockHeader {
        version: 4,
        prev_block_hash: tip_hash,
        merkle_root: [2u8; 32],
        timestamp: 1_700_000_001,
        bits: 0x207fffff,
        nonce: 0,
    });
    let unlinked = BlockHeader {
        version: 4,
        prev_block_hash: [0u8; 32],
        merkle_root: [3u8; 32],
        timestamp: 1_700_000_002,
        bits: 0x207fffff,
        nonce: 4,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = listener.local_addr().unwrap();
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let nm = Arc::new(
        NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
            protocol,
            Arc::clone(&storage),
            Arc::new(MempoolManager::new()),
        ),
    );
    let nm_read = Arc::clone(&nm);
    let reply = vec![linking, unlinked];
    let reader = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let frame = read_frame(&mut sock).await;
        match ProtocolParser::parse_message(&frame).unwrap() {
            ProtocolMessage::GetHeaders(_) => {
                assert!(nm_read.complete_headers_request(peer_addr, reply));
            }
            other => panic!("expected getheaders, got {other:?}"),
        }
    });
    nm.connect_to_peer(peer_addr).await.unwrap();
    let probed = timeout(
        Duration::from_secs(5),
        nm.refresh_peer_tip_from(peer_addr, 1, tip_hash),
    )
    .await
    .expect("probe")
    .unwrap()
    .expect("headers");
    timeout(Duration::from_secs(5), reader)
        .await
        .expect("reader")
        .unwrap();
    assert_eq!(probed.peer_height, 2);
    assert_eq!(best_height(&nm, peer_addr).await, Some(2));
    assert!(storage.blocks().get_hash_by_height(3).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlinked_probe_leaves_peer_height_unchanged() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let tip_hash = [9u8; 32];
    let tip_header = BlockHeader {
        version: 4,
        prev_block_hash: [0u8; 32],
        merkle_root: [1u8; 32],
        timestamp: 1_700_000_000,
        bits: 0x207fffff,
        nonce: 0,
    };
    connect_tip(&storage, &tip_hash, &tip_header, 1);
    let unlinked = BlockHeader {
        version: 4,
        prev_block_hash: [1u8; 32],
        merkle_root: [3u8; 32],
        timestamp: 1_700_000_002,
        bits: 0x207fffff,
        nonce: 4,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = listener.local_addr().unwrap();
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let nm = Arc::new(
        NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
            protocol,
            storage,
            Arc::new(MempoolManager::new()),
        ),
    );
    let nm_read = Arc::clone(&nm);
    let reader = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let frame = read_frame(&mut sock).await;
        match ProtocolParser::parse_message(&frame).unwrap() {
            ProtocolMessage::GetHeaders(_) => {
                assert!(nm_read.complete_headers_request(peer_addr, vec![unlinked]));
            }
            other => panic!("expected getheaders, got {other:?}"),
        }
    });
    nm.connect_to_peer(peer_addr).await.unwrap();
    let probed = timeout(
        Duration::from_secs(5),
        nm.refresh_peer_tip_from(peer_addr, 1, tip_hash),
    )
    .await
    .expect("probe")
    .unwrap()
    .expect("headers");
    timeout(Duration::from_secs(5), reader)
        .await
        .expect("reader")
        .unwrap();
    assert_eq!(probed.peer_height, 1);
    assert!(best_height(&nm, peer_addr).await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_headers_reply_does_not_claim_the_local_height() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let tip_hash = [9u8; 32];
    let tip_header = BlockHeader {
        version: 4,
        prev_block_hash: [0u8; 32],
        merkle_root: [1u8; 32],
        timestamp: 1_700_000_000,
        bits: 0x207fffff,
        nonce: 0,
    };
    connect_tip(&storage, &tip_hash, &tip_header, 1);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = listener.local_addr().unwrap();
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let nm = Arc::new(
        NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
            protocol,
            storage,
            Arc::new(MempoolManager::new()),
        ),
    );
    let nm_read = Arc::clone(&nm);
    let reader = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let frame = read_frame(&mut sock).await;
        match ProtocolParser::parse_message(&frame).unwrap() {
            ProtocolMessage::GetHeaders(_) => {
                assert!(nm_read.complete_headers_request(peer_addr, Vec::new()));
            }
            other => panic!("expected getheaders, got {other:?}"),
        }
    });
    nm.connect_to_peer(peer_addr).await.unwrap();
    let probed = timeout(
        Duration::from_secs(5),
        nm.refresh_peer_tip_from(peer_addr, 1, tip_hash),
    )
    .await
    .expect("probe")
    .unwrap()
    .expect("headers");
    timeout(Duration::from_secs(5), reader)
        .await
        .expect("reader")
        .unwrap();
    assert_eq!(probed.peer_height, 0);
    assert!(probed.first_missing_hash.is_none());
    assert!(best_height(&nm, peer_addr).await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ancestor_header_is_stored_without_replacing_the_tip_height() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let parent = with_proof(BlockHeader {
        version: 4,
        prev_block_hash: [0u8; 32],
        merkle_root: [4u8; 32],
        timestamp: 1_700_000_000,
        bits: 0x207fffff,
        nonce: 0,
    });
    let parent_hash = blvm_consensus::block::block_header_hash(&parent);
    storage
        .blocks()
        .store_header(&parent_hash, &parent)
        .unwrap();
    storage.blocks().store_height(0, &parent_hash).unwrap();
    let tip_hash = [9u8; 32];
    let tip_header = BlockHeader {
        version: 4,
        prev_block_hash: parent_hash,
        merkle_root: [1u8; 32],
        timestamp: 1_700_000_001,
        bits: 0x207fffff,
        nonce: 1,
    };
    connect_tip(&storage, &tip_hash, &tip_header, 1);
    storage.blocks().store_height(1, &tip_hash).unwrap();
    let sibling = with_proof(BlockHeader {
        version: 4,
        prev_block_hash: parent_hash,
        merkle_root: [8u8; 32],
        timestamp: 1_700_000_002,
        bits: 0x207fffff,
        nonce: 0,
    });
    let sibling_hash = blvm_consensus::block::block_header_hash(&sibling);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = listener.local_addr().unwrap();
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let nm = Arc::new(
        NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
            protocol,
            Arc::clone(&storage),
            Arc::new(MempoolManager::new()),
        ),
    );
    let nm_read = Arc::clone(&nm);
    let sibling_reply = sibling.clone();
    let reader = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut getdata = None;
        for _ in 0..4 {
            let frame = read_frame(&mut sock).await;
            match ProtocolParser::parse_message(&frame).unwrap() {
                ProtocolMessage::GetHeaders(msg) => {
                    assert!(msg.block_locator_hashes.contains(&parent_hash));
                    assert!(msg.block_locator_hashes.contains(&tip_hash));
                    assert!(
                        nm_read.complete_headers_request(peer_addr, vec![sibling_reply.clone()])
                    );
                }
                ProtocolMessage::GetData(msg) => {
                    getdata = Some(msg);
                    break;
                }
                _ => {}
            }
        }
        getdata.expect("getdata for the stored sibling")
    });
    nm.connect_to_peer(peer_addr).await.unwrap();
    let announced = [0xcdu8; 32];
    let inv = ProtocolMessage::Inv(crate::network::protocol::InvMessage {
        inventory: vec![InventoryVector {
            inv_type: MSG_BLOCK,
            hash: announced,
        }],
    });
    let inv_bytes = ProtocolParser::serialize_message(&inv).unwrap();
    timeout(
        Duration::from_secs(5),
        nm.process_announced_block_inventory(&inv_bytes, peer_addr),
    )
    .await
    .expect("inv")
    .unwrap();
    let getdata = timeout(Duration::from_secs(5), reader)
        .await
        .expect("reader")
        .unwrap();
    assert_eq!(getdata.inventory.len(), 1);
    assert_eq!(getdata.inventory[0].hash, sibling_hash);
    assert_ne!(getdata.inventory[0].hash, announced);
    assert_eq!(
        storage.blocks().get_hash_by_height(1).unwrap().unwrap(),
        tip_hash
    );
    assert!(
        storage
            .blocks()
            .get_header(&sibling_hash)
            .unwrap()
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heavier_fork_caches_chainwork_and_requests_both_bodies() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let parent = with_proof(BlockHeader {
        version: 4,
        prev_block_hash: [0u8; 32],
        merkle_root: [4u8; 32],
        timestamp: 1_700_000_000,
        bits: 0x207fffff,
        nonce: 0,
    });
    let parent_hash = blvm_consensus::block::block_header_hash(&parent);
    storage
        .blocks()
        .store_header(&parent_hash, &parent)
        .unwrap();
    storage.blocks().store_height(0, &parent_hash).unwrap();
    let tip_header = BlockHeader {
        version: 4,
        prev_block_hash: parent_hash,
        merkle_root: [1u8; 32],
        timestamp: 1_700_000_001,
        bits: 0x207fffff,
        nonce: 1,
    };
    let tip_hash = blvm_consensus::block::block_header_hash(&tip_header);
    connect_tip(&storage, &tip_hash, &tip_header, 1);
    storage.blocks().store_height(1, &tip_hash).unwrap();
    storage
        .blocks()
        .store_header(&tip_hash, &tip_header)
        .unwrap();
    let sibling = with_proof(BlockHeader {
        version: 4,
        prev_block_hash: parent_hash,
        merkle_root: [8u8; 32],
        timestamp: 1_700_000_002,
        bits: 0x207fffff,
        nonce: 0,
    });
    let sibling_hash = blvm_consensus::block::block_header_hash(&sibling);
    let child = with_proof(BlockHeader {
        version: 4,
        prev_block_hash: sibling_hash,
        merkle_root: [9u8; 32],
        timestamp: 1_700_000_003,
        bits: 0x207fffff,
        nonce: 0,
    });
    let child_hash = blvm_consensus::block::block_header_hash(&child);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = listener.local_addr().unwrap();
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let nm = Arc::new(
        NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
            protocol,
            Arc::clone(&storage),
            Arc::new(MempoolManager::new()),
        ),
    );
    let nm_read = Arc::clone(&nm);
    let reply = vec![sibling, child];
    let reader = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut getdata = None;
        for _ in 0..4 {
            let frame = read_frame(&mut sock).await;
            match ProtocolParser::parse_message(&frame).unwrap() {
                ProtocolMessage::GetHeaders(_) => {
                    assert!(nm_read.complete_headers_request(peer_addr, reply.clone()));
                }
                ProtocolMessage::GetData(msg) => {
                    getdata = Some(msg);
                    break;
                }
                _ => {}
            }
        }
        getdata.expect("getdata for the heavier fork")
    });
    nm.connect_to_peer(peer_addr).await.unwrap();
    let inv = ProtocolMessage::Inv(crate::network::protocol::InvMessage {
        inventory: vec![InventoryVector {
            inv_type: MSG_BLOCK,
            hash: [0x11u8; 32],
        }],
    });
    let inv_bytes = ProtocolParser::serialize_message(&inv).unwrap();
    timeout(
        Duration::from_secs(5),
        nm.process_announced_block_inventory(&inv_bytes, peer_addr),
    )
    .await
    .expect("inv")
    .unwrap();
    let getdata = timeout(Duration::from_secs(5), reader)
        .await
        .expect("reader")
        .unwrap();
    assert_eq!(
        getdata
            .inventory
            .iter()
            .map(|item| item.hash)
            .collect::<Vec<_>>(),
        vec![sibling_hash, child_hash]
    );
    assert_eq!(
        storage.blocks().get_hash_by_height(1).unwrap().unwrap(),
        tip_hash
    );
    assert!(
        storage.blocks().get_hash_by_height(2).unwrap().is_none(),
        "the heavier child must not become the next active height while height 1 is the other block"
    );
    let tip_work = storage.chain().get_chainwork(&tip_hash).unwrap().unwrap();
    let sibling_work = storage
        .chain()
        .get_chainwork(&sibling_hash)
        .unwrap()
        .expect("sibling chainwork cached");
    let child_work = storage
        .chain()
        .get_chainwork(&child_hash)
        .unwrap()
        .expect("child chainwork cached");
    assert_eq!(sibling_work, tip_work);
    assert!(child_work > tip_work);
    assert!(
        !crate::node::chain_selector::should_activate_over_active_tip(&storage, &sibling_hash)
            .unwrap()
    );
    assert!(
        crate::node::chain_selector::should_activate_over_active_tip(&storage, &child_hash)
            .unwrap()
    );
}

/// av3: the other block at the tip height is indexed by hash only. Its child
/// must not be stored as the next active height. That child was coinbase-only,
/// so connecting it did not spend anything from our tip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn side_child_does_not_claim_the_next_height() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let parent = with_proof(BlockHeader {
        version: 4,
        prev_block_hash: [0u8; 32],
        merkle_root: [4u8; 32],
        timestamp: 1_700_000_000,
        bits: 0x207fffff,
        nonce: 0,
    });
    let parent_hash = blvm_consensus::block::block_header_hash(&parent);
    storage
        .blocks()
        .store_header(&parent_hash, &parent)
        .unwrap();
    storage.blocks().store_height(0, &parent_hash).unwrap();
    let tip_header = BlockHeader {
        version: 4,
        prev_block_hash: parent_hash,
        merkle_root: [1u8; 32],
        timestamp: 1_700_000_001,
        bits: 0x207fffff,
        nonce: 1,
    };
    let tip_hash = blvm_consensus::block::block_header_hash(&tip_header);
    connect_tip(&storage, &tip_hash, &tip_header, 1);
    storage.blocks().store_height(1, &tip_hash).unwrap();
    storage
        .blocks()
        .store_header(&tip_hash, &tip_header)
        .unwrap();
    let side = with_proof(BlockHeader {
        version: 4,
        prev_block_hash: parent_hash,
        merkle_root: [8u8; 32],
        timestamp: 1_700_000_002,
        bits: 0x207fffff,
        nonce: 0,
    });
    let side_hash = blvm_consensus::block::block_header_hash(&side);
    storage.blocks().store_header(&side_hash, &side).unwrap();
    storage.blocks().store_hash_height(1, &side_hash).unwrap();
    let child = with_proof(BlockHeader {
        version: 4,
        prev_block_hash: side_hash,
        merkle_root: [9u8; 32],
        timestamp: 1_700_000_003,
        bits: 0x207fffff,
        nonce: 0,
    });
    let child_hash = blvm_consensus::block::block_header_hash(&child);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = listener.local_addr().unwrap();
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let nm = Arc::new(
        NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
            protocol,
            Arc::clone(&storage),
            Arc::new(MempoolManager::new()),
        ),
    );
    let nm_read = Arc::clone(&nm);
    let reply = vec![child];
    let reader = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut getdata = None;
        for _ in 0..4 {
            let frame = read_frame(&mut sock).await;
            match ProtocolParser::parse_message(&frame).unwrap() {
                ProtocolMessage::GetHeaders(_) => {
                    assert!(nm_read.complete_headers_request(peer_addr, reply.clone()));
                }
                ProtocolMessage::GetData(msg) => {
                    getdata = Some(msg);
                    break;
                }
                _ => {}
            }
        }
        getdata.expect("getdata for the side child")
    });
    nm.connect_to_peer(peer_addr).await.unwrap();
    let inv = ProtocolMessage::Inv(crate::network::protocol::InvMessage {
        inventory: vec![InventoryVector {
            inv_type: MSG_BLOCK,
            hash: [0x11u8; 32],
        }],
    });
    let inv_bytes = ProtocolParser::serialize_message(&inv).unwrap();
    timeout(
        Duration::from_secs(5),
        nm.process_announced_block_inventory(&inv_bytes, peer_addr),
    )
    .await
    .expect("inv")
    .unwrap();
    let getdata = timeout(Duration::from_secs(5), reader)
        .await
        .expect("reader")
        .unwrap();
    assert_eq!(getdata.inventory[0].hash, child_hash);
    assert_eq!(
        storage.blocks().get_hash_by_height(1).unwrap().unwrap(),
        tip_hash
    );
    assert!(
        storage.blocks().get_hash_by_height(2).unwrap().is_none(),
        "side child must not occupy the next height"
    );
    assert!(storage.blocks().get_header(&child_hash).unwrap().is_some());
    assert_eq!(
        storage.blocks().get_height_by_hash(&child_hash).unwrap(),
        Some(2)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn announcer_is_probed_and_still_receives_getdata() {
    let temp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp.path()).unwrap());
    let tip_hash = [9u8; 32];
    let tip_header = BlockHeader {
        version: 4,
        prev_block_hash: [0u8; 32],
        merkle_root: [1u8; 32],
        timestamp: 1_700_000_000,
        bits: 0x207fffff,
        nonce: 0,
    };
    connect_tip(&storage, &tip_hash, &tip_header, 1);
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let nm = Arc::new(
        NetworkManager::new("127.0.0.1:0".parse().unwrap()).with_dependencies(
            protocol,
            storage,
            Arc::new(MempoolManager::new()),
        ),
    );
    let listener_b = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_b = listener_b.local_addr().unwrap();
    let listener_a = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_a = listener_a.local_addr().unwrap();
    let announced = [0xabu8; 32];
    tokio::spawn(async move {
        let (mut sock, _) = listener_b.accept().await.unwrap();
        let mut buf = [0u8; 64];
        let _ = timeout(Duration::from_secs(8), sock.read(&mut buf)).await;
    });
    let nm_read = Arc::clone(&nm);
    let reader_a = tokio::spawn(async move {
        let (mut sock, _) = listener_a.accept().await.unwrap();
        let mut saw_headers = false;
        let mut getdata = None;
        for _ in 0..6 {
            let frame = timeout(Duration::from_secs(5), read_frame(&mut sock))
                .await
                .expect("frame from A");
            match ProtocolParser::parse_message(&frame).unwrap() {
                ProtocolMessage::GetHeaders(_) => {
                    saw_headers = true;
                    let unrelated = vec![
                        BlockHeader {
                            version: 4,
                            prev_block_hash: [1u8; 32],
                            merkle_root: [2u8; 32],
                            timestamp: 1_700_000_003,
                            bits: 0x207fffff,
                            nonce: 1,
                        },
                        BlockHeader {
                            version: 4,
                            prev_block_hash: [3u8; 32],
                            merkle_root: [4u8; 32],
                            timestamp: 1_700_000_004,
                            bits: 0x207fffff,
                            nonce: 2,
                        },
                    ];
                    assert!(nm_read.complete_headers_request(peer_a, unrelated));
                }
                ProtocolMessage::GetData(msg) => {
                    getdata = Some(msg);
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_headers, "announcer received getheaders");
        getdata.expect("announcer received getdata")
    });
    nm.connect_to_peer(peer_b).await.unwrap();
    nm.connect_to_peer(peer_a).await.unwrap();
    let inv = ProtocolMessage::Inv(crate::network::protocol::InvMessage {
        inventory: vec![InventoryVector {
            inv_type: MSG_BLOCK,
            hash: announced,
        }],
    });
    let inv_bytes = ProtocolParser::serialize_message(&inv).unwrap();
    timeout(
        Duration::from_secs(5),
        nm.process_announced_block_inventory(&inv_bytes, peer_a),
    )
    .await
    .expect("inv handled")
    .unwrap();
    let getdata = timeout(Duration::from_secs(5), reader_a)
        .await
        .expect("reader")
        .unwrap();
    assert!(getdata.inventory.iter().any(|item| item.hash == announced));
}
