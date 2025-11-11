//! Basic integration tests for bllvm-node

use bllvm_node::*;
use bllvm_protocol::*;
use bllvm_protocol::ProtocolVersion;

#[test]
fn test_protocol_integration() {
    // Test that bllvm-protocol works in bllvm-node context
    let node = ReferenceNode::new(Some(ProtocolVersion::Regtest)).unwrap();
    let protocol = node.protocol();
    
    // Verify protocol version
    assert_eq!(protocol.get_protocol_version(), &ProtocolVersion::Regtest);
    
    // Test feature support
    assert!(protocol.supports_feature("fast_mining"));
}

#[test]
fn test_consensus_integration() {
    // Test consensus validation through bllvm-protocol
    let node = ReferenceNode::new(None).unwrap(); // Uses default Regtest
    let protocol = node.protocol();
    
    // Create a simple transaction
    let tx = Transaction {
        version: 1,
        inputs: vec![],
        outputs: vec![TransactionOutput {
            value: 1000,
            script_pubkey: vec![0x51], // OP_1
        }],
        lock_time: 0,
    };
    
    // Test transaction validation
    let result = protocol.validate_transaction(&tx);
    assert!(result.is_ok());
}

#[test]
fn test_bllvm_node_creation() {
    // Test default (Regtest) creation
    let node = ReferenceNode::new(None).unwrap();
    assert_eq!(node.protocol().get_protocol_version(), &ProtocolVersion::Regtest);
    
    // Test mainnet creation
    let mainnet_node = ReferenceNode::new(Some(ProtocolVersion::BitcoinV1)).unwrap();
    assert_eq!(mainnet_node.protocol().get_protocol_version(), &ProtocolVersion::BitcoinV1);
}
