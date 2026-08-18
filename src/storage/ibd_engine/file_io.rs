//! Portable positioned read for IBD engine flat files (Unix `read_at`, Windows `seek_read`).

use std::fs::File;
use std::io::{Error, ErrorKind, Result};

/// Read exactly `buf.len()` bytes from `file` at `offset` without mutating the file cursor
/// (where supported).
///
/// Loops on short `pread`/`seek_read` returns. Linux `pread` commonly caps a single call near
/// 2 GiB (`SSIZE_MAX`); a one-shot call for an 80M×56 B HotPin load (~4.2 GiB) left the
/// unread tail as zeros and caused post-seed `MISSING_UTXO`.
pub fn read_at(file: &File, buf: &mut [u8], offset: u64) -> Result<usize> {
    let need = buf.len();
    let mut done = 0usize;
    while done < need {
        let n = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileExt;
                file.read_at(&mut buf[done..], offset + done as u64)?
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::FileExt;
                file.seek_read(&mut buf[done..], offset + done as u64)?
            }
            #[cfg(not(any(unix, windows)))]
            {
                use std::io::{Read, Seek, SeekFrom};
                file.seek(SeekFrom::Start(offset + done as u64))?;
                file.read(&mut buf[done..])?
            }
        };
        if n == 0 {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                format!("read_at short: got {done}/{need} bytes at offset {offset}"),
            ));
        }
        done += n;
    }
    Ok(done)
}
