//! Witness-aware transaction bytes for RPC.
//!
//! `hash` is the double SHA256 of these bytes. A transaction with no witness
//! uses the non-witness serialization, so `hash` equals `txid`.

use blvm_protocol::Transaction;
use blvm_protocol::block::calculate_tx_id;
use blvm_protocol::segwit::Witness;
use blvm_protocol::serialization::serialize_transaction;
use blvm_protocol::serialization::serialize_transaction_with_witness;
use sha2::{Digest, Sha256};

pub(crate) struct TxWire {
    pub bytes: Vec<u8>,
    pub txid_hex: String,
    pub hash_hex: String,
}

pub(crate) fn tx_wire(tx: &Transaction, witnesses: Option<&[Witness]>) -> TxWire {
    let txid = calculate_tx_id(tx);
    let txid_hex = hex::encode(txid);
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
        hex::encode(sha256d(&bytes))
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

fn sha256d(bytes: &[u8]) -> [u8; 32] {
    let first = Sha256::digest(bytes);
    let second = Sha256::digest(first);
    let mut out = [0u8; 32];
    out.copy_from_slice(&second);
    out
}
