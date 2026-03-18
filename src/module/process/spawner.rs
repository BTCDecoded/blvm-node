//! Module process spawning and management
//!
//! Handles spawning module processes as separate executables with process isolation.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::{Child, Command};
use tokio::time::{timeout, Duration};
use tracing::{debug, info, warn};

use crate::utils::retry::{retry_async_with_backoff, RetryConfig};

use crate::module::ipc::client::ModuleIpcClient;
#[cfg(unix)]
use crate::module::ipc::protocol::{MessageType, RequestMessage, RequestPayload};
use crate::module::sandbox::{FileSystemSandbox, NetworkSandbox, ProcessSandbox, SandboxConfig};
use crate::module::traits::{ModuleContext, ModuleError};

/// Spawn and manage module processes
pub struct ModuleProcessSpawner {
    /// Base directory for module binaries
    pub modules_dir: PathBuf,
    /// Base directory for module data
    pub data_dir: PathBuf,
    /// IPC socket directory
    pub socket_dir: PathBuf,
    /// Process sandbox for resource limits
    process_sandbox: Option<ProcessSandbox>,
    /// File system sandbox for access control
    filesystem_sandbox: Option<FileSystemSandbox>,
    /// Network sandbox for network isolation
    network_sandbox: NetworkSandbox,
    /// Module resource limits configuration
    resource_limits_config: Option<crate::config::ModuleResourceLimitsConfig>,
}

impl ModuleProcessSpawner {
    /// Create a new module process spawner
    pub fn new<P: AsRef<Path>>(modules_dir: P, data_dir: P, socket_dir: P) -> Self {
        Self::with_config(modules_dir, data_dir, socket_dir, None)
    }

    /// Create a new module process spawner with resource limits configuration
    pub fn with_config<P: AsRef<Path>>(
        modules_dir: P,
        data_dir: P,
        socket_dir: P,
        resource_limits_config: Option<&crate::config::ModuleResourceLimitsConfig>,
    ) -> Self {
        let data_dir_path = data_dir.as_ref().to_path_buf();

        // Get resource limits from config or use defaults
        let limits_config = resource_limits_config
            .cloned()
            .unwrap_or_else(crate::config::ModuleResourceLimitsConfig::default);

        // Initialize sandboxes with config
        let sandbox_config = SandboxConfig::with_resource_limits(&data_dir_path, &limits_config);
        let process_sandbox = Some(ProcessSandbox::new(sandbox_config));
        let filesystem_sandbox = Some(FileSystemSandbox::new(&data_dir_path));
        let network_sandbox = NetworkSandbox::new(); // No network access by default

        Self {
            modules_dir: modules_dir.as_ref().to_path_buf(),
            data_dir: data_dir_path,
            socket_dir: socket_dir.as_ref().to_path_buf(),
            process_sandbox,
            filesystem_sandbox,
            network_sandbox,
            resource_limits_config: Some(limits_config),
        }
    }

    /// Spawn a module process
    pub async fn spawn(
        &self,
        module_name: &str,
        binary_path: &Path,
        context: ModuleContext,
    ) -> Result<ModuleProcess, ModuleError> {
        info!("Spawning module process: {}", module_name);

        // Verify binary exists
        if !binary_path.exists() {
            return Err(ModuleError::ModuleNotFound(format!(
                "Module binary not found: {binary_path:?}"
            )));
        }

        // Create module data directory
        let module_data_dir = self.data_dir.join(module_name);
        std::fs::create_dir_all(&module_data_dir).map_err(|e| {
            ModuleError::InitializationError(format!("Failed to create module data directory: {e}"))
        })?;

        // Validate data directory is within sandbox
        if let Some(ref fs_sandbox) = self.filesystem_sandbox {
            fs_sandbox.validate_path(&module_data_dir)?;
        }

        // Use node's IPC socket path from context (modules connect to node; node listens on one socket)
        let socket_path = PathBuf::from(&context.socket_path);

        // Spawn the process
        let mut command = Command::new(binary_path);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("MODULE_NAME", module_name)
            .env("MODULE_ID", &context.module_id)
            .env("SOCKET_PATH", &socket_path)
            .env("DATA_DIR", &module_data_dir);

        // Add module config as environment variables
        for (key, value) in &context.config {
            command.env(
                format!("MODULE_CONFIG_{}", key.to_uppercase().as_str()),
                value,
            );
        }

        debug!(
            "Spawning process: {:?} with args: {:?}",
            binary_path, command
        );

        let child = command.spawn().map_err(|e| {
            ModuleError::InitializationError(format!("Failed to spawn module process: {e}"))
        })?;

        // Apply resource limits if sandbox is configured
        if let Some(ref sandbox) = self.process_sandbox {
            let pid = child.id();
            if let Err(e) = sandbox.apply_limits(pid) {
                warn!(
                    "Failed to apply resource limits to module {}: {}",
                    module_name, e
                );
                // Continue anyway - limits will be enforced in Phase 2+
            }
        }

        // Wait a moment for process to start (from config)
        let startup_wait = self
            .resource_limits_config
            .as_ref()
            .map(|c| c.module_startup_wait_millis)
            .unwrap_or(100);
        tokio::time::sleep(Duration::from_millis(startup_wait)).await;

        // Wait for socket to be created (with timeout from config)
        let socket_timeout = self
            .resource_limits_config
            .as_ref()
            .map(|c| c.module_socket_timeout_seconds)
            .unwrap_or(5);
        let socket_ready = timeout(
            Duration::from_secs(socket_timeout),
            self.wait_for_socket(&socket_path),
        )
        .await;
        match socket_ready {
            Ok(Ok(_)) => {
                info!("Module {} socket ready", module_name);
            }
            Ok(Err(e)) => {
                return Err(ModuleError::InitializationError(format!(
                    "Failed to wait for module socket: {e}"
                )));
            }
            Err(_) => {
                return Err(ModuleError::Timeout);
            }
        }

        // Connect to the node IPC (modules connect to node; spawner connects for heartbeat)
        let mut ipc_client = ModuleIpcClient::connect(&socket_path).await.map_err(|e| {
            ModuleError::IpcError(format!("Failed to connect to node IPC: {e}"))
        })?;

        // Send handshake so node registers this connection (for heartbeat)
        let version = context
            .config
            .get("version")
            .cloned()
            .unwrap_or_else(|| "0.1.0".to_string());
        let handshake = RequestMessage {
            correlation_id: 1,
            request_type: MessageType::Handshake,
            payload: RequestPayload::Handshake {
                module_id: context.module_id.clone(),
                module_name: module_name.to_string(),
                version,
            },
        };
        let response = ipc_client.request(handshake).await.map_err(|e| {
            ModuleError::IpcError(format!("Failed to send handshake: {e}"))
        })?;
        if !response.success {
            return Err(ModuleError::IpcError(
                response
                    .error
                    .unwrap_or_else(|| "Handshake failed".to_string()),
            ));
        }

        let client = Some(ipc_client);

        Ok(ModuleProcess {
            module_name: module_name.to_string(),
            process: child,
            socket_path,
            client,
        })
    }

    /// Wait for socket/pipe to be ready (Unix: file exists, Windows: connect succeeds)
    async fn wait_for_socket(&self, socket_path: &Path) -> Result<(), ModuleError> {
        let check_interval = self
            .resource_limits_config
            .as_ref()
            .map(|c| c.module_socket_check_interval_millis)
            .unwrap_or(100);
        let max_attempts = self
            .resource_limits_config
            .as_ref()
            .map(|c| c.module_socket_max_attempts)
            .unwrap_or(50);

        let path = socket_path.to_path_buf();
        let config = RetryConfig::new(
            max_attempts.try_into().unwrap_or(u32::MAX),
            Duration::from_millis(check_interval),
        );

        retry_async_with_backoff(&config, || {
            let p = path.clone();
            async move {
                #[cfg(unix)]
                if p.exists() {
                    return Ok(());
                }
                #[cfg(windows)]
                if ModuleIpcClient::connect(&p).await.is_ok() {
                    return Ok(());
                }
                Err(SocketNotReady)
            }
        })
        .await
        .map_err(|_| {
            ModuleError::InitializationError(
                "Module socket/pipe did not appear within timeout".to_string(),
            )
        })
    }
}

/// Used with retry_async_with_backoff when socket is not yet ready (Display required).
struct SocketNotReady;
impl std::fmt::Display for SocketNotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "socket not ready")
    }
}

/// Running module process
pub struct ModuleProcess {
    /// Module name
    pub module_name: String,
    /// Child process handle
    pub process: Child,
    /// IPC socket path
    pub socket_path: PathBuf,
    /// IPC client connection (optional, may be dropped for cleanup)
    client: Option<ModuleIpcClient>,
}

impl ModuleProcess {
    /// Get the process ID
    pub fn id(&self) -> Option<u32> {
        self.process.id()
    }

    /// Check if process is still running
    pub fn is_running(&mut self) -> bool {
        !matches!(self.process.try_wait(), Ok(Some(_)))
    }

    /// Wait for process to exit
    pub async fn wait(&mut self) -> Result<Option<std::process::ExitStatus>, ModuleError> {
        self.process
            .wait()
            .await
            .map_err(|e| ModuleError::op_err("Failed to wait for process", e))
            .map(Some)
    }

    /// Kill the process
    pub async fn kill(&mut self) -> Result<(), ModuleError> {
        debug!("Killing module process: {}", self.module_name);

        if let Err(e) = self.process.kill().await {
            warn!("Failed to kill module process {}: {}", self.module_name, e);
        }

        // Wait for process to exit
        let _ = self.process.wait().await;

        // Clean up socket file if it exists (Unix; Windows named pipes don't create files)
        if self.socket_path.exists() {
            if let Err(e) = std::fs::remove_file(&self.socket_path) {
                warn!("Failed to remove socket file {:?}: {}", self.socket_path, e);
            }
        }

        Ok(())
    }

    /// Get IPC client (mutable)
    pub fn client_mut(&mut self) -> Option<&mut ModuleIpcClient> {
        self.client.as_mut()
    }

    /// Take IPC client (for cleanup)
    pub fn take_client(&mut self) -> Option<ModuleIpcClient> {
        self.client.take()
    }
}
