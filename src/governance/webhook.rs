//! Governance webhook client for bllvm-node
//!
//! Sends block notifications to bllvm-commons for fee forwarding tracking

use anyhow::Result;
use bllvm_protocol::Block;
use serde_json::json;
use tracing::{debug, info, warn};

#[cfg(feature = "governance")]
use reqwest::Client;

/// Governance webhook client
#[cfg(feature = "governance")]
pub struct GovernanceWebhookClient {
    client: Client,
    webhook_url: String,
    node_id: Option<String>,
    enabled: bool,
}

#[cfg(feature = "governance")]
impl GovernanceWebhookClient {
    /// Create a new governance webhook client
    pub fn new(webhook_url: Option<String>, node_id: Option<String>) -> Self {
        let enabled = webhook_url.is_some();
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| Client::new());

        let url = webhook_url.unwrap_or_default();

        if enabled {
            info!("Governance webhook client initialized: {}", url);
        } else {
            debug!("Governance webhook client disabled (no URL configured)");
        }

        Self {
            client,
            webhook_url: url,
            node_id,
            enabled,
        }
    }

    /// Create from environment variables
    pub fn from_env() -> Self {
        let webhook_url = std::env::var("GOVERNANCE_WEBHOOK_URL").ok();
        let node_id = std::env::var("GOVERNANCE_NODE_ID").ok();
        Self::new(webhook_url, node_id)
    }

    /// Notify governance app about a new block
    pub async fn notify_block(&self, block: &Block, height: u64) -> Result<()> {
        if !self.enabled {
            return Ok(()); // Silently skip if disabled
        }

        // Calculate block hash
        let block_hash = self.calculate_block_hash(block);

        // Serialize block to JSON (Block implements Serialize)
        let block_json = serde_json::to_value(block)
            .map_err(|e| anyhow::anyhow!("Failed to serialize block: {}", e))?;

        // Prepare payload
        let payload = json!({
            "block_hash": hex::encode(block_hash),
            "block_height": height as i32,
            "block": block_json,
            "contributor_id": self.node_id.as_deref(),
        });

        // Send webhook (fire and forget - don't block block processing)
        let client = self.client.clone();
        let url = self.webhook_url.clone();
        let block_hash_str = hex::encode(block_hash);
        let height_clone = height;

        tokio::spawn(async move {
            match client.post(&url).json(&payload).send().await {
                Ok(response) => {
                    if response.status().is_success() {
                        debug!(
                            "Governance webhook sent successfully for block {} at height {}",
                            block_hash_str, height_clone
                        );
                    } else {
                        warn!(
                            "Governance webhook returned error status {} for block {} at height {}",
                            response.status(),
                            block_hash_str,
                            height_clone
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to send governance webhook for block {} at height {}: {}",
                        block_hash_str, height_clone, e
                    );
                }
            }
        });

        Ok(())
    }

    /// Calculate block hash (double SHA256 of block header)
    fn calculate_block_hash(&self, block: &Block) -> [u8; 32] {
        use sha2::{Digest, Sha256};

        // Serialize block header
        let mut header_data = Vec::new();
        header_data.extend_from_slice(&(block.header.version as u32).to_le_bytes());
        header_data.extend_from_slice(&block.header.prev_block_hash);
        header_data.extend_from_slice(&block.header.merkle_root);
        header_data.extend_from_slice(&block.header.timestamp.to_le_bytes());
        header_data.extend_from_slice(&block.header.bits.to_le_bytes());
        header_data.extend_from_slice(&block.header.nonce.to_le_bytes());

        // Double SHA256
        let first_hash = Sha256::digest(&header_data);
        let second_hash = Sha256::digest(first_hash);

        let mut hash = [0u8; 32];
        hash.copy_from_slice(&second_hash);
        hash
    }
}

#[cfg(not(feature = "governance"))]
/// Dummy implementation when governance feature is disabled
pub struct GovernanceWebhookClient;

#[cfg(not(feature = "governance"))]
impl GovernanceWebhookClient {
    pub fn new(_webhook_url: Option<String>, _node_id: Option<String>) -> Self {
        Self
    }

    pub fn from_env() -> Self {
        Self
    }

    pub async fn notify_block(&self, _block: &Block, _height: u64) -> Result<()> {
        Ok(())
    }
}
