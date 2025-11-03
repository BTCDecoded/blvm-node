#![no_main]
use libfuzzer_sys::fuzz_target;
use reference_node::network::compact_blocks::{
    create_compact_block, calculate_short_tx_id, calculate_tx_hash,
    should_prefer_compact_blocks, recommended_compact_block_version,
    is_quic_transport,
};
use reference_node::network::transport::TransportType;
use consensus_proof::{Block, BlockHeader, Transaction, TransactionOutput, Hash};
use std::collections::HashSet;

fuzz_target!(|data: &[u8]| {
    // Fuzz compact block reconstruction and transport-aware logic
    
    if data.len() < 88 {
        return; // Need at least block header
    }
    
    // Create a minimal block from fuzzed data
    let header = BlockHeader {
        version: i64::from_le_bytes([
            data.get(0).copied().unwrap_or(1) as u8,
            data.get(1).copied().unwrap_or(0),
            data.get(2).copied().unwrap_or(0),
            data.get(3).copied().unwrap_or(0),
            data.get(4).copied().unwrap_or(0),
            data.get(5).copied().unwrap_or(0),
            data.get(6).copied().unwrap_or(0),
            data.get(7).copied().unwrap_or(0),
        ]),
        prev_block_hash: data.get(8..40)
            .unwrap_or(&[0; 32])
            .try_into()
            .unwrap_or([0; 32]),
        merkle_root: data.get(40..72)
            .unwrap_or(&[0; 32])
            .try_into()
            .unwrap_or([0; 32]),
        timestamp: u64::from_le_bytes([
            data.get(72).copied().unwrap_or(0),
            data.get(73).copied().unwrap_or(0),
            data.get(74).copied().unwrap_or(0),
            data.get(75).copied().unwrap_or(0),
            data.get(76).copied().unwrap_or(0),
            data.get(77).copied().unwrap_or(0),
            data.get(78).copied().unwrap_or(0),
            data.get(79).copied().unwrap_or(0),
        ]),
        bits: u32::from_le_bytes([
            data.get(80).copied().unwrap_or(0),
            data.get(81).copied().unwrap_or(0),
            data.get(82).copied().unwrap_or(0),
            data.get(83).copied().unwrap_or(0),
        ]) as u64,
        nonce: u32::from_le_bytes([
            data.get(84).copied().unwrap_or(0),
            data.get(85).copied().unwrap_or(0),
            data.get(86).copied().unwrap_or(0),
            data.get(87).copied().unwrap_or(0),
        ]) as u64,
    };
    
    // Create block with coinbase transaction
    let block = Block {
        header: header.clone(),
        transactions: vec![Transaction {
            version: 1,
            inputs: vec![],
            outputs: vec![TransactionOutput {
                value: 5000000000,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }],
    };
    
    // Test compact block creation - should never panic
    let nonce = header.nonce;
    let prefilled_indices = HashSet::new(); // No prefilled for fuzzing
    let _compact_block = create_compact_block(&block, nonce, &prefilled_indices);
    
    // Test transaction hash calculation
    if !block.transactions.is_empty() {
        let _tx_hash = calculate_tx_hash(&block.transactions[0]);
    }
    
    // Test short ID calculation
    if !block.transactions.is_empty() && data.len() >= 96 {
        let nonce = u64::from_le_bytes([
            data.get(88).copied().unwrap_or(0),
            data.get(89).copied().unwrap_or(0),
            data.get(90).copied().unwrap_or(0),
            data.get(91).copied().unwrap_or(0),
            data.get(92).copied().unwrap_or(0),
            data.get(93).copied().unwrap_or(0),
            data.get(94).copied().unwrap_or(0),
            data.get(95).copied().unwrap_or(0),
        ]);
        let tx_hash = calculate_tx_hash(&block.transactions[0]);
        let _short_id = calculate_short_tx_id(&tx_hash, nonce);
    }
    
    // Test transport-aware functions with different transport types
    let transport_types = [
        TransportType::Tcp,
        #[cfg(feature = "quinn")]
        TransportType::Quinn,
        #[cfg(feature = "iroh")]
        TransportType::Iroh,
    ];
    
    for &transport in &transport_types {
        let _should_prefer = should_prefer_compact_blocks(transport);
        let _recommended_version = recommended_compact_block_version(transport);
        let _is_quic = is_quic_transport(transport);
    }
});

