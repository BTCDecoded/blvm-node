//! Process-level sandboxing and resource limits
//!
//! Implements CPU, memory, and file descriptor limits for module processes.

#[cfg(unix)]
#[cfg(feature = "nix")]
use nix::sys::resource::{setrlimit, Resource};

use std::path::Path;
use tracing::debug;

use crate::module::traits::ModuleError;

/// Resource limits for a module process
#[derive(Debug, Clone)]
pub struct ResourceLimits {
    /// Maximum CPU usage (percentage, 0-100)
    pub max_cpu_percent: Option<u32>,
    /// Maximum memory usage (bytes)
    pub max_memory_bytes: Option<u64>,
    /// Maximum number of file descriptors
    pub max_file_descriptors: Option<u32>,
    /// Maximum number of child processes
    pub max_child_processes: Option<u32>,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_cpu_percent: Some(50),                 // Default: 50% CPU
            max_memory_bytes: Some(512 * 1024 * 1024), // Default: 512 MB
            max_file_descriptors: Some(256),           // Default: 256 FDs
            max_child_processes: Some(10),             // Default: 10 child processes
        }
    }
}

/// Sandbox configuration for a module
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Allowed data directory (modules can only access this)
    pub allowed_data_dir: std::path::PathBuf,
    /// Resource limits
    pub resource_limits: ResourceLimits,
    /// Whether to enable strict sandboxing (OS-level restrictions)
    pub strict_mode: bool,
}

impl SandboxConfig {
    /// Create a new sandbox config with default limits
    pub fn new<P: AsRef<Path>>(data_dir: P) -> Self {
        Self {
            allowed_data_dir: data_dir.as_ref().to_path_buf(),
            resource_limits: ResourceLimits::default(),
            strict_mode: false, // Phase 1: disabled, Phase 2+: enabled
        }
    }

    /// Create strict sandbox config (for Phase 2+)
    pub fn strict<P: AsRef<Path>>(data_dir: P) -> Self {
        Self {
            allowed_data_dir: data_dir.as_ref().to_path_buf(),
            resource_limits: ResourceLimits::default(),
            strict_mode: true,
        }
    }
}

/// Process sandbox manager
pub struct ProcessSandbox {
    config: SandboxConfig,
}

impl ProcessSandbox {
    /// Create a new process sandbox
    pub fn new(config: SandboxConfig) -> Self {
        Self { config }
    }

    /// Apply resource limits to a process
    ///
    /// On Unix systems, uses `setrlimit` via the `nix` crate.
    /// On Windows, uses job objects (not yet implemented).
    pub fn apply_limits(&self, pid: Option<u32>) -> Result<(), ModuleError> {
        let limits = &self.config.resource_limits;

        #[cfg(unix)]
        {
            if let Some(pid) = pid {
                // Note: setrlimit applies to the current process, not another process
                // For applying limits to another process, we would need to:
                // 1. Set limits before spawning the child process, or
                // 2. Use prlimit (Linux-specific) to set limits on another process
                // For now, we apply limits to the current process context
                // In practice, limits should be set before spawning module processes

                // Apply memory limit (RLIMIT_AS = address space limit)
                #[cfg(feature = "nix")]
                if let Some(max_memory) = limits.max_memory_bytes {
                    let soft_limit = max_memory as u64;
                    let hard_limit = max_memory as u64;
                    setrlimit(Resource::RLIMIT_AS, soft_limit, hard_limit).map_err(|e| {
                        ModuleError::OperationError(format!("Failed to set memory limit: {}", e))
                    })?;
                    debug!("Set memory limit: {} bytes", max_memory);
                }

                // Apply file descriptor limit
                if let Some(max_fds) = limits.max_file_descriptors {
                    let soft_limit = max_fds as u64;
                    let hard_limit = max_fds as u64;
                    #[cfg(feature = "nix")]
                    {
                        use nix::sys::resource::{setrlimit, Resource};
                        setrlimit(Resource::RLIMIT_NOFILE, soft_limit, hard_limit).map_err(
                            |e| {
                                ModuleError::OperationError(format!(
                                    "Failed to set file descriptor limit: {}",
                                    e
                                ))
                            },
                        )?;
                    }
                    #[cfg(not(feature = "nix"))]
                    {
                        // No-op when nix feature is disabled
                    }
                    debug!("Set file descriptor limit: {}", max_fds);
                }

                // Apply process limit (RLIMIT_NPROC = number of processes)
                if let Some(max_children) = limits.max_child_processes {
                    // Get current process count and add max_children as limit
                    #[cfg(feature = "nix")]
                    let soft_limit = max_children as u64;
                    #[cfg(feature = "nix")]
                    let hard_limit = max_children as u64;
                    #[cfg(feature = "nix")]
                    setrlimit(Resource::RLIMIT_NPROC, soft_limit, hard_limit).map_err(|e| {
                        ModuleError::OperationError(format!("Failed to set process limit: {}", e))
                    })?;
                    debug!("Set process limit: {}", max_children);
                }

                // CPU limit is typically enforced via cgroups or process scheduling
                // setrlimit doesn't directly limit CPU percentage, but we can use RLIMIT_CPU
                // which limits CPU time in seconds (not percentage)
                // For percentage-based limits, cgroups would be needed
                if self.config.strict_mode {
                    debug!("Strict sandboxing enabled - resource limits applied");
                }
            } else {
                debug!("No PID provided, skipping resource limit application");
            }
        }

        #[cfg(not(unix))]
        {
            if self.config.strict_mode {
                warn!("Strict sandboxing requested but Windows support not yet implemented");
                return Err(ModuleError::OperationError(
                    "Windows sandboxing not yet implemented".to_string(),
                ));
            }
            debug!("Resource limits configured (Windows support pending)");
        }

        Ok(())
    }

    /// Monitor process resource usage
    pub async fn monitor_resources(&self, pid: Option<u32>) -> Result<ResourceUsage, ModuleError> {
        #[cfg(unix)]
        {
            if let Some(pid) = pid {
                // Read resource usage from /proc/<pid>/stat (Linux-specific)
                // For cross-platform support, we'd need platform-specific implementations
                let proc_stat_path = format!("/proc/{}/stat", pid);
                if let Ok(stat_content) = std::fs::read_to_string(&proc_stat_path) {
                    let fields: Vec<&str> = stat_content.split_whitespace().collect();
                    if fields.len() >= 24 {
                        // Field 14 (index 13): utime - CPU time spent in user mode (clock ticks)
                        // Field 15 (index 14): stime - CPU time spent in kernel mode (clock ticks)
                        // Field 23 (index 22): rss - Resident Set Size (pages)
                        let utime: u64 = fields.get(13).and_then(|s| s.parse().ok()).unwrap_or(0);
                        let stime: u64 = fields.get(14).and_then(|s| s.parse().ok()).unwrap_or(0);
                        let rss_pages: u64 =
                            fields.get(22).and_then(|s| s.parse().ok()).unwrap_or(0);

                        // Get page size (typically 4096 bytes on Linux)
                        #[cfg(feature = "libc")]
                        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
                        #[cfg(not(feature = "libc"))]
                        let page_size = 4096u64; // Default page size
                        let memory_bytes = rss_pages * page_size;

                        // CPU percentage calculation would require sampling over time
                        // For now, return 0.0 (would need previous sample to calculate)
                        let cpu_percent = 0.0;

                        // Count file descriptors from /proc/<pid>/fd
                        let fd_count = std::fs::read_dir(format!("/proc/{}/fd", pid))
                            .map(|dir| dir.count() as u32)
                            .unwrap_or(0);

                        // Count child processes (simplified - would need to traverse process tree)
                        let child_processes = 0;

                        return Ok(ResourceUsage {
                            cpu_percent,
                            memory_bytes,
                            file_descriptors: fd_count,
                            child_processes,
                        });
                    }
                }

                // Fallback: return zeros if we can't read proc
                Ok(ResourceUsage {
                    cpu_percent: 0.0,
                    memory_bytes: 0,
                    file_descriptors: 0,
                    child_processes: 0,
                })
            } else {
                Ok(ResourceUsage {
                    cpu_percent: 0.0,
                    memory_bytes: 0,
                    file_descriptors: 0,
                    child_processes: 0,
                })
            }
        }

        #[cfg(not(unix))]
        {
            // Windows implementation would use process APIs
            Ok(ResourceUsage {
                cpu_percent: 0.0,
                memory_bytes: 0,
                file_descriptors: 0,
                child_processes: 0,
            })
        }
    }

    /// Get sandbox configuration
    pub fn config(&self) -> &SandboxConfig {
        &self.config
    }
}

/// Current resource usage for a process
#[derive(Debug, Clone)]
pub struct ResourceUsage {
    /// CPU usage percentage
    pub cpu_percent: f64,
    /// Memory usage in bytes
    pub memory_bytes: u64,
    /// Number of open file descriptors
    pub file_descriptors: u32,
    /// Number of child processes
    pub child_processes: u32,
}

impl ResourceUsage {
    /// Check if resource usage exceeds limits
    pub fn exceeds_limits(&self, limits: &ResourceLimits) -> bool {
        if let Some(max_cpu) = limits.max_cpu_percent {
            if self.cpu_percent > max_cpu as f64 {
                return true;
            }
        }
        if let Some(max_memory) = limits.max_memory_bytes {
            if self.memory_bytes > max_memory {
                return true;
            }
        }
        if let Some(max_fds) = limits.max_file_descriptors {
            if self.file_descriptors > max_fds {
                return true;
            }
        }
        if let Some(max_children) = limits.max_child_processes {
            if self.child_processes > max_children {
                return true;
            }
        }
        false
    }
}
