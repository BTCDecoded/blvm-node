//! One process owns a data directory.
//!
//! The kernel lock is a non-blocking exclusive `F_SETLK` on `.lock`. Record locks
//! do not fail a second open in the same process, so this also records `(st_dev, st_ino)`.

use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

#[cfg(all(unix, feature = "libc"))]
use std::os::unix::fs::MetadataExt;
#[cfg(all(unix, feature = "libc"))]
use std::os::unix::io::{AsRawFd, FromRawFd};

const LOCK_FILE: &str = ".lock";

fn held_dirs() -> &'static Mutex<HashSet<(u64, u64)>> {
    static HELD: OnceLock<Mutex<HashSet<(u64, u64)>>> = OnceLock::new();
    HELD.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Held for the process lifetime. Drop releases the kernel lock and the inode record.
#[derive(Debug)]
pub struct DataDirLock {
    _file: Option<File>,
    key: (u64, u64),
}

/// Lock `data_dir/.lock` for this process.
///
/// `O_NOFOLLOW` rejects a symlink. Anything that is not a regular file is rejected.
/// A second call for the same directory fails even inside this process.
pub fn acquire_data_dir_lock(data_dir: &Path) -> Result<DataDirLock> {
    #[cfg(all(unix, feature = "libc"))]
    {
        acquire_unix(data_dir)
    }
    #[cfg(not(all(unix, feature = "libc")))]
    {
        let _ = data_dir;
        bail!("data directory lock requires libc on unix")
    }
}

#[cfg(all(unix, feature = "libc"))]
fn acquire_unix(data_dir: &Path) -> Result<DataDirLock> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("cannot create data directory {}", data_dir.display()))?;
    let dir_meta = std::fs::metadata(data_dir)
        .with_context(|| format!("cannot stat data directory {}", data_dir.display()))?;
    if !dir_meta.is_dir() {
        bail!("{} is not a directory", data_dir.display());
    }
    let key = (dir_meta.dev(), dir_meta.ino());
    let mut held = held_dirs()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if held.contains(&key) {
        bail!(
            "data directory {} is already in use by this process",
            data_dir.display()
        );
    }

    let lock_path = data_dir.join(LOCK_FILE);
    let file = match open_nofollow(&lock_path) {
        Ok(file) => file,
        Err(error) if is_not_regular(&error) => {
            bail!("{}/{} is not a regular file", data_dir.display(), LOCK_FILE);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot open {}/{}", data_dir.display(), LOCK_FILE));
        }
    };
    let meta = file
        .metadata()
        .with_context(|| format!("cannot stat {}/{}", data_dir.display(), LOCK_FILE))?;
    if !meta.is_file() {
        bail!("{}/{} is not a regular file", data_dir.display(), LOCK_FILE);
    }
    match try_exclusive_lock(&file) {
        Ok(()) => {
            held.insert(key);
            Ok(DataDirLock {
                _file: Some(file),
                key,
            })
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock || lock_conflict(&error) => {
            bail!("data directory {} is already in use", data_dir.display());
        }
        Err(error) => {
            Err(error).with_context(|| format!("cannot lock {}/{}", data_dir.display(), LOCK_FILE))
        }
    }
}

#[cfg(all(unix, feature = "libc"))]
fn open_nofollow(path: &Path) -> io::Result<File> {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    bytes.push(0);
    let fd = unsafe {
        libc::open(
            bytes.as_ptr() as *const libc::c_char,
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(all(unix, feature = "libc"))]
fn is_not_regular(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code) if code == libc::ELOOP || code == libc::EISDIR
    )
}

#[cfg(all(unix, feature = "libc"))]
fn lock_conflict(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK || code == libc::EACCES
    )
}

#[cfg(all(unix, feature = "libc"))]
fn try_exclusive_lock(file: &File) -> io::Result<()> {
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    lock.l_start = 0;
    lock.l_len = 0;
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &mut lock) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

impl Drop for DataDirLock {
    fn drop(&mut self) {
        let mut held = held_dirs()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        drop(self._file.take());
        held.remove(&self.key);
    }
}

#[cfg(all(test, unix, feature = "libc"))]
mod tests {
    use super::acquire_data_dir_lock;
    use std::os::unix::fs::symlink;

    #[test]
    fn a_second_acquire_in_this_process_fails_until_drop() {
        let dir = tempfile::TempDir::new().unwrap();
        let first = acquire_data_dir_lock(dir.path()).unwrap();
        let second = acquire_data_dir_lock(dir.path());
        assert!(second.is_err(), "record locks do not exclude this process");
        drop(first);
        let third = acquire_data_dir_lock(dir.path());
        assert!(third.is_ok());
    }

    #[test]
    fn a_symlink_lock_file_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"x").unwrap();
        symlink(&target, dir.path().join(".lock")).unwrap();
        let error = acquire_data_dir_lock(dir.path()).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("not a regular file"), "{message}");
    }

    #[test]
    fn a_directory_lock_file_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".lock")).unwrap();
        let error = acquire_data_dir_lock(dir.path()).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("not a regular file"), "{message}");
    }

    #[test]
    fn two_directories_lock_independently() {
        let left = tempfile::TempDir::new().unwrap();
        let right = tempfile::TempDir::new().unwrap();
        let _left = acquire_data_dir_lock(left.path()).unwrap();
        let _right = acquire_data_dir_lock(right.path()).unwrap();
    }
}
