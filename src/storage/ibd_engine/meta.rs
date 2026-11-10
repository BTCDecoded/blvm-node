//! On-disk sidecars for the engine flat table (resume hints; not consensus-critical).

use std::path::{Path, PathBuf};

pub fn contiguous_length_sidecar(table_path: &Path) -> PathBuf {
    let mut p = table_path.as_os_str().to_owned();
    p.push(".contiguous_length");
    PathBuf::from(p)
}

/// Read persisted append watermark. Returns `None` if missing or corrupt.
pub fn read_contiguous_length_sidecar(table_path: &Path) -> Option<i32> {
    let data = std::fs::read(contiguous_length_sidecar(table_path)).ok()?;
    if data.len() != 4 {
        return None;
    }
    let arr: [u8; 4] = data.try_into().ok()?;
    Some(i32::from_be_bytes(arr))
}

/// Best-effort write (no fsync — hint for skip-reseed; ckpt export is authoritative).
pub fn write_contiguous_length_sidecar(table_path: &Path, cl: i32) -> std::io::Result<()> {
    std::fs::write(contiguous_length_sidecar(table_path), cl.to_be_bytes())
}

pub fn remove_contiguous_length_sidecar(table_path: &Path) {
    let _ = std::fs::remove_file(contiguous_length_sidecar(table_path));
}

pub fn engine_dirty_flag_path(table_path: &Path) -> PathBuf {
    let mut p = table_path.as_os_str().to_owned();
    p.push(".dirty");
    PathBuf::from(p)
}

/// Remove the dirty flag after a graceful shutdown so the next open loads segments.
pub fn clear_engine_dirty_flag(table_path: &Path) {
    let _ = std::fs::remove_file(engine_dirty_flag_path(table_path));
}
