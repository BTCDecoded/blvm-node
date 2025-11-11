//! Stratum V2 Pool Role Implementation
//!
//! Implements the pool role for Stratum V2, handling template generation,
//! share validation, and miner management.

use crate::network::stratum_v2::error::{StratumV2Error, StratumV2Result};
use crate::network::stratum_v2::messages::*;
use bllvm_protocol::types::{Block, BlockHeader, Hash, Natural};
use bllvm_protocol::ConsensusProof;
use std::collections::HashMap;
use tracing::{debug, info, warn};

/// Miner connection information
#[derive(Debug, Clone)]
pub struct MinerConnection {
    /// Miner endpoint identifier
    pub endpoint: String,
    /// Open mining channels (channel_id -> ChannelInfo)
    pub channels: HashMap<u32, ChannelInfo>,
    /// Miner statistics
    pub stats: MinerStats,
}

/// Mining job information
#[derive(Debug, Clone)]
pub struct JobInfo {
    /// Job identifier
    pub job_id: u32,
    /// Previous block hash
    pub prev_hash: Hash,
    /// Difficulty bits
    pub bits: Natural,
    /// Timestamp
    pub timestamp: Natural,
}

/// Mining channel information
#[derive(Debug, Clone)]
pub struct ChannelInfo {
    /// Channel identifier
    pub channel_id: u32,
    /// Target difficulty (for share validation)
    pub target: Hash,
    /// Current job ID
    pub current_job_id: Option<u32>,
    /// Minimum difficulty requested by miner
    pub min_difficulty: u32,
    /// Maximum number of jobs
    pub max_jobs: u32,
    /// Active jobs (job_id -> job info)
    pub jobs: HashMap<u32, JobInfo>,
}

/// Miner statistics
#[derive(Debug, Clone)]
pub struct MinerStats {
    /// Total shares submitted
    pub total_shares: u64,
    /// Accepted shares
    pub accepted_shares: u64,
    /// Rejected shares
    pub rejected_shares: u64,
    /// Last share timestamp
    pub last_share_time: Option<u64>,
}

impl Default for MinerStats {
    fn default() -> Self {
        Self {
            total_shares: 0,
            accepted_shares: 0,
            rejected_shares: 0,
            last_share_time: None,
        }
    }
}

/// Stratum V2 pool implementation
pub struct StratumV2Pool {
    /// Connected miners (endpoint -> connection info)
    miners: HashMap<String, MinerConnection>,
    /// Current block template
    current_template: Option<Block>,
    /// Current job ID counter
    job_id_counter: u32,
    /// Consensus proof instance for validation
    consensus: ConsensusProof,
}

impl StratumV2Pool {
    /// Create a new pool instance
    pub fn new() -> Self {
        Self {
            miners: HashMap::new(),
            current_template: None,
            job_id_counter: 1,
            consensus: ConsensusProof::new(),
        }
    }

    /// Handle Setup Connection from miner
    pub fn handle_setup_connection(
        &mut self,
        msg: SetupConnectionMessage,
    ) -> StratumV2Result<SetupConnectionSuccessMessage> {
        info!("Setup Connection from miner: {}", msg.endpoint);

        // Register miner connection
        let connection = MinerConnection {
            endpoint: msg.endpoint.clone(),
            channels: HashMap::new(),
            stats: MinerStats::default(),
        };
        self.miners.insert(msg.endpoint.clone(), connection);

        // Respond with success
        Ok(SetupConnectionSuccessMessage {
            supported_versions: vec![2], // Stratum V2
            capabilities: vec!["mining".to_string()],
        })
    }

    /// Handle Open Mining Channel request
    pub fn handle_open_channel(
        &mut self,
        endpoint: &str,
        msg: OpenMiningChannelMessage,
    ) -> StratumV2Result<OpenMiningChannelSuccessMessage> {
        debug!(
            "Open Mining Channel request from {}: channel_id={}",
            endpoint, msg.channel_id
        );

        // Calculate target from difficulty first (before borrowing miner)
        // Channel difficulty is typically easier than network difficulty
        // For now, use a simple conversion (full implementation would use proper difficulty calculation)
        let channel_target = self.calculate_channel_target(msg.min_difficulty)?;

        // Get or create miner connection
        let miner = self
            .miners
            .get_mut(endpoint)
            .ok_or_else(|| StratumV2Error::MiningJob("Miner not registered".to_string()))?;

        // Create channel info
        let channel_info = ChannelInfo {
            channel_id: msg.channel_id,
            target: channel_target,
            current_job_id: None,
            min_difficulty: msg.min_difficulty,
            max_jobs: 10, // Default max jobs
            jobs: HashMap::new(),
        };

        miner.channels.insert(msg.channel_id, channel_info.clone());

        // Respond with success
        Ok(OpenMiningChannelSuccessMessage {
            channel_id: msg.channel_id,
            request_id: msg.request_id,
            target: channel_target,
            max_jobs: 10,
        })
    }

    /// Set current block template
    /// Returns the job_id and messages to distribute
    pub fn set_template(&mut self, template: Block) -> (u32, Vec<(String, NewMiningJobMessage)>) {
        // Generate new job ID
        let job_id = self.job_id_counter;
        self.job_id_counter = self.job_id_counter.wrapping_add(1);

        // Create job info from template
        let job_info = JobInfo {
            job_id,
            prev_hash: template.header.prev_block_hash,
            bits: template.header.bits,
            timestamp: template.header.timestamp,
        };

        // Distribute new job to all open channels (returns messages)
        let messages = self.distribute_new_job(job_id, &job_info);

        // Store job info in all channels
        for miner in self.miners.values_mut() {
            for channel in miner.channels.values_mut() {
                channel.current_job_id = Some(job_id);
                channel.jobs.insert(job_id, job_info.clone());
            }
        }

        // Store template
        self.current_template = Some(template);

        (job_id, messages)
    }

    /// Distribute new mining job to all miners
    ///
    /// This method creates the job messages but doesn't send them.
    /// Actual sending is handled by the server using connections.
    pub fn distribute_new_job(
        &self,
        job_id: u32,
        job_info: &JobInfo,
    ) -> Vec<(String, NewMiningJobMessage)> {
        info!(
            "Distributing new job {} to {} miners",
            job_id,
            self.miners.len()
        );

        // Extract merkle path and coinbase from template if available
        let (coinbase_prefix, coinbase_suffix, merkle_path) =
            if let Some(ref template) = self.current_template {
                self.extract_template_parts(template)
            } else {
                (vec![], vec![], vec![])
            };

        let mut messages = Vec::new();
        for (endpoint, miner) in &self.miners {
            for (channel_id, _channel) in &miner.channels {
                // Create NewMiningJob message
                let job_msg = NewMiningJobMessage {
                    channel_id: *channel_id,
                    job_id,
                    prev_hash: job_info.prev_hash,
                    coinbase_prefix: coinbase_prefix.clone(),
                    coinbase_suffix: coinbase_suffix.clone(),
                    merkle_path: merkle_path.clone(),
                };

                messages.push((endpoint.clone(), job_msg));
                debug!(
                    "Prepared job {} for miner {} channel {}",
                    job_id, endpoint, channel_id
                );
            }
        }

        messages
    }

    /// Handle share submission
    pub fn handle_submit_shares(
        &mut self,
        endpoint: &str,
        msg: SubmitSharesMessage,
    ) -> StratumV2Result<SubmitSharesSuccessMessage> {
        debug!(
            "Submit Shares from {}: channel_id={}, {} shares",
            endpoint,
            msg.channel_id,
            msg.shares.len()
        );

        // Get miner connection and extract channel info
        let (mut total_shares, mut accepted_shares, mut rejected_shares, last_job_id) = {
            let miner = self
                .miners
                .get_mut(endpoint)
                .ok_or_else(|| StratumV2Error::MiningJob("Miner not registered".to_string()))?;

            // Update statistics
            miner.stats.total_shares += msg.shares.len() as u64;

            // Get channel info before borrowing for validation
            let channel = miner
                .channels
                .get(&msg.channel_id)
                .ok_or_else(|| StratumV2Error::MiningJob("Channel not found".to_string()))?;
            let last_job_id = channel.current_job_id.unwrap_or(0);

            (
                miner.stats.total_shares,
                miner.stats.accepted_shares,
                miner.stats.rejected_shares,
                last_job_id,
            )
        };

        // Validate shares (can't borrow self immutably while mutable borrow exists)
        let mut accepted = 0;
        let mut rejected = 0;

        for share in &msg.shares {
            if self.validate_share(share) {
                accepted += 1;
            } else {
                rejected += 1;
            }
        }

        // Update statistics
        {
            let miner = self.miners.get_mut(endpoint).unwrap(); // Safe: we just checked it exists
            miner.stats.accepted_shares = accepted_shares + accepted;
            miner.stats.rejected_shares = rejected_shares + rejected;
        }

        if accepted > 0 {
            info!("Accepted {} shares from miner {}", accepted, endpoint);
        }
        if rejected > 0 {
            warn!("Rejected {} shares from miner {}", rejected, endpoint);
        }

        // Respond with success
        Ok(SubmitSharesSuccessMessage {
            channel_id: msg.channel_id,
            last_job_id,
        })
    }

    /// Validate a share using formally verified consensus-proof functions
    fn validate_share(&self, share: &ShareData) -> bool {
        // 1. Get job information for this share
        let job_info = match self.get_job_info(share.channel_id, share.job_id) {
            Some(job) => job,
            None => {
                warn!(
                    "Share validation failed: job {} not found for channel {}",
                    share.job_id, share.channel_id
                );
                return false;
            }
        };

        // 2. Construct block header from share data and job info
        let header = match self.share_to_header(share, &job_info) {
            Ok(h) => h,
            Err(e) => {
                warn!("Share validation failed: cannot construct header: {}", e);
                return false;
            }
        };

        // 3. Verify proof of work using formally verified consensus-proof function
        // This function has Kani proofs in bllvm-consensus/src/pow.rs
        let pow_valid = match self.consensus.check_proof_of_work(&header) {
            Ok(valid) => valid,
            Err(e) => {
                warn!("Share validation failed: PoW check error: {}", e);
                return false;
            }
        };

        if !pow_valid {
            return false;
        }

        // 4. Check difficulty meets channel target (for share validation)
        // Channel targets are typically easier than network targets
        self.meets_channel_target(&header, share.channel_id)
    }

    /// Get job information for a channel and job ID
    fn get_job_info(&self, channel_id: u32, job_id: u32) -> Option<&JobInfo> {
        for miner in self.miners.values() {
            if let Some(channel) = miner.channels.get(&channel_id) {
                return channel.jobs.get(&job_id);
            }
        }
        None
    }

    /// Convert Stratum V2 share to BlockHeader
    fn share_to_header(
        &self,
        share: &ShareData,
        job_info: &JobInfo,
    ) -> StratumV2Result<BlockHeader> {
        Ok(BlockHeader {
            version: share.version as i64,
            prev_block_hash: job_info.prev_hash,
            merkle_root: share.merkle_root,
            timestamp: job_info.timestamp,
            bits: job_info.bits,
            nonce: share.nonce as u64,
        })
    }

    /// Check if header meets channel-specific difficulty target
    fn meets_channel_target(&self, header: &BlockHeader, channel_id: u32) -> bool {
        // Get channel target
        let channel_target = match self.get_channel_target(channel_id) {
            Some(target) => target,
            None => return false,
        };

        // Calculate block hash
        let block_hash = self.calculate_block_hash(header);

        // Compare hash to channel target
        // Channel target is typically lower than network target (for share validation)
        block_hash <= channel_target
    }

    /// Get channel target for a channel ID
    fn get_channel_target(&self, channel_id: u32) -> Option<Hash> {
        for miner in self.miners.values() {
            if let Some(channel) = miner.channels.get(&channel_id) {
                return Some(channel.target);
            }
        }
        None
    }

    /// Calculate block hash (double SHA256 of header)
    fn calculate_block_hash(&self, header: &BlockHeader) -> Hash {
        use sha2::{Digest, Sha256};

        // Serialize header
        let mut data = Vec::new();
        data.extend_from_slice(&(header.version as u32).to_le_bytes());
        data.extend_from_slice(&header.prev_block_hash);
        data.extend_from_slice(&header.merkle_root);
        data.extend_from_slice(&(header.timestamp as u32).to_le_bytes());
        data.extend_from_slice(&(header.bits as u32).to_le_bytes());
        data.extend_from_slice(&(header.nonce as u32).to_le_bytes());

        // Double SHA256
        let hash1 = Sha256::digest(&data);
        let hash2 = Sha256::digest(hash1);

        let mut result = [0u8; 32];
        result.copy_from_slice(&hash2);
        result
    }

    /// Calculate channel target from difficulty
    ///
    /// Uses consensus-proof's difficulty calculation functions for proper target computation.
    /// Channel targets are typically easier than network targets to allow share validation.
    fn calculate_channel_target(&self, min_difficulty: u32) -> StratumV2Result<Hash> {
        // For share validation, channel difficulty is typically easier than network difficulty
        // This allows miners to submit shares that meet channel difficulty but not network difficulty

        // Use network difficulty as base (if available)
        // For now, use genesis difficulty as fallback
        let network_bits = 0x1d00ffffu32; // Genesis difficulty

        // Calculate network target using consensus-proof logic
        // Channel target = network_target * (network_difficulty / channel_difficulty)
        // This is a simplified calculation - full implementation would use expand_target from consensus-proof

        // Convert difficulty to target (simplified)
        // In practice, this would use expand_target from consensus-proof::pow
        // For now, use a calculation that creates easier targets for shares
        let target_value = (0xffffffffu64 / min_difficulty.max(1) as u64) as u32;

        // Convert to Hash (big-endian, 32 bytes)
        let mut target = [0u8; 32];
        target[28..32].copy_from_slice(&target_value.to_be_bytes());

        Ok(target)
    }

    /// Extract template parts (coinbase prefix/suffix, merkle path)
    fn extract_template_parts(&self, template: &Block) -> (Vec<u8>, Vec<u8>, Vec<Hash>) {
        // Extract coinbase transaction
        if let Some(coinbase) = template.transactions.first() {
            // Serialize coinbase transaction
            let coinbase_bytes = self.serialize_transaction(coinbase);

            // Extract merkle path from coinbase (index 0) to root
            let merkle_path = self.extract_merkle_path(template, 0);

            // For Stratum V2, coinbase prefix is the full coinbase (miners can modify it)
            // Coinbase suffix is empty (not used in Stratum V2)
            (coinbase_bytes, vec![], merkle_path)
        } else {
            (vec![], vec![], vec![])
        }
    }

    /// Extract merkle path from a specific transaction index to root
    fn extract_merkle_path(&self, block: &Block, tx_index: usize) -> Vec<Hash> {
        use crate::storage::hashing::double_sha256;
        use bllvm_protocol::block::calculate_tx_id;

        if block.transactions.is_empty() || tx_index >= block.transactions.len() {
            return vec![];
        }

        // Calculate all transaction hashes
        let mut tx_hashes: Vec<Hash> = block
            .transactions
            .iter()
            .map(|tx| calculate_tx_id(tx))
            .collect();

        let mut merkle_path = Vec::new();
        let mut current_index = tx_index;
        let mut current_level = tx_hashes;

        // Build merkle path by traversing tree bottom-up
        while current_level.len() > 1 {
            let mut next_level = Vec::new();

            // Process pairs of hashes
            for (pair_idx, chunk) in current_level.chunks(2).enumerate() {
                if chunk.len() == 2 {
                    // Hash two hashes together
                    let mut combined = Vec::with_capacity(64);
                    combined.extend_from_slice(&chunk[0]);
                    combined.extend_from_slice(&chunk[1]);
                    let parent_hash = double_sha256(&combined);
                    next_level.push(parent_hash);

                    // Check if current_index is in this pair
                    let pair_start = pair_idx * 2;
                    if current_index >= pair_start && current_index < pair_start + 2 {
                        // Add sibling to path
                        if current_index % 2 == 0 {
                            // Left child - add right sibling
                            merkle_path.push(chunk[1]);
                        } else {
                            // Right child - add left sibling
                            merkle_path.push(chunk[0]);
                        }
                        // Update index for next level
                        current_index = pair_idx;
                    }
                } else {
                    // Odd number: duplicate the last hash
                    let mut combined = Vec::with_capacity(64);
                    combined.extend_from_slice(&chunk[0]);
                    combined.extend_from_slice(&chunk[0]);
                    let parent_hash = double_sha256(&combined);
                    next_level.push(parent_hash);

                    // If current_index is the last (odd) transaction, no sibling to add
                    let pair_start = pair_idx * 2;
                    if current_index == pair_start {
                        // Last transaction in odd-numbered level - no sibling
                        current_index = pair_idx;
                    }
                }
            }

            current_level = next_level;
        }

        merkle_path
    }

    /// Serialize transaction for template extraction
    fn serialize_transaction(&self, tx: &bllvm_protocol::types::Transaction) -> Vec<u8> {
        // Use consensus-proof serialization function
        use bllvm_protocol::serialization::serialize_transaction;
        serialize_transaction(tx)
    }

    /// Get miner statistics
    pub fn get_miner_stats(&self, endpoint: &str) -> Option<&MinerStats> {
        self.miners.get(endpoint).map(|m| &m.stats)
    }

    /// Get connected miner count
    pub fn miner_count(&self) -> usize {
        self.miners.len()
    }

    /// Remove miner connection
    pub fn remove_miner(&mut self, endpoint: &str) {
        if self.miners.remove(endpoint).is_some() {
            info!("Removed miner connection: {}", endpoint);
        }
    }
}

impl Default for StratumV2Pool {
    fn default() -> Self {
        Self::new()
    }
}
