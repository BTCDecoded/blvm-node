#![no_main]
use libfuzzer_sys::fuzz_target;
use protocol_engine::serialization::serialize_transaction;
use protocol_engine::types::{Transaction, TransactionInput, TransactionOutput, OutPoint};
use sha2::{Digest, Sha256};

fuzz_target!(|data: &[u8]| {
    // Fuzz transaction serialization
    
    if data.len() < 4 {
        return; // Need at least version
    }
    
    // Parse version (first 4 bytes, little-endian)
    let version = if data.len() >= 4 {
        i32::from_le_bytes([data[0], data[1], data[2], data[3]])
    } else {
        return;
    };
    
    // Skip invalid versions
    if version < 1 || version > 2 {
        return;
    }
    
    // Try to parse a transaction structure from fuzzed data
    // This is a simplified parser for fuzzing - real parser would be more robust
    
    let mut offset = 4;
    
    // Parse input count (varint)
    if offset >= data.len() {
        return;
    }
    let input_count = if data[offset] < 0xfd {
        offset += 1;
        data[offset - 1] as usize
    } else if data[offset] == 0xfd && offset + 3 <= data.len() {
        offset += 3;
        u16::from_le_bytes([data[offset - 2], data[offset - 1]]) as usize
    } else {
        return;
    };
    
    // Limit input count for fuzzing
    if input_count > 100 {
        return;
    }
    
    // Parse inputs
    let mut inputs = Vec::new();
    for _ in 0..input_count {
        if offset + 36 > data.len() {
            break;
        }
        
        // Parse prevout hash (32 bytes)
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&data[offset..offset + 32]);
        offset += 32;
        
        // Parse prevout index (4 bytes)
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
        
        // Parse script length (varint)
        if offset >= data.len() {
            break;
        }
        let script_len = if data[offset] < 0xfd {
            offset += 1;
            data[offset - 1] as usize
        } else if data[offset] == 0xfd && offset + 3 <= data.len() {
            offset += 3;
            u16::from_le_bytes([data[offset - 2], data[offset - 1]]) as usize
        } else {
            break;
        };
        
        // Limit script length
        if script_len > 10000 {
            break;
        }
        
        if offset + script_len + 4 > data.len() {
            break;
        }
        
        let script_sig = data[offset..offset + script_len].to_vec();
        offset += script_len;
        
        // Parse sequence (4 bytes)
        let sequence = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        offset += 4;
        
        inputs.push(TransactionInput {
            prevout: OutPoint { hash, index },
            script_sig,
            sequence: sequence as u64,
        });
    }
    
    // Parse output count (varint)
    if offset >= data.len() {
        return;
    }
    let output_count = if data[offset] < 0xfd {
        offset += 1;
        data[offset - 1] as usize
    } else if data[offset] == 0xfd && offset + 3 <= data.len() {
        offset += 3;
        u16::from_le_bytes([data[offset - 2], data[offset - 1]]) as usize
    } else {
        return;
    };
    
    // Limit output count for fuzzing
    if output_count > 100 {
        return;
    }
    
    // Parse outputs
    let mut outputs = Vec::new();
    for _ in 0..output_count {
        if offset + 8 > data.len() {
            break;
        }
        
        // Parse value (8 bytes)
        let value = i64::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
            data[offset + 4],
            data[offset + 5],
            data[offset + 6],
            data[offset + 7],
        ]);
        offset += 8;
        
        // Parse script length (varint)
        if offset >= data.len() {
            break;
        }
        let script_len = if data[offset] < 0xfd {
            offset += 1;
            data[offset - 1] as usize
        } else if data[offset] == 0xfd && offset + 3 <= data.len() {
            offset += 3;
            u16::from_le_bytes([data[offset - 2], data[offset - 1]]) as usize
        } else {
            break;
        };
        
        // Limit script length
        if script_len > 10000 {
            break;
        }
        
        if offset + script_len > data.len() {
            break;
        }
        
        let script_pubkey = data[offset..offset + script_len].to_vec();
        offset += script_len;
        
        outputs.push(TransactionOutput {
            value,
            script_pubkey,
        });
    }
    
    // Parse lock time (4 bytes)
    let lock_time = if offset + 4 <= data.len() {
        u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as u64
    } else {
        0
    };
    
    // Create transaction
    let tx = Transaction {
        version: version as u64,
        inputs,
        outputs,
        lock_time,
    };
    
    // Serialize - should never panic
    let serialized = serialize_transaction(&tx);
    
    // Verify serialized transaction is not empty
    assert!(!serialized.is_empty());
    
    // Calculate hash - should never panic
    let hash1 = Sha256::digest(&serialized);
    let hash2 = Sha256::digest(hash1);
    
    // Verify hash is 32 bytes
    assert_eq!(hash2.len(), 32);
});

