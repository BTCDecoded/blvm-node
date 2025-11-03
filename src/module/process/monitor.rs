//! Module process health monitoring
//! 
//! Monitors module processes for crashes and health issues.

use tokio::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::module::process::spawner::ModuleProcess;
use crate::module::traits::{ModuleError, ModuleState};

/// Module health monitor
pub struct ModuleProcessMonitor {
    /// Monitoring interval
    interval: Duration,
    /// Crash notification channel
    crash_tx: mpsc::UnboundedSender<(String, ModuleError)>,
}

/// Module health status
#[derive(Debug, Clone)]
pub enum ModuleHealth {
    /// Module is healthy
    Healthy,
    /// Module is unresponsive
    Unresponsive,
    /// Module has crashed
    Crashed(String),
}

impl ModuleProcessMonitor {
    /// Create a new module process monitor
    pub fn new(crash_tx: mpsc::UnboundedSender<(String, ModuleError)>) -> Self {
        Self {
            interval: Duration::from_secs(5),
            crash_tx,
        }
    }
    
    /// Set monitoring interval
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }
    
    /// Start monitoring a module
    pub async fn monitor_module(
        &self,
        module_name: String,
        mut process: ModuleProcess,
    ) -> Result<(), ModuleError> {
        info!("Starting health monitoring for module: {}", module_name);
        
        let mut ticker = tokio::time::interval(self.interval);
        
        loop {
            ticker.tick().await;
            
            // Check if process is still running
            if !process.is_running() {
                // Process exited, check exit status
                match process.wait().await? {
                    Some(status) => {
                        if status.success() {
                            info!("Module {} exited normally", module_name);
                        } else {
                            let error_msg = format!(
                                "Module {} exited with error: {:?}",
                                module_name, status.code()
                            );
                            error!("{}", error_msg);
                            
                            let _ = self.crash_tx.send((
                                module_name.clone(),
                                ModuleError::ModuleCrashed(error_msg),
                            ));
                        }
                        return Ok(());
                    }
                    None => {
                        warn!("Module {} process status unknown", module_name);
                        return Ok(());
                    }
                }
            }
            
            // TODO: Add heartbeat check via IPC
            // For now, just checking if process is alive
        }
        
        Ok(())
    }
    
    /// Check module health
    pub fn check_health(process: &mut ModuleProcess) -> ModuleHealth {
        if !process.is_running() {
            ModuleHealth::Crashed("Process exited".to_string())
        } else {
            // TODO: Add IPC heartbeat check
            ModuleHealth::Healthy
        }
    }
}
