//! Module process health monitoring
//!
//! Monitors module processes for crashes and health issues.

use tokio::sync::mpsc;
use tokio::time::Duration;
use tracing::{debug, error, info, warn};

use crate::module::process::spawner::ModuleProcess;
use crate::module::traits::{ModuleError, ModuleState};
use std::sync::{Arc, Mutex};

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
                                module_name,
                                status.code()
                            );
                            error!("{}", error_msg);

                            let _ = self
                                .crash_tx
                                .send((module_name.clone(), ModuleError::ModuleCrashed(error_msg)));
                        }
                        return Ok(());
                    }
                    None => {
                        warn!("Module {} process status unknown", module_name);
                        return Ok(());
                    }
                }
            }

            // Check heartbeat via IPC
            if let Some(client) = process.client_mut() {
                use crate::module::ipc::protocol::MessageType;
                use crate::module::ipc::protocol::RequestMessage;
                use crate::module::ipc::protocol::RequestPayload;

                // Use GetChainTip as a lightweight heartbeat check
                let heartbeat_request = RequestMessage {
                    correlation_id: 0, // Use 0 for heartbeat (won't match any real request)
                    request_type: MessageType::GetChainTip,
                    payload: RequestPayload::GetChainTip,
                };

                // Send heartbeat with timeout
                let heartbeat_result =
                    tokio::time::timeout(Duration::from_secs(2), client.request(heartbeat_request))
                        .await;

                match heartbeat_result {
                    Ok(Ok(_)) => {
                        // Heartbeat successful - module is responsive
                        debug!("Module {} heartbeat OK", module_name);
                    }
                    Ok(Err(e)) => {
                        warn!("Module {} heartbeat failed: {}", module_name, e);
                        // Mark as unresponsive but don't crash yet
                    }
                    Err(_) => {
                        warn!("Module {} heartbeat timeout", module_name);
                        // Timeout - module may be unresponsive
                    }
                }
            } else {
                // No IPC client available - can't check heartbeat
                debug!(
                    "Module {} has no IPC client for heartbeat check",
                    module_name
                );
            }

            // Loop continues to next iteration
        }
    }

    /// Start monitoring a module (with shared process via Arc<Mutex<>>)
    pub async fn monitor_module_shared(
        &self,
        module_name: String,
        shared_process: Arc<tokio::sync::Mutex<ModuleProcess>>,
    ) -> Result<(), ModuleError> {
        info!("Starting health monitoring for module: {}", module_name);

        let mut ticker = tokio::time::interval(self.interval);

        loop {
            ticker.tick().await;

            // Check if process is still running
            let is_running = {
                let mut process_guard = shared_process.lock().await;
                process_guard.is_running()
            };

            if !is_running {
                // Process exited, check exit status
                let exit_status = {
                    let mut process_guard = shared_process.lock().await;
                    process_guard.wait().await?
                };

                match exit_status {
                    Some(status) => {
                        if status.success() {
                            info!("Module {} exited normally", module_name);
                        } else {
                            let error_msg = format!(
                                "Module {} exited with error: {:?}",
                                module_name,
                                status.code()
                            );
                            error!("{}", error_msg);

                            let _ = self
                                .crash_tx
                                .send((module_name.clone(), ModuleError::ModuleCrashed(error_msg)));
                        }
                        return Ok(());
                    }
                    None => {
                        warn!("Module {} process status unknown", module_name);
                        return Ok(());
                    }
                }
            }

            // Check heartbeat via IPC
            {
                let mut process_guard = shared_process.lock().await;
                if let Some(client) = process_guard.client_mut() {
                    use crate::module::ipc::protocol::MessageType;
                    use crate::module::ipc::protocol::RequestMessage;
                    use crate::module::ipc::protocol::RequestPayload;

                    // Use GetChainTip as a lightweight heartbeat check
                    let heartbeat_request = RequestMessage {
                        correlation_id: 0, // Use 0 for heartbeat (won't match any real request)
                        request_type: MessageType::GetChainTip,
                        payload: RequestPayload::GetChainTip,
                    };

                    // Send heartbeat with timeout
                    let heartbeat_result = tokio::time::timeout(
                        Duration::from_secs(2),
                        client.request(heartbeat_request),
                    )
                    .await;

                    match heartbeat_result {
                        Ok(Ok(_)) => {
                            // Heartbeat successful - module is responsive
                            debug!("Module {} heartbeat OK", module_name);
                        }
                        Ok(Err(e)) => {
                            warn!("Module {} heartbeat failed: {}", module_name, e);
                            // Mark as unresponsive but don't crash yet
                        }
                        Err(_) => {
                            warn!("Module {} heartbeat timeout", module_name);
                            // Timeout - module may be unresponsive
                        }
                    }
                } else {
                    // No IPC client available - can't check heartbeat
                    debug!(
                        "Module {} has no IPC client for heartbeat check",
                        module_name
                    );
                }
            }

            // Loop continues to next iteration
        }
    }

    /// Check module health
    pub fn check_health(process: &mut ModuleProcess) -> ModuleHealth {
        if !process.is_running() {
            ModuleHealth::Crashed("Process exited".to_string())
        } else if let Some(client) = process.client_mut() {
            // Check heartbeat via IPC with short timeout
            use crate::module::ipc::protocol::{MessageType, RequestMessage, RequestPayload};
            use tokio::time::timeout;

            let heartbeat_request = RequestMessage {
                correlation_id: 0,
                request_type: MessageType::GetChainTip,
                payload: RequestPayload::GetChainTip,
            };

            // Use tokio::runtime::Handle to run async code in sync context
            // This is a simplified check - in production would use proper async context
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    let result = handle.block_on(timeout(
                        Duration::from_secs(1),
                        client.request(heartbeat_request),
                    ));

                    match result {
                        Ok(Ok(_)) => ModuleHealth::Healthy,
                        _ => ModuleHealth::Unresponsive,
                    }
                }
                Err(_) => {
                    // No async runtime - can't check heartbeat
                    ModuleHealth::Healthy // Assume healthy if we can't check
                }
            }
        } else {
            // No IPC client - can only check if process is running
            ModuleHealth::Healthy
        }
    }
}
