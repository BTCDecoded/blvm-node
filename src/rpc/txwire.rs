//! Witness-aware transaction bytes for RPC.
//!
//! `txid` and `hash` are BIP145 display order: the reversed SHA256d digest,
//! the same encoding as Bitcoin Core `GetHex()` and rust-bitcoin `Display`.
//! `data` stays the wire bytes. A transaction with no witness uses the
//! non-witness serialization, so `hash` equals `txid`.

use blvm_protocol::Transaction;
use blvm_protocol::block::calculate_tx_id;
use blvm_protocol::segwit::Witness;
use blvm_protocol::serialization::serialize_transaction;
use blvm_protocol::serialization::serialize_transaction_with_witness;
use sha2::{Digest, Sha256};

use crate::storage::hashing::hash_to_rpc_hex;

pub(crate) struct TxWire {
    pub bytes: Vec<u8>,
    pub txid_hex: String,
    pub hash_hex: String,
}

pub(crate) fn tx_wire(tx: &Transaction, witnesses: Option<&[Witness]>) -> TxWire {
    let txid = calculate_tx_id(tx);
    let txid_hex = hash_to_rpc_hex(&txid);
    let has_witness = witnesses
        .map(|stacks| stacks.iter().any(|stack| !stack.is_empty()))
        .unwrap_or(false);
    let bytes = if has_witness {
        let stacks = witnesses.unwrap_or(&[]);
        let padded = padded_witnesses(tx, stacks);
        serialize_transaction_with_witness(tx, &padded)
    } else {
        serialize_transaction(tx)
    };
    let hash_hex = if has_witness {
        hash_to_rpc_hex(&sha256d(&bytes))
    } else {
        txid_hex.clone()
    };
    TxWire {
        bytes,
        txid_hex,
        hash_hex,
    }
}

fn padded_witnesses(tx: &Transaction, witnesses: &[Witness]) -> Vec<Witness> {
    (0..tx.inputs.len())
        .map(|index| witnesses.get(index).cloned().unwrap_or_else(Witness::new))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::tx_wire;
    use blvm_protocol::serialization::deserialize_transaction_with_witness;

    /// Display ids from rust-bitcoin 0.32 `compute_txid` / `compute_wtxid`.
    /// The genesis id is also Bitcoin Core's published coinbase txid.
    #[test]
    fn rpc_identifiers_match_rust_bitcoin_display_order() {
        let (txid, hash) = identifiers(
            "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff4d04ffff001d0104455468652054696d65732030332f4a616e2f32303039204368616e63656c6c6f72206f6e206272696e6b206f66207365636f6e64206261696c6f757420666f722062616e6b73ffffffff0100f2052a01000000434104678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5fac00000000",
        );
        assert_eq!(
            txid,
            "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b"
        );
        assert_eq!(hash, txid);

        let (txid, hash) = identifiers(
            "0200000000010100000000000000000000000000000000000000000000000000000000000000000000000000ffffffff010000000000000000015101015100000000",
        );
        assert_eq!(
            txid,
            "fbc337e9a8f09fb0468dbd5661f5f158bbbd128a804cf04cc9f291f58b4ec8f3"
        );
        assert_eq!(
            hash,
            "fea9f47f3d1a1e98766c0fda747c10e10d850ba37bb694f557a0f5f61851b842"
        );
    }

    fn identifiers(hex_tx: &str) -> (String, String) {
        let bytes = hex::decode(hex_tx).unwrap();
        let (tx, witnesses, _) = deserialize_transaction_with_witness(&bytes).unwrap();
        let stacks = witnesses.iter().any(|stack| !stack.is_empty());
        let wire = tx_wire(&tx, stacks.then_some(witnesses.as_slice()));
        (wire.txid_hex, wire.hash_hex)
    }
}

fn sha256d(bytes: &[u8]) -> [u8; 32] {
    let first = Sha256::digest(bytes);
    let second = Sha256::digest(first);
    let mut out = [0u8; 32];
    out.copy_from_slice(&second);
    out
}
