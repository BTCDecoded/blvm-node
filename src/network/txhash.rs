//! Lightweight hashing utilities (non-consensus) for relay features
//!
//! These helpers compute txid, wtxid, and block header hash. For txid/wtxid we use
//! blvm_protocol::block::calculate_tx_id (canonical Bitcoin serialization) when
//! available. Block header hash uses bincode for relay purposes.

use blvm_protocol::{BlockHeader, Hash, Transaction};
use sha2::{Digest, Sha256};

/// Compute wtxid (witness txid) per BIP141. For package relay (BIP331).
/// When witnesses is None or all empty, returns txid (wtxid == txid for non-SegWit).
/// witnesses: one witness stack per input, each stack is Vec<Vec<u8>> (witness elements).
#[cfg(any(feature = "production", feature = "profile"))]
pub fn calculate_wtxid(
    tx: &Transaction,
    witnesses: Option<&[Vec<Vec<u8>>]>,
) -> Hash {
    use blvm_protocol::block::calculate_tx_id;

    let txid = calculate_tx_id(tx);
    let has_witness = witnesses
        .map(|w| w.iter().any(|s| !s.is_empty()))
        .unwrap_or(false);
    if !has_witness {
        return txid;
    }
    let witnesses = witnesses.expect("checked above");
    // SegWit serialization: version + marker(0x00) + flag(0x01) + inputs + outputs + locktime + witness
    let serialized = segwit_serialize_for_wtxid(tx, witnesses);
    let first = Sha256::digest(&serialized);
    let second = Sha256::digest(first);
    let mut out = [0u8; 32];
    out.copy_from_slice(&second);
    out
}

#[cfg(any(feature = "production", feature = "profile"))]
fn segwit_serialize_for_wtxid(tx: &Transaction, witnesses: &[Vec<Vec<u8>>]) -> Vec<u8> {
    use blvm_consensus::serialization::varint::encode_varint;

    let mut out = Vec::new();
    out.extend_from_slice(&(tx.version as u32).to_le_bytes());
    out.push(0x00);
    out.push(0x01);
    out.extend_from_slice(&encode_varint(tx.inputs.len() as u64));
    for input in &tx.inputs {
        out.extend_from_slice(&input.prevout.hash);
        out.extend_from_slice(&input.prevout.index.to_le_bytes());
        out.extend_from_slice(&encode_varint(input.script_sig.len() as u64));
        out.extend_from_slice(&input.script_sig);
        out.extend_from_slice(&(input.sequence as u32).to_le_bytes());
    }
    out.extend_from_slice(&encode_varint(tx.outputs.len() as u64));
    for output in &tx.outputs {
        out.extend_from_slice(&(output.value as u64).to_le_bytes());
        out.extend_from_slice(&encode_varint(output.script_pubkey.len() as u64));
        out.extend_from_slice(&output.script_pubkey);
    }
    out.extend_from_slice(&(tx.lock_time as u32).to_le_bytes());
    for stack in witnesses {
        out.extend_from_slice(&encode_varint(stack.len() as u64));
        for elem in stack {
            out.extend_from_slice(&encode_varint(elem.len() as u64));
            out.extend_from_slice(elem);
        }
    }
    out
}

/// Compute wtxid when blvm-consensus not available. Falls back to txid (correct for non-SegWit).
#[cfg(not(any(feature = "production", feature = "profile")))]
pub fn calculate_wtxid(_tx: &Transaction, _witnesses: Option<&[Vec<Vec<u8>>]>) -> Hash {
    use blvm_protocol::block::calculate_tx_id;
    calculate_tx_id(_tx)
}

/// Compute a best-effort txid (double-SHA256 of serialized transaction)
/// Note: For BIP331 package relay, use calculate_wtxid with blvm_protocol::block::calculate_tx_id.
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
