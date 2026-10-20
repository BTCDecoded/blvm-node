//! Public peers that send an oversized or non-connecting message are disconnected and banned.
//! Local, manual, and NoBan peers still skip the ban.

use std::net::SocketAddr;
use std::sync::Arc;

use blvm_protocol::network::{AddrV2Message, AddressType, NetworkAddressV2, PeerState};
use blvm_protocol::pow::check_proof_of_work;
use blvm_protocol::{
    BitcoinProtocolEngine, BlockHeader, NetworkMessageLimits, OutPoint, ProtocolVersion,
    Transaction, TransactionInput, TransactionOutput,
};

use crate::network::NetworkManager;
use crate::network::compact_blocks::CompactBlock;
use crate::network::inventory::MSG_TX;
use crate::network::peer::Peer;
use crate::network::protocol::{
    AddrMessage, BlockTxnMessage, CompactBlockMessage, GetBlockTxnMessage, GetHeadersMessage,
    HeadersMessage, InvMessage, InventoryVector, NetworkAddress, ProtocolMessage, VersionMessage,
};
use crate::network::transport::TransportAddr;

/// One past the compact-relay ceiling in `wire_dispatch` (50_000).
const OVER_COMPACT: usize = 50_001;
/// Bitcoin Core `MAX_LOCATOR_SZ`.
const MAX_LOCATOR_SZ: usize = 101;
use crate::node::mempool::MempoolManager;
use crate::storage::Storage;

const PUBLIC_PEER: &str = "203.0.113.10:8333";

struct Node {
    manager: NetworkManager,
    _dir: tempfile::TempDir,
}

async fn node_with_storage() -> Node {
    let dir = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let listen: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let manager =
        NetworkManager::new(listen).with_dependencies(protocol, Arc::clone(&storage), mempool);
    Node { manager, _dir: dir }
}

fn bare_manager() -> NetworkManager {
    let listen: SocketAddr = "127.0.0.1:0".parse().unwrap();
    NetworkManager::new(listen)
}

async fn add_peer(manager: &NetworkManager, addr: SocketAddr, manual: bool, noban: bool) {
    let (mut peer, _incoming) = Peer::pair_for_testing(addr);
    if manual {
        peer.set_is_manual(true);
    }
    if noban {
        peer.set_permissions(1);
    }
    manager
        .peer_manager_mutex()
        .lock()
        .await
        .add_peer(TransportAddr::Tcp(addr), peer)
        .unwrap();
}

async fn mark_versioned(manager: &NetworkManager, addr: SocketAddr) {
    let mut state = PeerState::new();
    state.version = 70015;
    manager.peer_states().write().await.insert(addr, state);
}

fn legacy_addr() -> NetworkAddress {
    NetworkAddress {
        services: 1,
        ip: [0u8; 16],
        port: 8333,
        time: 0,
    }
}

fn addr_v2() -> NetworkAddressV2 {
    NetworkAddressV2::new(0, 1, AddressType::IPv4, vec![203, 0, 113, 20], 8333).unwrap()
}

fn tiny_tx() -> Transaction {
    Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [1u8; 32],
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: 0xffff_ffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 1,
            script_pubkey: Vec::new(),
        }]
        .into(),
        lock_time: 0,
    }
}

fn version_message(user_agent: String) -> VersionMessage {
    VersionMessage {
        version: 70015,
        services: 0,
        timestamp: 1_500_000_000,
        addr_recv: legacy_addr(),
        addr_from: legacy_addr(),
        nonce: 7,
        user_agent,
        start_height: 0,
        relay: true,
    }
}

fn easy_header(prev: [u8; 32], merkle: u8, timestamp: u64, nonce: u64) -> BlockHeader {
    BlockHeader {
        version: 1,
        prev_block_hash: prev,
        merkle_root: [merkle; 32],
        timestamp,
        bits: 0x207fffff,
        nonce,
    }
}

async fn stored_user_agent(manager: &NetworkManager, addr: SocketAddr) -> Option<String> {
    let pm = manager.peer_manager_mutex().lock().await;
    pm.get_peer(&TransportAddr::Tcp(addr))
        .and_then(|peer| peer.user_agent().cloned())
}

fn assert_banned(manager: &NetworkManager, addr: SocketAddr, result: anyhow::Result<()>) {
    let err = result.expect_err("public peer must be disconnected");
    assert!(!err.to_string().is_empty());
    assert!(
        manager.is_banned(addr),
        "public peer {addr} was disconnected ({err}) without a ban"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_addr_bans_a_public_peer() {
    let manager = bare_manager();
    let peer: SocketAddr = PUBLIC_PEER.parse().unwrap();
    add_peer(&manager, peer, false, false).await;
    mark_versioned(&manager, peer).await;
    let max = manager.protocol_limits().max_addr_to_send;
    let result = manager
        .dispatch_protocol_message(
            peer,
            &ProtocolMessage::Addr(AddrMessage {
                addresses: vec![legacy_addr(); max + 1],
            }),
            Vec::new(),
        )
        .await;
    assert_banned(&manager, peer, result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_addrv2_bans_a_public_peer() {
    let manager = bare_manager();
    let peer: SocketAddr = PUBLIC_PEER.parse().unwrap();
    add_peer(&manager, peer, false, false).await;
    mark_versioned(&manager, peer).await;
    let max = manager.protocol_limits().max_addr_to_send;
    let result = manager
        .dispatch_protocol_message(
            peer,
            &ProtocolMessage::AddrV2(AddrV2Message {
                addresses: vec![addr_v2(); max + 1],
            }),
            Vec::new(),
        )
        .await;
    assert_banned(&manager, peer, result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_inv_bans_a_public_peer() {
    let manager = bare_manager();
    let peer: SocketAddr = PUBLIC_PEER.parse().unwrap();
    add_peer(&manager, peer, false, false).await;
    mark_versioned(&manager, peer).await;
    let max = manager.protocol_limits().max_inv_sz;
    let result = manager
        .dispatch_protocol_message(
            peer,
            &ProtocolMessage::Inv(InvMessage {
                inventory: vec![
                    InventoryVector {
                        inv_type: MSG_TX,
                        hash: [9u8; 32],
                    };
                    max + 1
                ],
            }),
            Vec::new(),
        )
        .await;
    assert_banned(&manager, peer, result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_compact_block_bans_a_public_peer() {
    let manager = bare_manager();
    let peer: SocketAddr = PUBLIC_PEER.parse().unwrap();
    add_peer(&manager, peer, false, false).await;
    mark_versioned(&manager, peer).await;
    let result = manager
        .dispatch_protocol_message(
            peer,
            &ProtocolMessage::CmpctBlock(CompactBlockMessage {
                compact_block: CompactBlock {
                    header: easy_header([0u8; 32], 1, 1_500_000_000, 0),
                    nonce: 0,
                    short_ids: vec![[0u8; 6]; OVER_COMPACT],
                    prefilled_txs: Vec::new(),
                },
            }),
            Vec::new(),
        )
        .await;
    assert_banned(&manager, peer, result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_getblocktxn_bans_a_public_peer() {
    let manager = bare_manager();
    let peer: SocketAddr = PUBLIC_PEER.parse().unwrap();
    add_peer(&manager, peer, false, false).await;
    mark_versioned(&manager, peer).await;
    let result = manager
        .dispatch_protocol_message(
            peer,
            &ProtocolMessage::GetBlockTxn(GetBlockTxnMessage {
                block_hash: [4u8; 32],
                indices: vec![0u16; OVER_COMPACT],
            }),
            Vec::new(),
        )
        .await;
    assert_banned(&manager, peer, result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_blocktxn_bans_a_public_peer() {
    let manager = bare_manager();
    let peer: SocketAddr = PUBLIC_PEER.parse().unwrap();
    add_peer(&manager, peer, false, false).await;
    mark_versioned(&manager, peer).await;
    let result = manager
        .dispatch_protocol_message(
            peer,
            &ProtocolMessage::BlockTxn(BlockTxnMessage {
                block_hash: [5u8; 32],
                transactions: vec![tiny_tx(); OVER_COMPACT],
                witnesses: None,
            }),
            Vec::new(),
        )
        .await;
    assert_banned(&manager, peer, result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn getheaders_locator_over_101_bans_a_public_peer() {
    let node = node_with_storage().await;
    let peer: SocketAddr = PUBLIC_PEER.parse().unwrap();
    add_peer(&node.manager, peer, false, false).await;
    mark_versioned(&node.manager, peer).await;
    let genesis = easy_header([0u8; 32], 2, 1_500_000_000, 0);
    node.manager
        .storage()
        .as_ref()
        .unwrap()
        .chain()
        .initialize(&genesis)
        .unwrap();
    let tip = node
        .manager
        .storage()
        .as_ref()
        .unwrap()
        .chain()
        .get_tip_hash()
        .unwrap()
        .unwrap();
    let blocks = node.manager.storage().as_ref().unwrap().blocks();
    blocks.store_header(&tip, &genesis).unwrap();
    blocks.store_height(0, &tip).unwrap();
    let result = node
        .manager
        .dispatch_protocol_message(
            peer,
            &ProtocolMessage::GetHeaders(GetHeadersMessage {
                version: 70015,
                block_locator_hashes: vec![[0x11; 32]; MAX_LOCATOR_SZ + 1],
                hash_stop: [0u8; 32],
            }),
            Vec::new(),
        )
        .await;
    assert_banned(&node.manager, peer, result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn getheaders_locator_of_101_stays_connected() {
    let node = node_with_storage().await;
    let peer: SocketAddr = PUBLIC_PEER.parse().unwrap();
    add_peer(&node.manager, peer, false, false).await;
    mark_versioned(&node.manager, peer).await;
    node.manager
        .dispatch_protocol_message(
            peer,
            &ProtocolMessage::GetHeaders(GetHeadersMessage {
                version: 70015,
                block_locator_hashes: vec![[0x22; 32]; MAX_LOCATOR_SZ],
                hash_stop: [0u8; 32],
            }),
            Vec::new(),
        )
        .await
        .unwrap();
    assert!(!node.manager.is_banned(peer));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_agent_over_256_bans_a_public_peer() {
    let manager = bare_manager();
    let peer: SocketAddr = PUBLIC_PEER.parse().unwrap();
    add_peer(&manager, peer, false, false).await;
    let max = NetworkMessageLimits::default().max_user_agent_length;
    let result = manager
        .dispatch_protocol_message(
            peer,
            &ProtocolMessage::Version(version_message("a".repeat(max + 1))),
            Vec::new(),
        )
        .await;
    assert_banned(&manager, peer, result);
    assert_eq!(
        stored_user_agent(&manager, peer).await.as_deref(),
        Some("/test-peer/")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_agent_of_256_is_stored() {
    let manager = bare_manager();
    let peer: SocketAddr = PUBLIC_PEER.parse().unwrap();
    add_peer(&manager, peer, false, false).await;
    let max = NetworkMessageLimits::default().max_user_agent_length;
    let agent = "b".repeat(max);
    manager
        .dispatch_protocol_message(
            peer,
            &ProtocolMessage::Version(version_message(agent.clone())),
            Vec::new(),
        )
        .await
        .unwrap();
    assert!(!manager.is_banned(peer));
    assert_eq!(
        stored_user_agent(&manager, peer).await.as_deref(),
        Some(agent.as_str())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nonconnecting_headers_batch_bans_after_the_valid_prefix() {
    let node = node_with_storage().await;
    let peer: SocketAddr = PUBLIC_PEER.parse().unwrap();
    add_peer(&node.manager, peer, false, false).await;
    mark_versioned(&node.manager, peer).await;
    let storage = node.manager.storage().as_ref().unwrap();
    let genesis = easy_header([0u8; 32], 2, 1_500_000_000, 0);
    storage.chain().initialize(&genesis).unwrap();
    let tip = storage.chain().get_tip_hash().unwrap().unwrap();
    storage.blocks().store_header(&tip, &genesis).unwrap();
    storage.blocks().store_height(0, &tip).unwrap();
    let child = easy_header(tip, 3, 1_500_000_001, 1);
    assert!(check_proof_of_work(&child).unwrap());
    let broken = easy_header([0xab; 32], 4, 1_500_000_002, 2);
    let result = node
        .manager
        .dispatch_protocol_message(
            peer,
            &ProtocolMessage::Headers(HeadersMessage {
                headers: vec![child, broken],
            }),
            Vec::new(),
        )
        .await;
    assert_eq!(storage.blocks().highest_stored_height().unwrap(), Some(1));
    assert_banned(&node.manager, peer, result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_oversized_addr_disconnects_without_a_ban() {
    let manager = bare_manager();
    let peer: SocketAddr = "127.0.0.1:18444".parse().unwrap();
    add_peer(&manager, peer, false, false).await;
    mark_versioned(&manager, peer).await;
    let max = manager.protocol_limits().max_addr_to_send;
    let err = manager
        .dispatch_protocol_message(
            peer,
            &ProtocolMessage::Addr(AddrMessage {
                addresses: vec![legacy_addr(); max + 1],
            }),
            Vec::new(),
        )
        .await
        .expect_err("local peer is still disconnected");
    assert!(err.to_string().contains("addr message size exceeded"));
    assert!(!manager.is_banned(peer));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_and_noban_peers_skip_the_inv_ban() {
    let manager = bare_manager();
    let manual: SocketAddr = "203.0.113.11:8333".parse().unwrap();
    let noban: SocketAddr = "203.0.113.12:8333".parse().unwrap();
    add_peer(&manager, manual, true, false).await;
    add_peer(&manager, noban, false, true).await;
    mark_versioned(&manager, manual).await;
    mark_versioned(&manager, noban).await;
    let max = manager.protocol_limits().max_inv_sz;
    let inv = ProtocolMessage::Inv(InvMessage {
        inventory: vec![
            InventoryVector {
                inv_type: MSG_TX,
                hash: [9u8; 32],
            };
            max + 1
        ],
    });
    manager
        .dispatch_protocol_message(manual, &inv, Vec::new())
        .await
        .unwrap();
    manager
        .dispatch_protocol_message(noban, &inv, Vec::new())
        .await
        .unwrap();
    assert!(!manager.is_banned(manual));
    assert!(!manager.is_banned(noban));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_peer_message_rate_limit_stays_off() {
    let manager = bare_manager();
    let peer: SocketAddr = PUBLIC_PEER.parse().unwrap();
    assert!(
        crate::config::NodeConfig::default()
            .peer_rate_limiting
            .is_none()
    );
    assert!(manager.allow_peer_message(peer).await);
}
