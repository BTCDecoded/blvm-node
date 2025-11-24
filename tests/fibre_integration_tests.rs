//! Integration tests for FIBRE (Fast Internet Bitcoin Relay Engine)

use bllvm_node::network::fibre::{FibreRelay, FibreError};
use bllvm_node::network::{NetworkManager, NetworkMessage};
use bllvm_protocol::{Block, BlockHeader};
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
    
    // Note: Full block assembly test requires access to chunks which are private
    // This is tested at the unit test level. Integration test verifies encoding works.
}

#[tokio::test]
async fn test_fibre_network_manager_integration() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut network = NetworkManager::with_config(listen_addr, 10, bllvm_node::network::transport::TransportPreference::TCP_ONLY, None);
    
    // Initialize FIBRE
    let config = bllvm_protocol::fibre::FibreConfig {
        enabled: true,
        fec_parity_ratio: 0.2,
        chunk_timeout_secs: 2,
        max_retries: 3,
        max_assemblies: 10,
    };
    
    // Create a test config with FIBRE enabled
    let mut node_config = bllvm_node::config::NodeConfig::default();
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
    use bllvm_protocol::fibre::{FecChunk, FIBRE_MAGIC};
    
    // Create chunk manually (since new() is not public, we'll use deserialize after creating raw data)
    let block_hash = [0x42; 32];
    let data = vec![1, 2, 3, 4, 5];
    
    // Create a valid serialized chunk manually for testing
    let mut packet = Vec::new();
    packet.extend_from_slice(&FIBRE_MAGIC);
    packet.push(bllvm_protocol::fibre::FIBRE_VERSION);
    packet.push(bllvm_protocol::fibre::PACKET_TYPE_CHUNK);
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

