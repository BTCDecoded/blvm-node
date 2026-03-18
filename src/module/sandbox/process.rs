//! Process-level sandboxing and resource limits
//!
//! Implements CPU, memory, and file descriptor limits for module processes.

// nix imports are used conditionally within functions

use std::path::Path;
use tracing::{debug, warn};

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

    /// Create a new sandbox config with resource limits from ModuleResourceLimitsConfig
    pub fn with_resource_limits<P: AsRef<Path>>(
        data_dir: P,
        config: &crate::config::ModuleResourceLimitsConfig,
    ) -> Self {
        let resource_limits = ResourceLimits {
            max_cpu_percent: Some(config.default_max_cpu_percent),
            max_memory_bytes: Some(config.default_max_memory_bytes),
            max_file_descriptors: Some(config.default_max_file_descriptors),
            max_child_processes: Some(config.default_max_child_processes),
        };
        Self {
            allowed_data_dir: data_dir.as_ref().to_path_buf(),
            resource_limits,
            strict_mode: false,
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

    #[cfg(target_os = "windows")]
    fn apply_windows_job_limits(
        &self,
        pid: u32,
        limits: &ResourceLimits,
    ) -> Result<(), ModuleError> {
        use std::ptr::null_mut;

        #[allow(unused_imports)]
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        #[allow(unused_imports)]
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
            JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOBOBJECT_BASIC_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        #[allow(unused_imports)]
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };
        #[allow(unused_imports)]
        use windows_sys::core::PCWSTR;

        let job_name: Vec<u16> = format!("blvm-module-{}", pid)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let job_handle = unsafe { CreateJobObjectW(null_mut(), PCWSTR(job_name.as_ptr())) };

        if job_handle == 0 || job_handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            warn!("Failed to create Windows job object for PID {}: {}", pid, std::io::Error::last_os_error());
            return Err(ModuleError::op_err(
                "Failed to create job object",
                std::io::Error::last_os_error(),
            ));
        }

        // Job handle is intentionally not closed: limits persist while the job exists.
        // Closing would destroy the job and remove limits. Handles are freed when our process exits.

        let mut limit_info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();

        if let Some(max_memory) = limits.max_memory_bytes {
            limit_info.ProcessMemoryLimit = max_memory as usize;
            limit_info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        }

        if self.config.strict_mode {
            limit_info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        }

        let result = unsafe {
            SetInformationJobObject(
                job_handle,
                JobObjectExtendedLimitInformation,
                &limit_info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
            )
        };

        if result == 0 {
            warn!("Failed to set job object limits for PID {}: {}", pid, std::io::Error::last_os_error());
            return Err(ModuleError::op_err(
                "Failed to set job limits",
                std::io::Error::last_os_error(),
            ));
        }

        let process_handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_QUOTA | PROCESS_TERMINATE,
                0,
                pid,
            )
        };

        if process_handle == 0 || process_handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            warn!("Failed to open process {} for job assignment: {}", pid, std::io::Error::last_os_error());
            return Err(ModuleError::op_err(
                "Failed to open process",
                std::io::Error::last_os_error(),
            ));
        }

        let assign_result = unsafe { AssignProcessToJobObject(job_handle, process_handle) };
        unsafe { CloseHandle(process_handle); }

        if assign_result == 0 {
            warn!("Failed to assign process {} to job object: {}", pid, std::io::Error::last_os_error());
            return Err(ModuleError::op_err(
                "Failed to assign process to job",
                std::io::Error::last_os_error(),
            ));
        }

        debug!("Applied Windows job object limits for PID {} (memory: {:?})", pid, limits.max_memory_bytes);
        Ok(())
    }

    /// Apply resource limits to a process
    ///
    /// On Unix (Linux): uses `prlimit` for memory, FDs, processes; CPU % not yet supported.
    /// On Windows: uses job objects (memory limit, kill-on-close). CPU % not yet supported.
    pub fn apply_limits(&self, pid: Option<u32>) -> Result<(), ModuleError> {
        let limits = &self.config.resource_limits;

        #[cfg(unix)]
        {
            if let Some(pid) = pid {
                // Use prlimit (Linux-specific) to set limits on another process
                #[cfg(all(feature = "libc", target_os = "linux"))]
                {
                    use libc::{prlimit64, rlimit64, RLIMIT_AS, RLIMIT_NOFILE, RLIMIT_NPROC};

                    // Apply memory limit using prlimit
                    if let Some(max_memory) = limits.max_memory_bytes {
                        let rlim = rlimit64 {
                            rlim_cur: max_memory,
                            rlim_max: max_memory,
                        };
                        unsafe {
                            if prlimit64(pid as libc::pid_t, RLIMIT_AS, &rlim, std::ptr::null_mut())
                                != 0
                            {
                                warn!(
                                    "Failed to set memory limit for PID {} using prlimit: {}",
                                    pid,
                                    std::io::Error::last_os_error()
                                );
                            } else {
                                debug!("Set memory limit for PID {}: {} bytes", pid, max_memory);
                            }
                        }
                    }

                    // Apply file descriptor limit using prlimit
                    if let Some(max_fds) = limits.max_file_descriptors {
                        let rlim = rlimit64 {
                            rlim_cur: max_fds as u64,
                            rlim_max: max_fds as u64,
                        };
                        unsafe {
                            if prlimit64(
                                pid as libc::pid_t,
                                RLIMIT_NOFILE,
                                &rlim,
                                std::ptr::null_mut(),
                            ) != 0
                            {
                                warn!("Failed to set file descriptor limit for PID {} using prlimit: {}", pid, std::io::Error::last_os_error());
                            } else {
                                debug!("Set file descriptor limit for PID {}: {}", pid, max_fds);
                            }
                        }
                    }

                    // Apply process limit using prlimit
                    if let Some(max_children) = limits.max_child_processes {
                        let rlim = rlimit64 {
                            rlim_cur: max_children as u64,
                            rlim_max: max_children as u64,
                        };
                        unsafe {
                            if prlimit64(
                                pid as libc::pid_t,
                                RLIMIT_NPROC,
                                &rlim,
                                std::ptr::null_mut(),
                            ) != 0
                            {
                                warn!(
                                    "Failed to set process limit for PID {} using prlimit: {}",
                                    pid,
                                    std::io::Error::last_os_error()
                                );
                            } else {
                                debug!("Set process limit for PID {}: {}", pid, max_children);
                            }
                        }
                    }

                    // Apply CPU limit using prlimit (RLIMIT_CPU = CPU time in seconds)
                    if let Some(max_cpu_percent) = limits.max_cpu_percent {
                        // Convert percentage to CPU time limit (approximate: 100% = unlimited)
                        // For now, we'll skip CPU percentage as it requires more complex calculation
                        debug!(
                            "CPU percentage limit ({}) not yet implemented for prlimit",
                            max_cpu_percent
                        );
                    }

                    // Return early since we've applied limits via prlimit
                    return Ok(());
                }

                // Fallback: Note that setrlimit applies to the current process, not another process
                // For non-Linux systems or when libc feature is disabled, we can't set limits on another process
                #[cfg(not(all(feature = "libc", target_os = "linux")))]
                {
                    warn!("prlimit not available (requires Linux with libc feature). Limits should be set before spawning process with PID {}", pid);

                    // Apply memory limit (RLIMIT_AS = address space limit) - fallback for non-libc systems
                    #[cfg(feature = "nix")]
                    if let Some(max_memory) = limits.max_memory_bytes {
                        use nix::sys::resource::{setrlimit, Resource};
                        let soft_limit = max_memory as u64;
                        let hard_limit = max_memory as u64;
                        setrlimit(Resource::RLIMIT_AS, soft_limit, hard_limit)
                            .map_err(|e| ModuleError::op_err("Failed to set memory limit", e))?;
                        debug!("Set memory limit: {} bytes", max_memory);
                    }

                    // Apply file descriptor limit - fallback for non-libc systems
                    if let Some(max_fds) = limits.max_file_descriptors {
                        let soft_limit = max_fds as u64;
                        let hard_limit = max_fds as u64;
                        #[cfg(feature = "nix")]
                        {
                            use nix::sys::resource::{setrlimit, Resource};
                            setrlimit(Resource::RLIMIT_NOFILE, soft_limit, hard_limit)
                                .map_err(|e| ModuleError::op_err("Failed to set file descriptor limit", e))?;
                        }
                        #[cfg(not(feature = "nix"))]
                        {
                            // No-op when nix feature is disabled
                        }
                        debug!("Set file descriptor limit: {}", max_fds);
                    }

                    // Apply process limit (RLIMIT_NPROC = number of processes) - fallback for non-libc systems
                    if let Some(max_children) = limits.max_child_processes {
                        // Get current process count and add max_children as limit
                        #[cfg(feature = "nix")]
                        {
                            use nix::sys::resource::{setrlimit, Resource};
                            let soft_limit = max_children as u64;
                            let hard_limit = max_children as u64;
                            setrlimit(Resource::RLIMIT_NPROC, soft_limit, hard_limit)
                                .map_err(|e| ModuleError::op_err("Failed to set process limit", e))?;
                        }
                        #[cfg(not(feature = "nix"))]
                        {
                            // No-op when nix feature is disabled
                        }
                        debug!("Set process limit: {}", max_children);
                    }

                    // CPU limit is typically enforced via cgroups or process scheduling
                    // setrlimit doesn't directly limit CPU percentage, but we can use RLIMIT_CPU
                    // which limits CPU time in seconds (not percentage)
                    // For percentage-based limits, cgroups would be needed
                    if self.config.strict_mode {
                        debug!("Strict sandboxing enabled - resource limits applied");
                    }
                }
            } else {
                debug!("No PID provided, skipping resource limit application");
            }
        }

        #[cfg(target_os = "windows")]
        {
            if let Some(pid) = pid {
                self.apply_windows_job_limits(pid, limits)?;
            } else {
                debug!("No PID provided, skipping Windows job object limits");
            }
        }

        #[cfg(all(not(unix), not(target_os = "windows")))]
        {
            debug!("Resource limits not supported on this platform");
        }

        Ok(())
    }

    /// Monitor process resource usage
    pub async fn monitor_resources(&self, pid: Option<u32>) -> Result<ResourceUsage, ModuleError> {
        #[cfg(target_os = "linux")]
        {
            if let Some(pid) = pid {
                // Read resource usage from /proc/<pid>/stat (Linux-specific)
                let proc_stat_path = format!("/proc/{pid}/stat");
                if let Ok(stat_content) = std::fs::read_to_string(&proc_stat_path) {
                    let fields: Vec<&str> = stat_content.split_whitespace().collect();
                    if fields.len() >= 24 {
                        // Field 14 (index 13): utime - CPU time spent in user mode (clock ticks)
                        // Field 15 (index 14): stime - CPU time spent in kernel mode (clock ticks)
                        // Field 23 (index 22): rss - Resident Set Size (pages)
                        let _utime: u64 = fields.get(13).and_then(|s| s.parse().ok()).unwrap_or(0);
                        let _stime: u64 = fields.get(14).and_then(|s| s.parse().ok()).unwrap_or(0);
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
                        let fd_count = std::fs::read_dir(format!("/proc/{pid}/fd"))
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

        #[cfg(not(target_os = "linux"))]
        {
            // Windows/macOS: /proc doesn't exist. Return zeros; future: use sysinfo or platform APIs.
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
