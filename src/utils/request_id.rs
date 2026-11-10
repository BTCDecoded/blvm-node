//! Request ID generation.
//!
//! Generates opaque, random hex strings used as JSON-RPC request correlation IDs.
//! Using `rand` + `hex` (already dependencies) avoids the `uuid` crate for a single function.

use rand::RngCore;

/// Generate a random 128-bit request ID as a hex string (32 hex chars).
///
/// Equivalent in randomness and uniqueness to `Uuid::new_v4()` for correlation purposes,
/// without requiring the `uuid` crate.
pub fn new_request_id() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}
