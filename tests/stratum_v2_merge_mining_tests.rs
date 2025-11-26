//! Tests for Stratum V2 Merge Mining

#[cfg(feature = "stratum-v2")]
mod tests {
    use bllvm_node::network::stratum_v2::merge_mining::{
        MergeMiningCoordinator, SecondaryChain,
    };

    fn create_test_chain(chain_id: &str) -> SecondaryChain {
        SecondaryChain {
            chain_id: chain_id.to_string(),
            chain_name: format!("{} Chain", chain_id),
            enabled: false,
        }
    }

    #[test]
    fn test_merge_mining_coordinator_new() {
        let chains = vec![
            create_test_chain("rsk"),
            create_test_chain("namecoin"),
        ];
        let coordinator = MergeMiningCoordinator::new(chains);
        
        assert_eq!(coordinator.get_enabled_chains().len(), 0);
    }

    #[test]
    fn test_merge_mining_coordinator_enable_chain() {
        let chains = vec![create_test_chain("rsk")];
        let mut coordinator = MergeMiningCoordinator::new(chains);
        
        let result = coordinator.enable_chain("rsk");
        assert!(result.is_ok());
        
        assert_eq!(coordinator.get_enabled_chains().len(), 1);
    }

    #[test]
    fn test_merge_mining_coordinator_enable_unknown_chain() {
        let chains = vec![create_test_chain("rsk")];
        let mut coordinator = MergeMiningCoordinator::new(chains);
        
        let result = coordinator.enable_chain("unknown");
        assert!(result.is_err());
    }

    #[test]
    fn test_merge_mining_coordinator_create_channel() {
        let chains = vec![create_test_chain("rsk")];
        let mut coordinator = MergeMiningCoordinator::new(chains);
        
        coordinator.enable_chain("rsk").unwrap();
        
        let result = coordinator.create_channel("rsk", 1);
        assert!(result.is_ok());
        
        let channel = coordinator.get_channel("rsk");
        assert!(channel.is_some());
        assert_eq!(channel.unwrap().channel_id, 1);
    }

    #[test]
    fn test_merge_mining_coordinator_create_channel_not_enabled() {
        let chains = vec![create_test_chain("rsk")];
        let mut coordinator = MergeMiningCoordinator::new(chains);
        
        // Don't enable chain
        let result = coordinator.create_channel("rsk", 1);
        assert!(result.is_err());
    }

    #[test]
    fn test_merge_mining_coordinator_update_job() {
        let chains = vec![create_test_chain("rsk")];
        let mut coordinator = MergeMiningCoordinator::new(chains);
        
        coordinator.enable_chain("rsk").unwrap();
        coordinator.create_channel("rsk", 1).unwrap();
        
        let result = coordinator.update_job("rsk", 100);
        assert!(result.is_ok());
        
        let channel = coordinator.get_channel("rsk").unwrap();
        assert_eq!(channel.current_job_id, Some(100));
    }

    #[test]
    fn test_merge_mining_coordinator_record_share() {
        let chains = vec![create_test_chain("rsk")];
        let mut coordinator = MergeMiningCoordinator::new(chains);
        
        coordinator.enable_chain("rsk").unwrap();
        coordinator.create_channel("rsk", 1).unwrap();
        
        let result = coordinator.record_share("rsk", 5);
        assert!(result.is_ok());
        
        let channel = coordinator.get_channel("rsk").unwrap();
        assert_eq!(channel.shares_submitted, 5);
    }

    #[test]
    fn test_merge_mining_coordinator_record_reward() {
        let chains = vec![create_test_chain("rsk")];
        let mut coordinator = MergeMiningCoordinator::new(chains);
        
        coordinator.enable_chain("rsk").unwrap();
        coordinator.create_channel("rsk", 1).unwrap();
        
        let result = coordinator.record_reward("rsk", 1000);
        assert!(result.is_ok());
        
        let channel = coordinator.get_channel("rsk").unwrap();
        assert_eq!(channel.total_rewards, 1000);
    }

    #[test]
    fn test_merge_mining_coordinator_revenue_distribution() {
        let chains = vec![create_test_chain("rsk")];
        let mut coordinator = MergeMiningCoordinator::new(chains);
        
        coordinator.enable_chain("rsk").unwrap();
        coordinator.create_channel("rsk", 1).unwrap();
        coordinator.record_reward("rsk", 1000).unwrap();
        
        let distribution = coordinator.get_total_revenue_distribution();
        assert_eq!(distribution.core, 600); // 60%
        assert_eq!(distribution.grants, 250); // 25%
        assert_eq!(distribution.audits, 100); // 10%
        assert_eq!(distribution.operations, 50); // 5%
    }

    #[test]
    fn test_merge_mining_coordinator_get_chain_stats() {
        let chains = vec![create_test_chain("rsk")];
        let mut coordinator = MergeMiningCoordinator::new(chains);
        
        coordinator.enable_chain("rsk").unwrap();
        coordinator.create_channel("rsk", 1).unwrap();
        coordinator.update_job("rsk", 100).unwrap();
        coordinator.record_share("rsk", 10).unwrap();
        coordinator.record_reward("rsk", 500).unwrap();
        
        let stats = coordinator.get_chain_stats("rsk");
        assert!(stats.is_some());
        let stats = stats.unwrap();
        assert_eq!(stats.chain_id, "rsk");
        assert_eq!(stats.channel_id, 1);
        assert_eq!(stats.total_rewards, 500);
        assert_eq!(stats.shares_submitted, 10);
        assert_eq!(stats.current_job_id, Some(100));
    }

    #[test]
    fn test_merge_mining_coordinator_get_all_channels() {
        let chains = vec![
            create_test_chain("rsk"),
            create_test_chain("namecoin"),
        ];
        let mut coordinator = MergeMiningCoordinator::new(chains);
        
        coordinator.enable_chain("rsk").unwrap();
        coordinator.enable_chain("namecoin").unwrap();
        coordinator.create_channel("rsk", 1).unwrap();
        coordinator.create_channel("namecoin", 2).unwrap();
        
        let channels = coordinator.get_all_channels();
        assert_eq!(channels.len(), 2);
    }
}

