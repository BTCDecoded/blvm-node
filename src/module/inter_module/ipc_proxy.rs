//! IPC proxy for subprocess module APIs.
//!
//! Spawned modules cannot pass `Arc<dyn ModuleAPI>` to the node. They register a
//! method descriptor over IPC; the node installs this proxy which forwards
//! `handle_request` to the module subprocess via [`ModuleIpcHandle::invoke_module_api`].
//!
//! Uses [`ModuleIpcHandle`] (a lock-free struct of individually `Arc`-wrapped state)
//! rather than `Arc<Mutex<ModuleIpcServer>>` to avoid deadlocking with the server's
//! own message loop, which holds the Mutex for its entire lifetime.

use async_trait::async_trait;

use crate::module::inter_module::api::ModuleAPI;
use crate::module::ipc::server::ModuleIpcHandle;
use crate::module::traits::ModuleError;

/// Forwards module API calls to a connected subprocess over IPC.
pub struct IpcForwardingModuleAPI {
    module_id: String,
    ipc_handle: ModuleIpcHandle,
    methods: Vec<String>,
    api_version: u32,
}

impl IpcForwardingModuleAPI {
    pub fn new(
        module_id: String,
        ipc_handle: ModuleIpcHandle,
        methods: Vec<String>,
        api_version: u32,
    ) -> Self {
        Self {
            module_id,
            ipc_handle,
            methods,
            api_version,
        }
    }
}

#[async_trait]
impl ModuleAPI for IpcForwardingModuleAPI {
    async fn handle_request(
        &self,
        method: &str,
        params: &[u8],
        caller_module_id: &str,
    ) -> Result<Vec<u8>, ModuleError> {
        if !self.methods.iter().any(|m| m == method) {
            return Err(ModuleError::OperationError(format!(
                "Unknown method '{method}' for module '{}'",
                self.module_id
            )));
        }

        self.ipc_handle
            .invoke_module_api(&self.module_id, method, params, caller_module_id)
            .await
    }

    fn list_methods(&self) -> Vec<String> {
        self.methods.clone()
    }

    fn api_version(&self) -> u32 {
        self.api_version
    }
}
