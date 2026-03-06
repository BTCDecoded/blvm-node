//! Tests for block request/response connection flow (IBD first-block path).
//!
//! Verifies that:
//! - register_block_request stores (peer, block_hash) correctly
//! - A valid Bitcoin wire-format block message triggers complete_block_request
//! - The oneshot delivers the block to the awaiting download_chunk caller

use anyhow::Result;
use blvm_node::network::{protocol::ProtocolParser, NetworkManager};
use blvm_protocol::genesis;
use blvm_protocol::serialization::serialize_block_with_witnesses;
use std::net::SocketAddr;

/// Build a Bitcoin wire-format "block" message (magic + command + length + checksum + payload).
/// Uses consensus serialization to match what real Bitcoin peers send.
fn build_block_wire_message(block: &blvm_protocol::Block) -> Vec<u8> {
    let witnesses: Vec<Vec<blvm_protocol::segwit::Witness>> =
        (0..block.transactions.len()).map(|_| vec![]).collect();
    let payload = serialize_block_with_witnesses(block, &witnesses, false);

    let mut msg = Vec::new();
    // Magic (mainnet)
    msg.extend_from_slice(&[0xf9u8, 0xbe, 0xb4, 0xd9]);
    // Command "block" (12 bytes, null-padded)
    let mut cmd = [0u8; 12];
    cmd[..5].copy_from_slice(b"block");
    msg.extend_from_slice(&cmd);
    // Payload length
    msg.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    // Checksum (first 4 bytes of double SHA256 of payload)
    let checksum = ProtocolParser::calculate_checksum(&payload);
    msg.extend_from_slice(&checksum);
    msg.extend_from_slice(&payload);
    msg
}

/// Compute block hash the same way handle_incoming_wire_tcp does (double SHA256 of 80-byte header).
fn block_hash_from_header(header: &blvm_protocol::BlockHeader) -> blvm_protocol::Hash {
    use blvm_node::storage::hashing::double_sha256;
    let mut header_bytes = Vec::with_capacity(80);
    header_bytes.extend_from_slice(&(header.version as i32).to_le_bytes());
    header_bytes.extend_from_slice(&header.prev_block_hash);
    header_bytes.extend_from_slice(&header.merkle_root);
    header_bytes.extend_from_slice(&(header.timestamp as u32).to_le_bytes());
    header_bytes.extend_from_slice(&(header.bits as u32).to_le_bytes());
    header_bytes.extend_from_slice(&(header.nonce as u32).to_le_bytes());
    let h = double_sha256(&header_bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&h);
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn test_block_request_completion_first_connection() -> Result<()> {
    let genesis = genesis::mainnet_genesis();
    let block_hash = block_hash_from_header(&genesis.header);
    let block_wire = build_block_wire_message(&genesis);

    // NetworkManager with minimal config
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let network =
        NetworkManager::with_config(addr, 5, blvm_node::network::transport::TransportPreference::TCP_ONLY, None);

    // Simulate IBD: register a block request before "receiving" the block
    let peer_addr: SocketAddr = "127.0.0.1:18444".parse().unwrap();
    let block_rx = network.register_block_request(peer_addr, block_hash);

    // Simulate peer sending block (same path as RawMessageReceived -> handle_incoming_wire_tcp)
    network.handle_incoming_wire_tcp(peer_addr, block_wire).await?;

    // Await the oneshot; should complete with the genesis block
    let (received_block, witnesses) = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        block_rx,
    )
    .await
    .map_err(|_| anyhow::anyhow!("Block response timeout"))?
    .map_err(|_| anyhow::anyhow!("Block channel closed"))?;

    assert_eq!(received_block.header.merkle_root, genesis.header.merkle_root);
    assert_eq!(received_block.transactions.len(), genesis.transactions.len());
    assert_eq!(witnesses.len(), genesis.transactions.len());

    Ok(())
}
