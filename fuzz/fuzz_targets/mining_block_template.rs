#![no_main]
use libfuzzer_sys::fuzz_target;
use reference_node::rpc::mining::MiningRpc;
use reference_node::storage::Storage;
use reference_node::node::mempool::MempoolManager;
use std::sync::Arc;
use tempfile::TempDir;
use protocol_engine::types::{BlockHeader, OutPoint, UTXO, Hash};
use serde_json::json;

fuzz_target!(|data: &[u8]| {
    // Fuzz block template creation
    
    if data.len() < 8 {
        return; // Need at least height
    }
    
    // Parse height (first 8 bytes, little-endian)
    let height = u64::from_le_bytes([
        data.get(0).copied().unwrap_or(0),
        data.get(1).copied().unwrap_or(0),
        data.get(2).copied().unwrap_or(0),
        data.get(3).copied().unwrap_or(0),
        data.get(4).copied().unwrap_or(0),
        data.get(5).copied().unwrap_or(0),
        data.get(6).copied().unwrap_or(0),
        data.get(7).copied().unwrap_or(0),
    ]);
    
    // Limit height for reasonable testing
    if height > 1000000 {
        return;
    }
    
    // Create temporary storage
    let temp_dir = match TempDir::new() {
        Ok(dir) => dir,
        Err(_) => return,
    };
    
    let storage = match Arc::new(Storage::new(temp_dir.path())) {
        Ok(s) => s,
        Err(_) => return,
    };
    
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);
    
    // Initialize chain state
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 2083236893,
    };
    
    if storage.chain().initialize(&genesis_header).is_err() {
        return;
    }
    
    // Create tip header from fuzzed data
    let mut prev_hash_bytes = [0u8; 32];
    if data.len() >= 40 {
        prev_hash_bytes.copy_from_slice(&data[8..40]);
    }
    
    let mut merkle_root_bytes = [0u8; 32];
    if data.len() >= 72 {
        merkle_root_bytes.copy_from_slice(&data[40..72]);
    }
    
    let timestamp = if data.len() >= 80 {
        u64::from_le_bytes([
            data.get(72).copied().unwrap_or(0),
            data.get(73).copied().unwrap_or(0),
            data.get(74).copied().unwrap_or(0),
            data.get(75).copied().unwrap_or(0),
            data.get(76).copied().unwrap_or(0),
            data.get(77).copied().unwrap_or(0),
            data.get(78).copied().unwrap_or(0),
            data.get(79).copied().unwrap_or(0),
        ])
    } else {
        1231006505 + height * 600
    };
    
    let bits = if data.len() >= 84 {
        u32::from_le_bytes([
            data.get(80).copied().unwrap_or(0xff),
            data.get(81).copied().unwrap_or(0x00),
            data.get(82).copied().unwrap_or(0x00),
            data.get(83).copied().unwrap_or(0x1d),
        ]) as u64
    } else {
        0x1d00ffff
    };
    
    let tip_header = BlockHeader {
        version: 1,
        prev_block_hash: prev_hash_bytes,
        merkle_root: merkle_root_bytes,
        timestamp,
        bits,
        nonce: 0,
    };
    
    // Update tip
    if storage.chain().update_tip(&prev_hash_bytes, &tip_header, height).is_err() {
        return;
    }
    
    // Add UTXOs from fuzzed data (if available)
    if data.len() >= 120 {
        let mut offset = 84;
        let mut utxo_count = 0;
        
        while offset + 36 <= data.len() && utxo_count < 10 {
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&data[offset..offset + 32]);
            offset += 32;
            
            if offset + 4 > data.len() {
                break;
            }
            
            let index = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as u64;
            offset += 4;
            
            let outpoint = OutPoint { hash, index };
            let utxo = UTXO {
                value: 100000000, // 1 BTC
                script_pubkey: vec![0x76, 0xa9, 0x14],
                height: 0,
            };
            
            // Ignore errors - just try to add
            let _ = storage.utxos().add_utxo(&outpoint, &utxo);
            utxo_count += 1;
        }
    }
    
    // Get block template - should never panic
    let params = json!([]);
    let rt = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(_) => return,
    };
    
    let result = rt.block_on(mining.get_block_template(&params));
    
    // Result may be Ok or Err, but should not panic
    if let Ok(template) = result {
        // Verify template has required fields
        assert!(template.get("version").is_some());
        assert!(template.get("height").is_some());
        assert!(template.get("previousblockhash").is_some());
        assert!(template.get("coinbasevalue").is_some());
    }
});

