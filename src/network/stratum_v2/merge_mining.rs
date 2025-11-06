//! Merge Mining Coordination for Stratum V2
//!
//! Implements merge mining coordination via Stratum V2 multiplexed channels,
//! allowing simultaneous mining of Bitcoin and secondary chains (e.g., RSK, Namecoin).
//!
//! Uses QUIC's native stream multiplexing to support multiple mining channels
//! over a single connection.

use crate::network::stratum_v2::error::{StratumV2Error, StratumV2Result};
use crate::network::stratum_v2::messages::*;
use protocol_engine::types::{Block, Hash};
use std::collections::HashMap;
use tracing::{debug, info, warn};

/// Secondary chain configuration for merge mining
#[derive(Debug, Clone)]
pub struct SecondaryChain {
    /// Chain identifier (e.g., "rsk", "namecoin")
    pub chain_id: String,
    /// Chain name for display
    pub chain_name: String,
    /// Merge mining enabled
    pub enabled: bool,
}

/// Merge mining channel per chain
#[derive(Debug, Clone)]
pub struct MergeMiningChannel {
    /// Chain identifier
    pub chain_id: String,
    /// Stratum V2 channel ID for this chain
    pub channel_id: u32,
    /// Current job ID for this chain
    pub current_job_id: Option<u32>,
    /// Total rewards mined for this chain
    pub total_rewards: u64,
    /// Shares submitted for this chain
    pub shares_submitted: u64,
}

/// Revenue distribution breakdown
#[derive(Debug, Clone)]
pub struct RevenueDistribution {
    /// Core development (60%)
    pub core: u64,
    /// Grants (25%)
    pub grants: u64,
    /// Audits (10%)
    pub audits: u64,
    /// Operations (5%)
    pub operations: u64,
}

/// Merge mining coordinator
pub struct MergeMiningCoordinator {
    /// Secondary chains configuration
    secondary_chains: Vec<SecondaryChain>,
    /// Active merge mining channels (chain_id -> channel info)
    channels: HashMap<String, MergeMiningChannel>,
    /// Total revenue tracked
    total_revenue: u64,
    /// Revenue per chain
    chain_revenue: HashMap<String, u64>,
}

impl MergeMiningCoordinator {
    /// Create a new merge mining coordinator
    pub fn new(secondary_chains: Vec<SecondaryChain>) -> Self {
        Self {
            secondary_chains,
            channels: HashMap::new(),
            total_revenue: 0,
            chain_revenue: HashMap::new(),
        }
    }

    /// Enable merge mining for a chain
    pub fn enable_chain(&mut self, chain_id: &str) -> StratumV2Result<()> {
        if let Some(chain) = self
            .secondary_chains
            .iter_mut()
            .find(|c| c.chain_id == chain_id)
        {
            chain.enabled = true;
            info!("Enabled merge mining for chain: {}", chain_id);
            Ok(())
        } else {
            Err(StratumV2Error::Configuration(format!(
                "Chain not found: {}",
                chain_id
            )))
        }
    }

    /// Create merge mining channel for a chain
    pub fn create_channel(&mut self, chain_id: &str, channel_id: u32) -> StratumV2Result<()> {
        // Verify chain is enabled
        let chain = self
            .secondary_chains
            .iter()
            .find(|c| c.chain_id == chain_id && c.enabled)
            .ok_or_else(|| {
                StratumV2Error::Configuration(format!("Chain not enabled: {}", chain_id))
            })?;

        let merge_channel = MergeMiningChannel {
            chain_id: chain_id.to_string(),
            channel_id,
            current_job_id: None,
            total_rewards: 0,
            shares_submitted: 0,
        };

        self.channels.insert(chain_id.to_string(), merge_channel);
        info!(
            "Created merge mining channel {} for chain {}",
            channel_id, chain_id
        );

        Ok(())
    }

    /// Update job for a merge mining channel
    pub fn update_job(&mut self, chain_id: &str, job_id: u32) -> StratumV2Result<()> {
        if let Some(channel) = self.channels.get_mut(chain_id) {
            channel.current_job_id = Some(job_id);
            debug!("Updated job {} for chain {}", job_id, chain_id);
            Ok(())
        } else {
            Err(StratumV2Error::MiningJob(format!(
                "Channel not found for chain: {}",
                chain_id
            )))
        }
    }

    /// Record share submission for a chain
    pub fn record_share(&mut self, chain_id: &str, share_count: u64) -> StratumV2Result<()> {
        if let Some(channel) = self.channels.get_mut(chain_id) {
            channel.shares_submitted += share_count;
            debug!("Recorded {} shares for chain {}", share_count, chain_id);
            Ok(())
        } else {
            Err(StratumV2Error::MiningJob(format!(
                "Channel not found for chain: {}",
                chain_id
            )))
        }
    }

    /// Record reward for a chain
    pub fn record_reward(&mut self, chain_id: &str, reward: u64) -> StratumV2Result<()> {
        if let Some(channel) = self.channels.get_mut(chain_id) {
            channel.total_rewards += reward;
            *self.chain_revenue.entry(chain_id.to_string()).or_insert(0) += reward;
            self.total_revenue += reward;
            info!(
                "Recorded reward {} for chain {} (total: {})",
                reward, chain_id, channel.total_rewards
            );
            Ok(())
        } else {
            Err(StratumV2Error::MiningJob(format!(
                "Channel not found for chain: {}",
                chain_id
            )))
        }
    }

    /// Calculate revenue distribution (per whitepaper: 60% core, 25% grants, 10% audits, 5% ops)
    pub fn calculate_revenue_distribution(&self, total_revenue: u64) -> RevenueDistribution {
        RevenueDistribution {
            core: (total_revenue * 60) / 100,
            grants: (total_revenue * 25) / 100,
            audits: (total_revenue * 10) / 100,
            operations: (total_revenue * 5) / 100,
        }
    }

    /// Get revenue distribution for total tracked revenue
    pub fn get_total_revenue_distribution(&self) -> RevenueDistribution {
        self.calculate_revenue_distribution(self.total_revenue)
    }

    /// Get revenue distribution per chain
    pub fn get_chain_revenue_distribution(&self, chain_id: &str) -> Option<RevenueDistribution> {
        self.chain_revenue
            .get(chain_id)
            .map(|&revenue| self.calculate_revenue_distribution(revenue))
    }

    /// Get enabled chains
    pub fn get_enabled_chains(&self) -> Vec<&SecondaryChain> {
        self.secondary_chains.iter().filter(|c| c.enabled).collect()
    }

    /// Get merge mining channel for a chain
    pub fn get_channel(&self, chain_id: &str) -> Option<&MergeMiningChannel> {
        self.channels.get(chain_id)
    }

    /// Get all active channels
    pub fn get_all_channels(&self) -> Vec<&MergeMiningChannel> {
        self.channels.values().collect()
    }

    /// Get chain statistics
    pub fn get_chain_stats(&self, chain_id: &str) -> Option<ChainStatistics> {
        self.channels.get(chain_id).map(|channel| ChainStatistics {
            chain_id: chain_id.to_string(),
            channel_id: channel.channel_id,
            total_rewards: channel.total_rewards,
            shares_submitted: channel.shares_submitted,
            current_job_id: channel.current_job_id,
        })
    }
}

/// Chain statistics for merge mining
#[derive(Debug, Clone)]
pub struct ChainStatistics {
    pub chain_id: String,
    pub channel_id: u32,
    pub total_rewards: u64,
    pub shares_submitted: u64,
    pub current_job_id: Option<u32>,
}

impl Default for MergeMiningCoordinator {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}
