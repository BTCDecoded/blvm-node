#![no_main]
use libfuzzer_sys::fuzz_target;
use reference_node::network::protocol::ProtocolParser;

fuzz_target!(|data: &[u8]| {
    // Fuzz protocol message parsing with malformed/corrupted data
    // Security-critical: malformed messages could cause crashes or exploits
    
    if data.is_empty() {
        return;
    }
    
    // Test parse_message() - should never panic, should handle errors gracefully
    let result = ProtocolParser::parse_message(data);
    
    // If parsing succeeds, test round-trip serialization
    if let Ok(message) = result {
        // Verify we can serialize it back
        let serialized = ProtocolParser::serialize_message(&message);
        
        // If serialization succeeds, test parsing the serialized message
        if let Ok(serialized_bytes) = serialized {
            let _round_trip = ProtocolParser::parse_message(&serialized_bytes);
            // Should succeed or return a structured error, never panic
        }
    }
    
    // Test with corrupted variations
    // 1. Truncated messages (various lengths)
    if data.len() > 24 {
        let _truncated = ProtocolParser::parse_message(&data[..24]);
    }
    if data.len() > 12 {
        let _truncated = ProtocolParser::parse_message(&data[..12]);
    }
    
    // 2. Corrupted magic number
    if data.len() >= 24 {
        let mut corrupted = data.to_vec();
        corrupted[0] = !corrupted[0];
        let _corrupted = ProtocolParser::parse_message(&corrupted);
    }
    
    // 3. Corrupted checksum
    if data.len() >= 24 {
        let mut corrupted = data.to_vec();
        if corrupted.len() > 20 {
            corrupted[20] = !corrupted[20];
            let _corrupted = ProtocolParser::parse_message(&corrupted);
        }
    }
    
    // 4. Invalid payload length (too large)
    if data.len() >= 24 {
        let mut corrupted = data.to_vec();
        // Set payload length to maximum + 1
        let max_payload = (32 * 1024 * 1024 - 24 + 1) as u32;
        if corrupted.len() >= 20 {
            corrupted[16..20].copy_from_slice(&max_payload.to_le_bytes());
            let _corrupted = ProtocolParser::parse_message(&corrupted);
        }
    }
    
    // 5. Invalid command string
    if data.len() >= 24 {
        let mut corrupted = data.to_vec();
        // Fill command with invalid characters
        for i in 4..12 {
            if i < corrupted.len() {
                corrupted[i] = 0xff;
            }
        }
        let _corrupted = ProtocolParser::parse_message(&corrupted);
    }
});

