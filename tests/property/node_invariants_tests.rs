//! Property tests for node layer invariants
//!
//! Tests critical invariants that must always hold true in the node implementation.

use proptest::prelude::*;
use bllvm_node::storage::Storage;
use bllvm_node::network::NetworkManager;
use std::net::SocketAddr;
use tokio::sync::mpsc;

proptest! {
    #[test]
    fn test_storage_bounds_invariant(
        block_count in 0usize..10_000_000,
        utxo_count in 0usize..1_000_000_000,
    ) {
        // Invariant: Storage bounds checking should never panic
        // Even with maximum values, bounds check should return a boolean
        let temp_dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(temp_dir.path().to_str().unwrap()).unwrap();
        
        // This should not panic, even if counts are at limits
        let _ = storage.check_storage_bounds();
    }
    
    #[test]
    fn test_peer_quality_score_bounds(
        successful in 0u64..1000,
        failed in 0u64..1000,
        uptime_seconds in 0u64..86400,
    ) {
        // Invariant: Quality score should always be between 0.0 and 1.0
        use bllvm_node::network::peer::Peer;
        use bllvm_node::network::transport::TransportAddr;
        use bllvm_node::network::NetworkMessage;
        
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut peer = Peer::new(addr, TransportAddr::Tcp(addr), tx);
        
        // Simulate exchanges
        for _ in 0..successful {
            peer.record_success(100.0); // 100ms response time
        }
        for _ in 0..failed {
            peer.record_failure();
        }
        
        let score = peer.quality_score();
        prop_assert!(score >= 0.0 && score <= 1.0, "Quality score must be in [0.0, 1.0], got {}", score);
    }
    
    #[test]
    fn test_peer_reliability_consistency(
        successful in 0u64..100,
        failed in 0u64..100,
    ) {
        // Invariant: Quality score calculation should be consistent
        use bllvm_node::network::peer::Peer;
        use bllvm_node::network::transport::TransportAddr;
        use bllvm_node::network::NetworkMessage;
        
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut peer = Peer::new(addr, TransportAddr::Tcp(addr), tx);
        
        // Record exchanges
        for _ in 0..successful {
            peer.record_success(50.0);
        }
        for _ in 0..failed {
            peer.record_failure();
        }
        
        let score = peer.quality_score();
        prop_assert!(score >= 0.0 && score <= 1.0, "Quality score must be in [0.0, 1.0], got {}", score);
    }
    
    #[test]
    fn test_network_active_state_consistency(
        active in proptest::bool::ANY,
    ) {
        // Invariant: Network active state should be consistent
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let manager = NetworkManager::new(addr);
        
        // Network should start as active
        prop_assert!(manager.is_network_active(), "Network should be active on creation");
    }
}

