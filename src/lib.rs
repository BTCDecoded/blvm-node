//! Reference Node - Minimal Bitcoin implementation using blvm-protocol
//!
//! This crate provides a minimal, production-ready Bitcoin node implementation
//! that uses the blvm-protocol crate for protocol abstraction and blvm-consensus
//! for all consensus decisions. It adds only the non-consensus infrastructure:
//! storage, networking, RPC, and orchestration.
//!
//! ## 5-Tier Architecture
//!
//! 1. Orange Paper (mathematical foundation)
//! 2. blvm-consensus (pure math implementation)
//! 3. blvm-protocol (Bitcoin abstraction) ← USED HERE
//! 4. blvm-node (full node implementation) ← THIS CRATE
//! 5. blvm-sdk (ergonomic API - future)
//!
//! ## Design Principles
//!
//! 1. **Zero Consensus Re-implementation**: All consensus logic from blvm-consensus
//! 2. **Protocol Abstraction**: Uses blvm-protocol for variant support
//! 3. **Pure Infrastructure**: Only adds storage, networking, RPC, orchestration
//! 4. **Production Ready**: Full Bitcoin node functionality

// Allow dead code - many fields/functions are part of the API or for future use
#![allow(dead_code)]
// Allow design-level clippy warnings that would require significant refactoring
#![allow(clippy::too_many_arguments)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::type_complexity)] // Module storage, RPC types; refactor would be large
#![allow(clippy::arc_with_non_send_sync)] // ZMQ Socket is not Sync; zmq crate limitation
#![allow(clippy::field_reassign_with_default)] // Many config structs; init would be verbose
#![allow(clippy::collapsible_match)] // Readability preference for nested matches
#![allow(unused_doc_comments, unused_imports, unused_variables, unused_mut)] // Many feature-gated paths; fix in follow-up
                                                                             // Memory allocator optimization using mimalloc (faster than default allocator)
                                                                             // Note: Only in blvm-node, not blvm-consensus
                                                                             // Disabled for Windows cross-compilation (mimalloc linking issues with MinGW)
                                                                             // Temporarily disabled for IBD memory profiling — mimalloc arenas
                                                                             // retain freed pages and inflate RSS on 16GB boxes.
                                                                             // Use the default Rust allocator in `cargo test` — mimalloc has caused SIGSEGV in
                                                                             // integration tests (UTXO Merkle / handler paths) while the same code passes under the
                                                                             // system allocator and in `blvm-protocol` tests.
#[cfg(all(not(target_os = "windows"), feature = "mimalloc", not(test)))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub mod bip21;
pub mod cli;
pub mod config;
#[cfg(feature = "miniscript")]
pub mod miniscript;
pub mod module;
pub mod network;
pub mod node;
pub mod payment;
pub mod rpc;
pub mod storage;
pub mod utils;
#[cfg(feature = "production")]
pub mod validation;
#[cfg(feature = "zmq")]
extern crate zeromq; // zeromq = { package = "zmq" } in Cargo.toml
#[cfg(feature = "zmq")]
pub mod zmq;

// Re-export config module
pub use config::*;

// Re-export commonly used types from blvm-protocol
// This allows depending only on blvm-protocol (which transitively provides blvm-consensus)
pub use blvm_protocol::mempool::Mempool;
pub use blvm_protocol::{
    tx_inputs, tx_outputs, Block, BlockHeader, ByteString, ConsensusError, Hash, Integer, Natural,
    OutPoint, Result, Transaction, TransactionInput, TransactionOutput, UtxoSet, ValidationResult,
    UTXO,
};

// Re-export blvm-protocol types
pub use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};

/// Main node implementation
pub struct Node {
    protocol: BitcoinProtocolEngine,
    /// Storage for blockchain data (optional, can be added via builder pattern)
    storage: Option<std::sync::Arc<crate::storage::Storage>>,
    /// Network manager for P2P connections (optional, can be added via builder pattern)
    network_manager: Option<std::sync::Arc<tokio::sync::RwLock<crate::network::NetworkManager>>>,
    /// Mempool manager for transaction handling (optional, can be added via builder pattern)
    mempool_manager: Option<std::sync::Arc<crate::node::mempool::MempoolManager>>,
}

impl Node {
    /// Create a new node with specified protocol variant
    /// Defaults to Regtest for safe development/testing
    pub fn new(version: Option<ProtocolVersion>) -> anyhow::Result<Self> {
        let version = version.unwrap_or(ProtocolVersion::Regtest);
        Ok(Self {
            protocol: BitcoinProtocolEngine::new(version)?,
            storage: None,
            network_manager: None,
            mempool_manager: None,
        })
    }

    /// Create with storage
    pub fn with_storage(mut self, storage: std::sync::Arc<crate::storage::Storage>) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Create with network manager
    pub fn with_network_manager(
        mut self,
        network_manager: std::sync::Arc<tokio::sync::RwLock<crate::network::NetworkManager>>,
    ) -> Self {
        self.network_manager = Some(network_manager);
        self
    }

    /// Create with mempool manager
    pub fn with_mempool_manager(
        mut self,
        mempool_manager: std::sync::Arc<crate::node::mempool::MempoolManager>,
    ) -> Self {
        self.mempool_manager = Some(mempool_manager);
        self
    }

    /// Get the protocol engine
    pub fn protocol(&self) -> &BitcoinProtocolEngine {
        &self.protocol
    }

    /// Get storage (if available)
    pub fn storage(&self) -> Option<&std::sync::Arc<crate::storage::Storage>> {
        self.storage.as_ref()
    }

    /// Get network manager (if available)
    pub fn network_manager(
        &self,
    ) -> Option<&std::sync::Arc<tokio::sync::RwLock<crate::network::NetworkManager>>> {
        self.network_manager.as_ref()
    }

    /// Get mempool manager (if available)
    pub fn mempool_manager(&self) -> Option<&std::sync::Arc<crate::node::mempool::MempoolManager>> {
        self.mempool_manager.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_integration() {
        // Test that blvm-protocol works in blvm-node context
        let node = Node::new(Some(ProtocolVersion::Regtest)).unwrap();
        let protocol = node.protocol();

        // Verify protocol version
        assert_eq!(protocol.get_protocol_version(), ProtocolVersion::Regtest);

        // Test feature support
        assert!(protocol.supports_feature("fast_mining"));
    }

    #[test]
    fn test_consensus_integration() {
        // Test consensus validation through blvm-protocol
        let node = Node::new(None).unwrap(); // Uses default Regtest
        let protocol = node.protocol();

        // Create a simple transaction
        let tx = Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 1000,
                script_pubkey: vec![blvm_protocol::opcodes::OP_1],
            }],
            lock_time: 0,
        };

        // Test transaction validation
        let result = protocol.validate_transaction(&tx);
        assert!(result.is_ok());
    }

    #[test]
    fn test_node_creation() {
        // Test default (Regtest) creation
        let node = Node::new(None).unwrap();
        assert_eq!(
            node.protocol().get_protocol_version(),
            ProtocolVersion::Regtest
        );

        // Test mainnet creation
        let mainnet_node = Node::new(Some(ProtocolVersion::BitcoinV1)).unwrap();
        assert_eq!(
            mainnet_node.protocol().get_protocol_version(),
            ProtocolVersion::BitcoinV1
        );
    }
}
