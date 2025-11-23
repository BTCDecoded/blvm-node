//! Lightweight hashing utilities (non-consensus) for relay features
//!
//! These helpers compute txid and block header hash using double SHA256 over
//! bincode serialization of the in-memory structures. They are suitable for
//! networking/relay purposes in this crate and do NOT replace consensus hashing.

use bllvm_protocol::{BlockHeader, Hash, Transaction};
use sha2::{Digest, Sha256};

/// Compute a best-effort txid (double-SHA256 of serialized transaction)
pub fn calculate_txid(tx: &Transaction) -> Hash {
    let serialized = bincode::serialize(tx).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(&serialized);
    let first = hasher.finalize();

    let mut hasher2 = Sha256::new();
    hasher2.update(first);
    let final_bytes = hasher2.finalize();

    let mut out = [0u8; 32];
    out.copy_from_slice(&final_bytes);
    out
}

/// Compute a best-effort block header hash (double-SHA256 of header)
pub fn calculate_block_header_hash(header: &BlockHeader) -> Hash {
    let serialized = bincode::serialize(header).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(&serialized);
    let first = hasher.finalize();

    let mut hasher2 = Sha256::new();
    hasher2.update(first);
    let final_bytes = hasher2.finalize();

    let mut out = [0u8; 32];
    out.copy_from_slice(&final_bytes);
    out
}
