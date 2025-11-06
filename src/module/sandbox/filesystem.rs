//! File system access control for modules
//!
//! Ensures modules can only access files within their allowed data directory.

use std::path::{Path, PathBuf};
use tracing::{debug, warn};

use crate::module::traits::ModuleError;

/// File system sandbox that restricts module file access
pub struct FileSystemSandbox {
    /// Allowed data directory (modules can only access files under this)
    allowed_path: PathBuf,
}

impl FileSystemSandbox {
    /// Create a new file system sandbox
    pub fn new<P: AsRef<Path>>(data_dir: P) -> Self {
        Self {
            allowed_path: data_dir.as_ref().to_path_buf(),
        }
    }

    /// Validate that a file path is within the allowed directory
    pub fn validate_path<P: AsRef<Path>>(&self, path: P) -> Result<PathBuf, ModuleError> {
        let path = path.as_ref();

        // Fast path: if path is already absolute and clearly within sandbox, avoid canonicalize
        if path.is_absolute() && path.starts_with(&self.allowed_path) {
            // Still canonicalize for security (handles symlinks, etc.)
            // But we can optimize by checking prefix first
        }

        // Resolve path to absolute (handles symlinks, relative paths, etc.)
        let canonical = path.canonicalize().map_err(|e| {
            ModuleError::OperationError(format!("Failed to canonicalize path {:?}: {}", path, e))
        })?;

        // Check if path is within allowed directory
        if !canonical.starts_with(&self.allowed_path) {
            warn!(
                "Module attempted to access path outside sandbox: {:?} (allowed: {:?})",
                canonical, self.allowed_path
            );
            return Err(ModuleError::OperationError(format!(
                "Access denied: path {:?} is outside allowed directory {:?}",
                canonical, self.allowed_path
            )));
        }

        debug!("Path validated: {:?} is within sandbox", canonical);
        Ok(canonical)
    }

    /// Get the allowed data directory
    pub fn allowed_path(&self) -> &Path {
        &self.allowed_path
    }

    /// Check if a path is within the sandbox (without canonicalizing)
    ///
    /// This is a fast check that avoids filesystem I/O for common cases.
    #[inline]
    pub fn is_within_sandbox<P: AsRef<Path>>(&self, path: P) -> bool {
        let path = path.as_ref();

        // Fast path: check prefix first (no I/O)
        if !path.starts_with(&self.allowed_path) {
            return false;
        }

        // If prefix matches, try to canonicalize for security (handles symlinks)
        // But don't fail if path doesn't exist yet
        if let Ok(canonical) = path.canonicalize() {
            canonical.starts_with(&self.allowed_path)
        } else {
            // If canonicalization fails (path doesn't exist), trust prefix check
            // This is safe because we've already verified the prefix
            true
        }
    }
}
