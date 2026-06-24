//! Bitcoin Core-compatible CMerkleBlock / CPartialMerkleTree (BIP37 SPV proofs).
//!
//! Wire format matches Bitcoin Core `merkleblock.h` serialization used by
//! `gettxoutproof` / `verifytxoutproof`.

use crate::storage::hashing::double_sha256;
use blvm_protocol::BlockHeader;
use blvm_protocol::serialization::serialize_block_header;
use blvm_protocol::serialization::varint::{decode_varint, encode_varint};

#[derive(Debug, Clone)]
pub struct PartialMerkleTree {
    n_transactions: u32,
    v_bits: Vec<bool>,
    v_hash: Vec<[u8; 32]>,
    bad: bool,
}

#[derive(Debug, Clone)]
pub struct MerkleBlock {
    pub header: BlockHeader,
    pub txn: PartialMerkleTree,
}

#[derive(Debug)]
pub enum MerkleBlockError {
    EmptyTransactions,
    InvalidEncoding(String),
    BadProof,
}

impl std::fmt::Display for MerkleBlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MerkleBlockError::EmptyTransactions => write!(f, "block has no transactions"),
            MerkleBlockError::InvalidEncoding(msg) => write!(f, "{msg}"),
            MerkleBlockError::BadProof => write!(f, "invalid merkle proof"),
        }
    }
}

impl std::error::Error for MerkleBlockError {}

fn calc_tree_width(n_transactions: u32, height: u32) -> u32 {
    (n_transactions + (1 << height) - 1) >> height
}

fn merge_merkle_node(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut combined = [0u8; 64];
    combined[..32].copy_from_slice(left);
    combined[32..].copy_from_slice(right);
    double_sha256(&combined)
}

fn calc_hash(height: u32, pos: u32, txids: &[[u8; 32]], n_transactions: u32) -> [u8; 32] {
    if height == 0 {
        txids[pos as usize]
    } else {
        let left = calc_hash(height - 1, pos * 2, txids, n_transactions);
        let right = if pos * 2 + 1 < calc_tree_width(n_transactions, height - 1) {
            calc_hash(height - 1, pos * 2 + 1, txids, n_transactions)
        } else {
            left
        };
        merge_merkle_node(&left, &right)
    }
}

fn traverse_and_build(
    height: u32,
    pos: u32,
    txids: &[[u8; 32]],
    v_match: &[bool],
    n_transactions: u32,
    v_bits: &mut Vec<bool>,
    v_hash: &mut Vec<[u8; 32]>,
) {
    let mut parent_of_match = false;
    for p in (pos << height)..((pos + 1) << height).min(n_transactions) {
        if v_match[p as usize] {
            parent_of_match = true;
            break;
        }
    }
    v_bits.push(parent_of_match);

    if height == 0 || !parent_of_match {
        v_hash.push(calc_hash(height, pos, txids, n_transactions));
    } else {
        traverse_and_build(
            height - 1,
            pos * 2,
            txids,
            v_match,
            n_transactions,
            v_bits,
            v_hash,
        );
        if pos * 2 + 1 < calc_tree_width(n_transactions, height - 1) {
            traverse_and_build(
                height - 1,
                pos * 2 + 1,
                txids,
                v_match,
                n_transactions,
                v_bits,
                v_hash,
            );
        }
    }
}

fn traverse_and_extract(
    height: u32,
    pos: u32,
    n_transactions: u32,
    v_bits: &[bool],
    v_hash: &[[u8; 32]],
    n_bits_used: &mut usize,
    n_hash_used: &mut usize,
    v_match: &mut Vec<[u8; 32]>,
    bad: &mut bool,
) -> [u8; 32] {
    if *n_bits_used >= v_bits.len() {
        *bad = true;
        return [0u8; 32];
    }
    let parent_of_match = v_bits[*n_bits_used];
    *n_bits_used += 1;

    if height == 0 || !parent_of_match {
        if *n_hash_used >= v_hash.len() {
            *bad = true;
            return [0u8; 32];
        }
        let hash = v_hash[*n_hash_used];
        *n_hash_used += 1;
        if height == 0 && parent_of_match {
            v_match.push(hash);
        }
        hash
    } else {
        let left = traverse_and_extract(
            height - 1,
            pos * 2,
            n_transactions,
            v_bits,
            v_hash,
            n_bits_used,
            n_hash_used,
            v_match,
            bad,
        );
        let right = if pos * 2 + 1 < calc_tree_width(n_transactions, height - 1) {
            traverse_and_extract(
                height - 1,
                pos * 2 + 1,
                n_transactions,
                v_bits,
                v_hash,
                n_bits_used,
                n_hash_used,
                v_match,
                bad,
            )
        } else {
            left
        };
        if right == left && height > 0 {
            *bad = true;
        }
        merge_merkle_node(&left, &right)
    }
}

fn bits_to_bytes(bits: &[bool]) -> Vec<u8> {
    let byte_len = bits.len().div_ceil(8);
    let mut out = vec![0u8; byte_len];
    for (p, bit) in bits.iter().enumerate() {
        if *bit {
            out[p / 8] |= 1 << (p % 8);
        }
    }
    out
}

fn bytes_to_bits(bytes: &[u8]) -> Vec<bool> {
    let mut out = vec![false; bytes.len() * 8];
    for (p, bit) in out.iter_mut().enumerate().take(bytes.len() * 8) {
        *bit = (bytes[p / 8] & (1 << (p % 8))) != 0;
    }
    out
}

impl PartialMerkleTree {
    pub fn new(txids: &[[u8; 32]], v_match: &[bool]) -> Result<Self, MerkleBlockError> {
        if txids.is_empty() {
            return Err(MerkleBlockError::EmptyTransactions);
        }
        if txids.len() != v_match.len() {
            return Err(MerkleBlockError::InvalidEncoding(
                "txid/match length mismatch".into(),
            ));
        }

        let n_transactions = txids.len() as u32;
        let mut height = 0u32;
        while calc_tree_width(n_transactions, height) > 1 {
            height += 1;
        }

        let mut v_bits = Vec::new();
        let mut v_hash = Vec::new();
        traverse_and_build(
            height,
            0,
            txids,
            v_match,
            n_transactions,
            &mut v_bits,
            &mut v_hash,
        );

        Ok(Self {
            n_transactions,
            v_bits,
            v_hash,
            bad: false,
        })
    }

    pub fn n_transactions(&self) -> u32 {
        self.n_transactions
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.n_transactions.to_le_bytes());
        out.extend_from_slice(&encode_varint(self.v_hash.len() as u64));
        for hash in &self.v_hash {
            out.extend_from_slice(hash);
        }
        let flag_bytes = bits_to_bytes(&self.v_bits);
        out.extend_from_slice(&encode_varint(flag_bytes.len() as u64));
        out.extend_from_slice(&flag_bytes);
        out
    }

    pub fn deserialize(data: &[u8]) -> Result<Self, MerkleBlockError> {
        if data.len() < 4 {
            return Err(MerkleBlockError::InvalidEncoding(
                "partial merkle tree too short".into(),
            ));
        }
        let n_transactions = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let mut offset = 4;

        let (hash_count, consumed) = decode_varint(&data[offset..])
            .map_err(|e| MerkleBlockError::InvalidEncoding(format!("hash count varint: {e}")))?;
        offset += consumed;

        let hash_count = hash_count as usize;
        if data.len() < offset + hash_count * 32 {
            return Err(MerkleBlockError::InvalidEncoding(
                "truncated hash list".into(),
            ));
        }
        let mut v_hash = Vec::with_capacity(hash_count);
        for i in 0..hash_count {
            let start = offset + i * 32;
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&data[start..start + 32]);
            v_hash.push(hash);
        }
        offset += hash_count * 32;

        let (flag_len, consumed) = decode_varint(&data[offset..])
            .map_err(|e| MerkleBlockError::InvalidEncoding(format!("flag length varint: {e}")))?;
        offset += consumed;

        if data.len() < offset + flag_len as usize {
            return Err(MerkleBlockError::InvalidEncoding(
                "truncated flag bytes".into(),
            ));
        }
        let v_bits = bytes_to_bits(&data[offset..offset + flag_len as usize]);

        Ok(Self {
            n_transactions,
            v_bits,
            v_hash,
            bad: false,
        })
    }

    /// Returns `(merkle_root, matched_txids)`; root is `[0; 32]` on failure.
    pub fn extract_matches(&mut self) -> ([u8; 32], Vec<[u8; 32]>) {
        let mut v_match = Vec::new();
        if self.n_transactions == 0 {
            return ([0u8; 32], v_match);
        }
        if self.v_hash.len() > self.n_transactions as usize {
            self.bad = true;
            return ([0u8; 32], v_match);
        }
        if self.v_bits.len() < self.v_hash.len() {
            self.bad = true;
            return ([0u8; 32], v_match);
        }

        let mut height = 0u32;
        while calc_tree_width(self.n_transactions, height) > 1 {
            height += 1;
        }

        let mut n_bits_used = 0;
        let mut n_hash_used = 0;
        let root = traverse_and_extract(
            height,
            0,
            self.n_transactions,
            &self.v_bits,
            &self.v_hash,
            &mut n_bits_used,
            &mut n_hash_used,
            &mut v_match,
            &mut self.bad,
        );

        if self.bad {
            return ([0u8; 32], Vec::new());
        }
        if n_bits_used.div_ceil(8) != self.v_bits.len().div_ceil(8) {
            self.bad = true;
            return ([0u8; 32], Vec::new());
        }
        if n_hash_used != self.v_hash.len() {
            self.bad = true;
            return ([0u8; 32], Vec::new());
        }

        (root, v_match)
    }
}

impl MerkleBlock {
    pub fn new(
        header: BlockHeader,
        txids: &[[u8; 32]],
        v_match: &[bool],
    ) -> Result<Self, MerkleBlockError> {
        Ok(Self {
            header,
            txn: PartialMerkleTree::new(txids, v_match)?,
        })
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut out = serialize_block_header(&self.header);
        out.extend_from_slice(&self.txn.serialize());
        out
    }

    pub fn deserialize(data: &[u8]) -> Result<Self, MerkleBlockError> {
        if data.len() < 80 {
            return Err(MerkleBlockError::InvalidEncoding(
                "merkle block too short for header".into(),
            ));
        }
        use blvm_protocol::serialization::block::deserialize_block_header;
        let header = deserialize_block_header(&data[..80])
            .map_err(|e| MerkleBlockError::InvalidEncoding(format!("header: {e}")))?;
        let txn = PartialMerkleTree::deserialize(&data[80..])?;
        Ok(Self { header, txn })
    }
}

pub fn block_hash_from_header(header: &BlockHeader) -> [u8; 32] {
    double_sha256(&serialize_block_header(header))
}

#[cfg(test)]
mod tests {
    use super::*;
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput};

    fn sample_tx(n: u8) -> Transaction {
        Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![TransactionInput {
                prevout: OutPoint {
                    hash: [n; 32],
                    index: 0,
                },
                script_sig: vec![0x51],
                sequence: 0xffffffff,
            }],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 1000,
                script_pubkey: vec![0x51].into(),
            }],
            lock_time: 0,
        }
    }

    #[test]
    fn merkle_block_roundtrip_single_tx() {
        let tx = sample_tx(1);
        let txid = calculate_tx_id(&tx);
        let header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: txid,
            timestamp: 1,
            bits: 0x207fffff,
            nonce: 0,
        };
        let mb = MerkleBlock::new(header.clone(), &[txid], &[true]).unwrap();
        let bytes = mb.serialize();
        let decoded = MerkleBlock::deserialize(&bytes).unwrap();
        let mut pmt = decoded.txn;
        let (root, matched) = pmt.extract_matches();
        assert_eq!(root, header.merkle_root);
        assert_eq!(matched, vec![txid]);
    }

    #[test]
    fn merkle_block_roundtrip_two_txs_second_matched() {
        let tx1 = sample_tx(1);
        let tx2 = sample_tx(2);
        let txid1 = calculate_tx_id(&tx1);
        let txid2 = calculate_tx_id(&tx2);
        let txids = [txid1, txid2];
        let root = {
            let mut h = txids.to_vec();
            while h.len() > 1 {
                let mut next = Vec::new();
                for chunk in h.chunks(2) {
                    let left = chunk[0];
                    let right = if chunk.len() == 2 { chunk[1] } else { chunk[0] };
                    next.push(merge_merkle_node(&left, &right));
                }
                h = next;
            }
            h[0]
        };
        let header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: root,
            timestamp: 1,
            bits: 0x207fffff,
            nonce: 0,
        };
        let mb = MerkleBlock::new(header.clone(), &txids, &[false, true]).unwrap();
        let decoded = MerkleBlock::deserialize(&mb.serialize()).unwrap();
        let mut pmt = decoded.txn;
        let (extracted_root, matched) = pmt.extract_matches();
        assert_eq!(extracted_root, root);
        assert_eq!(matched, vec![txid2]);
    }
}
