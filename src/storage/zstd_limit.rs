//! One-shot zstd decode with a window cap and an output cap.
//!
//! Stored blobs are produced by `zstd::encode_all`. A replaced datadir can
//! still plant a frame whose header asks for a huge window or a huge output.
//! `decode_all` only caps the initial reservation.

use std::io::{self, Read};

/// Consensus serialized-block maximum. A stored block, witness, UTXO, or
/// index blob is one object and cannot legitimately expand past this.
pub const MAX_DECOMPRESSED_BLOB: usize = 4_000_000;

/// Largest back-reference window we will honor (8 MiB). Frames we write with
/// `encode_all` stay under this. A frame that asks for a larger window fails.
const WINDOW_LOG_MAX: u32 = 23;

pub fn decode_bounded(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut decoder = zstd::stream::read::Decoder::with_buffer(data)?;
    decoder.window_log_max(WINDOW_LOG_MAX)?;
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = decoder.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if out.len().saturating_add(n) > MAX_DECOMPRESSED_BLOB {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "decompressed output exceeds 4000000 bytes",
            ));
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}
