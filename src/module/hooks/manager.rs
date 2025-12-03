//! Hook manager for calling module hooks with timeouts

use crate::module::traits::{MempoolSize, ModuleHooks};
use std::sync::Arc;
use tokio::time::{timeout, Duration};

/// Hook manager that calls module hooks with timeouts
pub struct HookManager {
    /// Registered hooks (module_id -> hooks)
    hooks: Vec<(String, Arc<dyn ModuleHooks>)>,
}

impl HookManager {
    /// Create a new hook manager
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    /// Register hooks for a module
    pub fn register_hooks(&mut self, module_id: String, hooks: Arc<dyn ModuleHooks>) {
        self.hooks.push((module_id, hooks));
    }

    /// Unregister hooks for a module
    pub fn unregister_hooks(&mut self, module_id: &str) {
        self.hooks.retain(|(id, _)| id != module_id);
    }

    /// Get cached fee estimate from modules
    /// Returns the first non-None value, or None if no module provides a cache
    pub async fn get_fee_estimate_cached(&self, target_blocks: u32) -> Option<u64> {
        for (module_id, hooks) in &self.hooks {
            match timeout(
                Duration::from_millis(100),
                hooks.get_fee_estimate_cached(target_blocks),
            )
            .await
            {
                Ok(Ok(Some(estimate))) => {
                    tracing::debug!(
                        "Module {} provided cached fee estimate: {}",
                        module_id,
                        estimate
                    );
                    return Some(estimate);
                }
                Ok(Ok(None)) => {
                    // Module doesn't have cached value, try next
                    continue;
                }
                Ok(Err(e)) => {
                    tracing::debug!("Module {} hook error: {}", module_id, e);
                    continue;
                }
                Err(_) => {
                    // Timeout - module unresponsive, try next
                    tracing::debug!("Module {} hook timeout", module_id);
                    continue;
                }
            }
        }
        None
    }

    /// Get cached mempool stats from modules
    /// Returns the first non-None value, or None if no module provides a cache
    pub async fn get_mempool_stats_cached(&self) -> Option<MempoolSize> {
        for (module_id, hooks) in &self.hooks {
            match timeout(Duration::from_millis(50), hooks.get_mempool_stats_cached()).await {
                Ok(Ok(Some(stats))) => {
                    tracing::debug!("Module {} provided cached mempool stats", module_id);
                    return Some(stats);
                }
                Ok(Ok(None)) => {
                    // Module doesn't have cached value, try next
                    continue;
                }
                Ok(Err(e)) => {
                    tracing::debug!("Module {} hook error: {}", module_id, e);
                    continue;
                }
                Err(_) => {
                    // Timeout - module unresponsive, try next
                    tracing::debug!("Module {} hook timeout", module_id);
                    continue;
                }
            }
        }
        None
    }
}

impl Default for HookManager {
    fn default() -> Self {
        Self::new()
    }
}
