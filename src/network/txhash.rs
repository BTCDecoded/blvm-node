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
pub fn calculate_wtxid(tx: &Transaction, witnesses: Option<&[Vec<Vec<u8>>]>) -> Hash {
    use blvm_protocol::block::calculate_tx_id;

    let txid = calculate_tx_id(tx);
    let has_witness = witnesses
        .map(|w| w.iter().any(|s| !s.is_empty()))
        .unwrap_or(false);
    if !has_witness {
        return txid;
    }
    let witnesses = witnesses.expect("checked above");
    let Ok(serialized) =
        blvm_protocol::serialization::serialize_transaction_with_witness(tx, witnesses)
    else {
        return txid;
    };
    let first = Sha256::digest(&serialized);
    let second = Sha256::digest(first);
    let mut out = [0u8; 32];
    out.copy_from_slice(&second);
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

#[cfg(all(test, any(feature = "production", feature = "profile")))]
mod tests {
    use super::calculate_wtxid;
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::serialization::serialize_transaction_with_witness;
    use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput};
    use sha2::{Digest, Sha256};

    fn one_in_one_out() -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [1u8; 32],
                    index: 0,
                },
                script_sig: Vec::new(),
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 50_000,
                script_pubkey: vec![0x51],
            }]
            .into(),
            lock_time: 7,
        }
    }

    fn sha256d(bytes: &[u8]) -> [u8; 32] {
        let first = Sha256::digest(bytes);
        let second = Sha256::digest(first);
        let mut out = [0u8; 32];
        out.copy_from_slice(&second);
        out
    }

    #[test]
    fn wtxid_hashes_witness_before_locktime() {
        let tx = one_in_one_out();
        let stacks = vec![vec![vec![0x30, 0x01], vec![0x02]]];
        let serialized = serialize_transaction_with_witness(&tx, &stacks).unwrap();
        assert_eq!(calculate_wtxid(&tx, Some(&stacks)), sha256d(&serialized));
        assert_ne!(calculate_wtxid(&tx, Some(&stacks)), calculate_tx_id(&tx));
        let empty = vec![vec![]];
        assert_eq!(calculate_wtxid(&tx, Some(&empty)), calculate_tx_id(&tx));
    }
}
