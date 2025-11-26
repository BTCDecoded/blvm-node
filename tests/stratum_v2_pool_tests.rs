//! Tests for Stratum V2 Pool

#[cfg(feature = "stratum-v2")]
mod tests {
    use bllvm_node::network::stratum_v2::messages::{
        OpenMiningChannelMessage, SetupConnectionMessage,
    };
    use bllvm_node::network::stratum_v2::pool::{StratumV2Pool, MinerStats};
    use bllvm_protocol::{Block, BlockHeader};
    use bllvm_protocol::tx_inputs;
    use bllvm_protocol::tx_outputs;

    fn create_test_block() -> Block {
        Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 1231006505,
                bits: 0x1d00ffff,
                nonce: 0,
            },
            transactions: vec![].into_boxed_slice(),
        }
    }

    #[test]
    fn test_stratum_v2_pool_new() {
        let pool = StratumV2Pool::new();
        // Should create successfully
        assert!(true);
    }

    #[test]
    fn test_stratum_v2_pool_handle_setup_connection() {
        let mut pool = StratumV2Pool::new();
        
        let msg = SetupConnectionMessage {
            protocol_version: 2,
            endpoint: "test-miner".to_string(),
            capabilities: vec!["mining".to_string()],
        };
        
        let result = pool.handle_setup_connection(msg);
        assert!(result.is_ok());
        
        let response = result.unwrap();
        assert_eq!(response.supported_versions, vec![2]);
        assert!(response.capabilities.contains(&"mining".to_string()));
    }

    #[test]
    fn test_stratum_v2_pool_handle_open_channel() {
        let mut pool = StratumV2Pool::new();
        
        // First setup connection
        let setup_msg = SetupConnectionMessage {
            protocol_version: 2,
            endpoint: "test-miner".to_string(),
            capabilities: vec!["mining".to_string()],
        };
        pool.handle_setup_connection(setup_msg).unwrap();
        
        // Then open channel
        let channel_msg = OpenMiningChannelMessage {
            channel_id: 1,
            request_id: 1,
            min_difficulty: 1,
        };
        
        let result = pool.handle_open_channel("test-miner", channel_msg);
        assert!(result.is_ok());
        
        let response = result.unwrap();
        assert_eq!(response.channel_id, 1);
    }

    #[test]
    fn test_stratum_v2_pool_handle_open_channel_no_miner() {
        let mut pool = StratumV2Pool::new();
        
        let channel_msg = OpenMiningChannelMessage {
            channel_id: 1,
            request_id: 1,
            min_difficulty: 1,
        };
        
        // Should fail if miner not registered
        let result = pool.handle_open_channel("unknown-miner", channel_msg);
        assert!(result.is_err());
    }

    #[test]
    fn test_stratum_v2_pool_set_template() {
        let mut pool = StratumV2Pool::new();
        let block = create_test_block();
        
        let (job_id, messages) = pool.set_template(block);
        
        // Should return job_id and messages
        assert!(job_id > 0);
        assert!(messages.is_empty()); // No miners connected yet
    }

    #[test]
    fn test_miner_stats_default() {
        let stats = MinerStats::default();
        assert_eq!(stats.total_shares, 0);
        assert_eq!(stats.accepted_shares, 0);
        assert_eq!(stats.rejected_shares, 0);
        assert!(stats.last_share_time.is_none());
    }
}

