//! Reference Node - Minimal Bitcoin implementation using protocol-engine
//!
//! This crate provides a minimal, production-ready Bitcoin node implementation
//! that uses the protocol-engine crate for protocol abstraction and consensus-proof
//! for all consensus decisions. It adds only the non-consensus infrastructure:
//! storage, networking, RPC, and orchestration.
//!
//! ## 5-Tier Architecture
//!
//! 1. Orange Paper (mathematical foundation)
//! 2. consensus-proof (pure math implementation)
//! 3. protocol-engine (Bitcoin abstraction) ← USED HERE
//! 4. reference-node (full node implementation) ← THIS CRATE
//! 5. developer-sdk (ergonomic API - future)
//!
//! ## Design Principles
//!
//! 1. **Zero Consensus Re-implementation**: All consensus logic from consensus-proof
//! 2. **Protocol Abstraction**: Uses protocol-engine for variant support
//! 3. **Pure Infrastructure**: Only adds storage, networking, RPC, orchestration
//! 4. **Production Ready**: Full Bitcoin node functionality

// Allow dead code - many fields/functions are part of the API or for future use
#![allow(dead_code)]

// Memory allocator optimization using mimalloc (faster than default allocator)
// Note: Only in reference-node, not consensus-proof, to maintain Kani compatibility
// Disabled for Windows cross-compilation (mimalloc linking issues with MinGW)
#[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub mod bip21;
pub mod config;
#[cfg(feature = "governance")]
pub mod governance;
pub mod module;
pub mod network;
pub mod node;
pub mod rpc;
pub mod storage;
pub mod utils;
#[cfg(feature = "production")]
pub mod validation;
#[cfg(feature = "zmq")]
pub mod zmq;

// Re-export config module
pub use config::*;

// Re-export commonly used types from protocol-engine
// This allows depending only on protocol-engine (which transitively provides consensus-proof)
pub use bllvm_protocol::mempool::Mempool;
pub use bllvm_protocol::{
    Block, BlockHeader, ByteString, ConsensusError, Hash, Integer, Natural, OutPoint, Result,
    Transaction, TransactionInput, TransactionOutput, UtxoSet, ValidationResult, UTXO,
};

// Re-export protocol-engine types
pub use bllvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};

/// Main reference node implementation
pub struct ReferenceNode {
    protocol: BitcoinProtocolEngine,
    // TODO: Add other components as they're implemented
}

impl ReferenceNode {
    /// Create a new reference node with specified protocol variant
    /// Defaults to Regtest for safe development/testing
    pub fn new(version: Option<ProtocolVersion>) -> anyhow::Result<Self> {
        let version = version.unwrap_or(ProtocolVersion::Regtest);
        Ok(Self {
            protocol: BitcoinProtocolEngine::new(version)?,
        })
    }

    /// Get the protocol engine
    pub fn protocol(&self) -> &BitcoinProtocolEngine {
        &self.protocol
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_integration() {
        // Test that protocol-engine works in reference-node context
        let node = ReferenceNode::new(Some(ProtocolVersion::Regtest)).unwrap();
        let protocol = node.protocol();

        // Verify protocol version
        assert_eq!(protocol.get_protocol_version(), ProtocolVersion::Regtest);

        // Test feature support
        assert!(protocol.supports_feature("fast_mining"));
    }

    #[test]
    fn test_consensus_integration() {
        // Test consensus validation through protocol-engine
        let node = ReferenceNode::new(None).unwrap(); // Uses default Regtest
        let protocol = node.protocol();

        // Create a simple transaction
        let tx = Transaction {
            version: 1,
            inputs: bllvm_protocol::tx_inputs![],
            outputs: bllvm_protocol::tx_outputs![TransactionOutput {
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
    fn test_reference_node_creation() {
        // Test default (Regtest) creation
        let node = ReferenceNode::new(None).unwrap();
        assert_eq!(
            node.protocol().get_protocol_version(),
            ProtocolVersion::Regtest
        );

        // Test mainnet creation
        let mainnet_node = ReferenceNode::new(Some(ProtocolVersion::BitcoinV1)).unwrap();
        assert_eq!(
            mainnet_node.protocol().get_protocol_version(),
            ProtocolVersion::BitcoinV1
        );
    }
}
