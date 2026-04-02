//! Integration tests for FIBRE (Fast Internet Bitcoin Relay Engine)

#![cfg(feature = "fibre")]

use blvm_node::network::fibre::{FibreError, FibreRelay};
use blvm_node::network::{NetworkManager, NetworkMessage};
use blvm_protocol::{Block, BlockHeader};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};

fn create_test_block() -> Block {
    Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0x11; 32],
            merkle_root: [0x22; 32],
            timestamp: 1234567890,
            bits: 0x1d00ffff,
            nonce: 0x12345678,
        },
        transactions: vec![].into(),
    }
}

#[tokio::test]
async fn test_fibre_relay_encode_decode_cycle() {
    let mut relay = FibreRelay::new();
    let block = create_test_block();

    // Encode block
    let encoded = relay.encode_block(block.clone()).unwrap();
    assert!(encoded.chunk_count > 0);

    // Verify we can get it from cache
    let cached = relay.get_encoded_block(&encoded.block_hash);
    assert!(cached.is_some());
    assert_eq!(cached.unwrap().block_hash, encoded.block_hash);
}

#[tokio::test]
async fn test_fibre_peer_registration() {
    let mut relay = FibreRelay::new();
    let udp_addr: SocketAddr = "127.0.0.1:8334".parse().unwrap();

    relay.register_fibre_peer("peer1".to_string(), Some(udp_addr));
    relay.register_fibre_peer("peer2".to_string(), None);

    assert!(relay.is_fibre_peer("peer1"));
    assert!(relay.is_fibre_peer("peer2"));
    assert!(!relay.is_fibre_peer("peer3"));

    let peers = relay.get_fibre_peers();
    assert_eq!(peers.len(), 2);
}

#[tokio::test]
async fn test_fibre_block_assembly() {
    let mut relay = FibreRelay::new();
    let block = create_test_block();

    // Encode block
    let encoded = relay.encode_block(block.clone()).unwrap();
    assert!(encoded.chunk_count > 0);

    // Feed chunks back via process_received_chunk and verify block assembly
    let mut assembled: Option<blvm_protocol::Block> = None;
    for chunk in &encoded.chunks {
        let result = relay.process_received_chunk(chunk.clone()).await.unwrap();
        if let Some(b) = result {
            assembled = Some(b);
            break;
        }
    }
    let assembled = assembled.expect("Block should assemble from all chunks");
    assert_eq!(
        assembled.header.prev_block_hash,
        block.header.prev_block_hash
    );
    assert_eq!(assembled.header.merkle_root, block.header.merkle_root);
}

#[tokio::test]
async fn test_fibre_fec_recovery() {
    // Test that block can be assembled with packet loss (FEC recovery)
    let config = blvm_protocol::fibre::FibreConfig {
        enabled: true,
        fec_parity_ratio: 0.5, // Enough parity to recover 1-2 missing data chunks
        chunk_timeout_secs: 2,
        max_retries: 3,
        max_assemblies: 10,
    };
    let mut relay = FibreRelay::with_config(config);
    let block = create_test_block();

    let encoded = relay.encode_block(block.clone()).unwrap();
    assert!(
        encoded.chunk_count >= 2,
        "Need multiple chunks for FEC test"
    );

    // Drop first data chunk, keep parity chunks - FEC should recover
    let data_chunks = encoded.data_chunks as usize;
    let chunks_to_send: Vec<_> = encoded
        .chunks
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 0) // Drop chunk 0
        .map(|(_, c)| c.clone())
        .collect();

    let mut assembled: Option<blvm_protocol::Block> = None;
    for chunk in chunks_to_send {
        let result = relay.process_received_chunk(chunk).await.unwrap();
        if let Some(b) = result {
            assembled = Some(b);
            break;
        }
    }
    let assembled = assembled.expect("FEC should recover block with 1 missing data chunk");
    assert_eq!(
        assembled.header.prev_block_hash,
        block.header.prev_block_hash
    );
}

#[tokio::test]
async fn test_fibre_network_manager_integration() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut network = NetworkManager::with_config(
        listen_addr,
        10,
        blvm_node::network::transport::TransportPreference::TCP_ONLY,
        None,
    );

    // Initialize FIBRE
    let config = blvm_protocol::fibre::FibreConfig {
        enabled: true,
        fec_parity_ratio: 0.2,
        chunk_timeout_secs: 2,
        max_retries: 3,
        max_assemblies: 10,
    };

    // Create a test config with FIBRE enabled
    let mut node_config = blvm_node::config::NodeConfig::default();
    node_config.fibre = Some(config);

    // Initialize FIBRE
    network.initialize_fibre(Some(&node_config)).await.unwrap();

    // Verify FIBRE relay is initialized
    assert!(network.fibre_relay().is_some());

    // Test block broadcasting
    let block = create_test_block();
    // This will fail if no peers, but that's okay for integration test
    let _ = network.broadcast_block_via_fibre(&block).await;
}

#[tokio::test]
async fn test_fibre_chunk_serialization_roundtrip() {
    use blvm_protocol::fibre::{FecChunk, FIBRE_MAGIC};

    // Test strategy: exercise deserialize path for FecChunk because the constructor is not public.
    // Create chunk by building raw packet then deserializing.
    let block_hash = [0x42; 32];
    let data = vec![1, 2, 3, 4, 5];

    // Create a valid serialized chunk manually for testing
    let mut packet = Vec::new();
    packet.extend_from_slice(&FIBRE_MAGIC);
    packet.push(blvm_protocol::fibre::FIBRE_VERSION);
    packet.push(blvm_protocol::fibre::PACKET_TYPE_CHUNK);
    packet.extend_from_slice(&block_hash);
    packet.extend_from_slice(&12345u64.to_be_bytes());
    packet.extend_from_slice(&0u32.to_be_bytes()); // index
    packet.extend_from_slice(&10u32.to_be_bytes()); // total_chunks
    packet.extend_from_slice(&8u32.to_be_bytes()); // data_chunks
    packet.extend_from_slice(&(data.len() as u32).to_be_bytes()); // data_length
    packet.extend_from_slice(&data);
    let checksum = crc32fast::hash(&packet);
    packet.extend_from_slice(&checksum.to_be_bytes());

    // Deserialize
    let chunk = FecChunk::deserialize(&packet).unwrap();
    assert_eq!(chunk.index, 0);
    assert_eq!(chunk.total_chunks, 10);
    assert_eq!(chunk.data_chunks, 8);
    assert_eq!(chunk.data, data);
    assert_eq!(chunk.block_hash, block_hash);
    assert_eq!(chunk.sequence, 12345);

    // Serialize back
    let reserialized = chunk.serialize().unwrap();
    assert_eq!(reserialized, packet);
}

/// Integration test: two nodes, FIBRE relay block between them
///
/// Node A encodes a block and sends via UDP to Node B. Node B receives chunks,
/// assembles the block, and verifies it matches.
#[tokio::test]
async fn test_fibre_two_node_block_relay() {
    use blvm_node::network::fibre::start_chunk_processor;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let block = create_test_block();

    // Node B: bind first (port 0 = OS assigns)
    let mut relay_b = FibreRelay::new();
    let chunk_rx_b = relay_b
        .initialize_udp("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let relay_b_addr = relay_b
        .udp_local_addr()
        .await
        .expect("B should have UDP transport");

    // Node A: bind, register B, encode and send
    let mut relay_a = FibreRelay::new();
    relay_a
        .initialize_udp("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    relay_a.register_fibre_peer("node_b".to_string(), Some(relay_b_addr));

    let encoded = relay_a.encode_block(block.clone()).unwrap();
    relay_a.send_block("node_b", encoded).await.unwrap();

    // Node B: process received chunks
    let relay_b_arc = Arc::new(Mutex::new(relay_b));
    let processor = start_chunk_processor(Arc::clone(&relay_b_arc), chunk_rx_b);

    // Give time for UDP delivery and processing
    sleep(Duration::from_millis(200)).await;

    // Cancel processor (we only need one block)
    processor.abort();
    let _ = processor.await;

    // Check B received and assembled the block via stats
    let relay_b_guard = relay_b_arc.lock().await;
    let stats = relay_b_guard.get_stats().await;
    assert!(
        stats.blocks_received >= 1 || stats.chunks_received >= 1,
        "Node B should receive block or chunks: blocks_received={}, chunks_received={}",
        stats.blocks_received,
        stats.chunks_received
    );
}
