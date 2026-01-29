//! Integration tests for UTXO proof protocol messages
//!
//! Tests the network protocol handlers for GetUTXOProof and UTXOProof messages

#[cfg(feature = "utxo-commitments")]
mod tests {
    use blvm_node::network::protocol::{GetUTXOProofMessage, UTXOProofMessage, ProtocolMessage, ProtocolParser};
    use blvm_node::network::protocol_extensions::{
        handle_get_utxo_proof, serialize_get_utxo_proof, deserialize_utxo_proof,
    };
    use blvm_consensus::types::{OutPoint, UTXO};
    use blvm_consensus::utxo_commitments::merkle_tree::UtxoMerkleTree;
    use blvm_consensus::utxo_commitments::data_structures::UtxoCommitment;
    use std::sync::Arc;
    
    // Mock storage for testing
    struct MockStorage {
        utxo_set: std::collections::HashMap<OutPoint, UTXO>,
    }
    
    impl MockStorage {
        fn new() -> Self {
            Self {
                utxo_set: std::collections::HashMap::new(),
            }
        }
        
        fn add_utxo(&mut self, outpoint: OutPoint, utxo: UTXO) {
            self.utxo_set.insert(outpoint, utxo);
        }
    }

    #[tokio::test]
    async fn test_protocol_message_serialization() {
        // Create GetUTXOProof message
        let request_id = 12345;
        let tx_hash = [1; 32];
        let output_index = 0;
        let block_height = 100;
        let block_hash = [2; 32];
        
        let get_proof_msg = GetUTXOProofMessage {
            request_id,
            tx_hash,
            output_index,
            block_height,
            block_hash,
        };
        
        // Serialize
        let serialized = serialize_get_utxo_proof(&get_proof_msg).unwrap();
        
        // Deserialize via protocol parser
        let parsed = ProtocolParser::parse_message(&serialized).unwrap();
        
        match parsed {
            ProtocolMessage::GetUTXOProof(msg) => {
                assert_eq!(msg.request_id, request_id);
                assert_eq!(msg.tx_hash, tx_hash);
                assert_eq!(msg.output_index, output_index);
                assert_eq!(msg.block_height, block_height);
                assert_eq!(msg.block_hash, block_hash);
            }
            _ => panic!("Expected GetUTXOProof message"),
        }
    }

    #[tokio::test]
    async fn test_protocol_proof_response_serialization() {
        // Create UTXOProof message
        let request_id = 12345;
        let tx_hash = [1; 32];
        let output_index = 0;
        
        // Create a mock proof (serialized)
        let mock_proof_bytes = vec![0u8; 100]; // Placeholder
        
        let proof_msg = UTXOProofMessage {
            request_id,
            tx_hash,
            output_index,
            value: 1000,
            script_pubkey: vec![0x51],
            height: 0,
            is_coinbase: false,
            proof: mock_proof_bytes.clone(),
        };
        
        // Serialize via protocol
        let serialized = ProtocolParser::serialize_message(&ProtocolMessage::UTXOProof(proof_msg.clone())).unwrap();
        
        // Deserialize
        let parsed = ProtocolParser::parse_message(&serialized).unwrap();
        
        match parsed {
            ProtocolMessage::UTXOProof(msg) => {
                assert_eq!(msg.request_id, request_id);
                assert_eq!(msg.tx_hash, tx_hash);
                assert_eq!(msg.output_index, output_index);
                assert_eq!(msg.value, 1000);
                assert_eq!(msg.proof, mock_proof_bytes);
            }
            _ => panic!("Expected UTXOProof message"),
        }
    }

    #[tokio::test]
    async fn test_proof_handler_integration() {
        // This test would require actual storage setup
        // For now, we test the serialization/deserialization
        
        // Create message
        let get_proof_msg = GetUTXOProofMessage {
            request_id: 1,
            tx_hash: [1; 32],
            output_index: 0,
            block_height: 0,
            block_hash: [2; 32],
        };
        
        // Test serialization round-trip
        let serialized = serialize_get_utxo_proof(&get_proof_msg).unwrap();
        let parsed = ProtocolParser::parse_message(&serialized).unwrap();
        
        assert!(matches!(parsed, ProtocolMessage::GetUTXOProof(_)));
    }

    #[test]
    fn test_proof_verification_with_protocol_data() {
        // Test that proof data from protocol can be used for verification
        let mut tree = UtxoMerkleTree::new().unwrap();
        
        let outpoint = OutPoint {
            hash: [1; 32],
            index: 0,
        };
        let utxo = UTXO {
            value: 1000,
            script_pubkey: vec![0x51],
            height: 0,
            is_coinbase: false,
        };
        
        tree.insert(outpoint.clone(), utxo.clone()).unwrap();
        let commitment = tree.generate_commitment([2; 32], 0);
        
        // Generate proof
        let proof = tree.generate_proof(&outpoint).unwrap();
        
        // Serialize proof (as protocol would)
        let proof_bytes = bincode::serialize(&proof).unwrap();
        
        // Deserialize proof (as client would)
        let proof_deserialized: sparse_merkle_tree::MerkleProof = bincode::deserialize(&proof_bytes).unwrap();
        
        // Create protocol message
        let proof_msg = UTXOProofMessage {
            request_id: 1,
            tx_hash: outpoint.hash,
            output_index: outpoint.index,
            value: utxo.value,
            script_pubkey: utxo.script_pubkey.clone(),
            height: utxo.height,
            is_coinbase: utxo.is_coinbase,
            proof: proof_bytes,
        };
        
        // Extract data from protocol message
        let utxo_from_msg = UTXO {
            value: proof_msg.value,
            script_pubkey: proof_msg.script_pubkey,
            height: proof_msg.height,
            is_coinbase: proof_msg.is_coinbase,
        };
        
        let outpoint_from_msg = OutPoint {
            hash: proof_msg.tx_hash,
            index: proof_msg.output_index,
        };
        
        // Verify using protocol data
        let is_valid = UtxoMerkleTree::verify_utxo_proof(
            &commitment,
            &outpoint_from_msg,
            &utxo_from_msg,
            proof_deserialized,
        ).unwrap();
        
        assert!(is_valid, "Proof from protocol message should be valid");
    }
}


