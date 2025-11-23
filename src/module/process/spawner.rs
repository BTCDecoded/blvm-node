//! Module process spawning and management
//!
//! Handles spawning module processes as separate executables with process isolation.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::{Child, Command};
use tokio::time::{timeout, Duration};
use tracing::{debug, info, warn};

#[cfg(unix)]
use crate::module::ipc::client::ModuleIpcClient;
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
                "Module binary not found: {:?}",
                binary_path
            )));
        }

        // Create module data directory
        let module_data_dir = self.data_dir.join(module_name);
        std::fs::create_dir_all(&module_data_dir).map_err(|e| {
            ModuleError::InitializationError(format!(
                "Failed to create module data directory: {}",
                e
            ))
        })?;

        // Validate data directory is within sandbox
        if let Some(ref fs_sandbox) = self.filesystem_sandbox {
            fs_sandbox.validate_path(&module_data_dir)?;
        }

        // Create IPC socket path
        let socket_path = self.socket_dir.join(format!("{}.sock", module_name));

        // Spawn the process
        let mut command = Command::new(binary_path);
        command
            .arg("--module-id")
            .arg(&context.module_id)
            .arg("--socket-path")
            .arg(&socket_path)
            .arg("--data-dir")
            .arg(&module_data_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("MODULE_NAME", module_name);

        // Add module config as environment variables
        for (key, value) in &context.config {
            command.env(format!("MODULE_CONFIG_{}", key.to_uppercase()), value);
        }

        debug!(
            "Spawning process: {:?} with args: {:?}",
            binary_path, command
        );

        let child = command.spawn().map_err(|e| {
            ModuleError::InitializationError(format!("Failed to spawn module process: {}", e))
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
                    "Failed to wait for module socket: {}",
                    e
                )));
            }
            Err(_) => {
                return Err(ModuleError::Timeout);
            }
        }

        // Connect to the module IPC (Unix only)
        #[cfg(unix)]
        {
            let client = Some(ModuleIpcClient::connect(&socket_path).await.map_err(|e| {
                ModuleError::IpcError(format!("Failed to connect to module IPC: {}", e))
            })?);

            Ok(ModuleProcess {
                module_name: module_name.to_string(),
                process: child,
                socket_path,
                client,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(ModuleProcess {
                module_name: module_name.to_string(),
                process: child,
                socket_path,
            })
        }
    }

    /// Wait for socket file to be created
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

        let mut attempts = 0;
        while attempts < max_attempts {
            if socket_path.exists() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(check_interval)).await;
            attempts += 1;
        }

        Err(ModuleError::InitializationError(
            "Module socket did not appear within timeout".to_string(),
        ))
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
    /// IPC client connection (optional, may be dropped for cleanup, Unix only)
    #[cfg(unix)]
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
            .map_err(|e| ModuleError::OperationError(format!("Failed to wait for process: {}", e)))
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

        // Clean up socket file
        if self.socket_path.exists() {
            if let Err(e) = std::fs::remove_file(&self.socket_path) {
                warn!("Failed to remove socket file {:?}: {}", self.socket_path, e);
            }
        }

        Ok(())
    }

    /// Get IPC client (mutable, Unix only)
    #[cfg(unix)]
    pub fn client_mut(&mut self) -> Option<&mut ModuleIpcClient> {
        self.client.as_mut()
    }

    /// Take IPC client (for cleanup, Unix only)
    #[cfg(unix)]
    pub fn take_client(&mut self) -> Option<ModuleIpcClient> {
        self.client.take()
    }
}
