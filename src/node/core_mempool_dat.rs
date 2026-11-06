//! Bitcoin Core `mempool.dat` (`DumpMempool` / `LoadMempool`).
//!
//! Version 1 is plain. Version 2 XORs every byte after the 8-byte key with
//! `key[file_offset % 8]`. The node's older length-prefixed dump is not this
//! layout; callers try this decoder only when the leading `u64` is 1 or 2.

use blvm_protocol::Transaction;
use blvm_protocol::segwit::Witness;
use blvm_protocol::serialization::transaction::{
    deserialize_transaction_with_witness, serialize_transaction, serialize_transaction_with_witness,
};

#[derive(Debug, Clone)]
pub struct CoreMempoolTx {
    pub tx: Transaction,
    pub witnesses: Option<Vec<Witness>>,
    pub time: i64,
    pub fee_delta: i64,
}

#[derive(Debug, Clone, Default)]
pub struct CoreMempoolDat {
    pub txs: Vec<CoreMempoolTx>,
    pub deltas: Vec<([u8; 32], i64)>,
    pub unbroadcast: Vec<[u8; 32]>,
}

pub fn encode(dump: &CoreMempoolDat) -> Result<Vec<u8>, String> {
    let mut out = 1u64.to_le_bytes().to_vec();
    out.extend_from_slice(&(dump.txs.len() as u64).to_le_bytes());
    for entry in &dump.txs {
        let bytes = match &entry.witnesses {
            Some(witnesses) => serialize_transaction_with_witness(&entry.tx, witnesses)
                .map_err(|err| err.to_string())?,
            None => serialize_transaction(&entry.tx),
        };
        out.extend_from_slice(&bytes);
        out.extend_from_slice(&entry.time.to_le_bytes());
        out.extend_from_slice(&entry.fee_delta.to_le_bytes());
    }
    push_compact(&mut out, dump.deltas.len() as u64);
    for (txid, delta) in &dump.deltas {
        out.extend_from_slice(txid);
        out.extend_from_slice(&delta.to_le_bytes());
    }
    push_compact(&mut out, dump.unbroadcast.len() as u64);
    for txid in &dump.unbroadcast {
        out.extend_from_slice(txid);
    }
    Ok(out)
}

pub fn decode(bytes: &[u8]) -> Result<CoreMempoolDat, String> {
    if bytes.len() < 8 {
        return Err("mempool.dat is shorter than a version".into());
    }
    let version = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let mut plain = bytes.to_vec();
    let body = if version == 2 {
        let (key_len, used) = read_compact(&bytes[8..])?;
        if key_len != 8 {
            return Err("mempool.dat version 2 key is not 8 bytes".into());
        }
        let key_at = 8 + used;
        if bytes.len() < key_at + 8 {
            return Err("mempool.dat version 2 key is truncated".into());
        }
        let mut key = [0u8; 8];
        key.copy_from_slice(&bytes[key_at..key_at + 8]);
        let start = key_at + 8;
        for (offset, byte) in plain[start..].iter_mut().enumerate() {
            *byte ^= key[(start + offset) % 8];
        }
        start
    } else if version == 1 {
        8
    } else {
        return Err(format!("mempool.dat version {version} is not 1 or 2"));
    };
    parse_body(&plain[body..])
}

fn parse_body(mut data: &[u8]) -> Result<CoreMempoolDat, String> {
    if data.len() < 8 {
        return Err("mempool.dat transaction count is truncated".into());
    }
    let count = u64::from_le_bytes(data[..8].try_into().unwrap()) as usize;
    data = &data[8..];
    let mut txs = Vec::with_capacity(count);
    for _ in 0..count {
        let (tx, witnesses, used) = deserialize_transaction_with_witness(data)
            .map_err(|err| format!("mempool.dat transaction: {err}"))?;
        if data.len() < used + 16 {
            return Err("mempool.dat transaction trailer is truncated".into());
        }
        let time = i64::from_le_bytes(data[used..used + 8].try_into().unwrap());
        let fee_delta = i64::from_le_bytes(data[used + 8..used + 16].try_into().unwrap());
        data = &data[used + 16..];
        let witnesses = if witnesses.iter().any(|stack| !stack.is_empty()) {
            Some(witnesses)
        } else {
            None
        };
        txs.push(CoreMempoolTx {
            tx,
            witnesses,
            time,
            fee_delta,
        });
    }
    let (delta_count, used) = read_compact(data)?;
    data = &data[used..];
    let mut deltas = Vec::with_capacity(delta_count as usize);
    for _ in 0..delta_count {
        if data.len() < 40 {
            return Err("mempool.dat fee delta is truncated".into());
        }
        let mut txid = [0u8; 32];
        txid.copy_from_slice(&data[..32]);
        let delta = i64::from_le_bytes(data[32..40].try_into().unwrap());
        deltas.push((txid, delta));
        data = &data[40..];
    }
    let (unbroadcast_count, used) = read_compact(data)?;
    data = &data[used..];
    let mut unbroadcast = Vec::with_capacity(unbroadcast_count as usize);
    for _ in 0..unbroadcast_count {
        if data.len() < 32 {
            return Err("mempool.dat unbroadcast set is truncated".into());
        }
        let mut txid = [0u8; 32];
        txid.copy_from_slice(&data[..32]);
        unbroadcast.push(txid);
        data = &data[32..];
    }
    if !data.is_empty() {
        return Err("mempool.dat has trailing bytes".into());
    }
    Ok(CoreMempoolDat {
        txs,
        deltas,
        unbroadcast,
    })
}

fn push_compact(out: &mut Vec<u8>, n: u64) {
    if n < 253 {
        out.push(n as u8);
    } else if n <= u64::from(u16::MAX) {
        out.push(253);
        out.extend_from_slice(&(n as u16).to_le_bytes());
    } else if n <= u64::from(u32::MAX) {
        out.push(254);
        out.extend_from_slice(&(n as u32).to_le_bytes());
    } else {
        out.push(255);
        out.extend_from_slice(&n.to_le_bytes());
    }
}

fn read_compact(data: &[u8]) -> Result<(u64, usize), String> {
    let first = *data.first().ok_or("compact size is truncated")?;
    if first < 253 {
        Ok((u64::from(first), 1))
    } else if first == 253 {
        let bytes: [u8; 2] = data
            .get(1..3)
            .ok_or("compact size is truncated")?
            .try_into()
            .unwrap();
        Ok((u64::from(u16::from_le_bytes(bytes)), 3))
    } else if first == 254 {
        let bytes: [u8; 4] = data
            .get(1..5)
            .ok_or("compact size is truncated")?
            .try_into()
            .unwrap();
        Ok((u64::from(u32::from_le_bytes(bytes)), 5))
    } else {
        let bytes: [u8; 8] = data
            .get(1..9)
            .ok_or("compact size is truncated")?
            .try_into()
            .unwrap();
        Ok((u64::from_le_bytes(bytes), 9))
    }
}

#[cfg(test)]
mod tests {
    use super::{CoreMempoolDat, CoreMempoolTx, decode, encode};
    use blvm_protocol::serialization::transaction::deserialize_transaction;

    #[test]
    fn version_one_roundtrip_keeps_the_fee_delta() {
        let raw = hex::decode("01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff4d04ffff001d0104455468652054696d65732030332f4a616e2f32303039204368616e63656c6c6f72206f6e206272696e6b206f66207365636f6e64206261696c6f757420666f722062616e6b73ffffffff0100f2052a01000000434104678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5fac00000000").unwrap();
        let tx = deserialize_transaction(&raw).unwrap();
        let dump = CoreMempoolDat {
            txs: vec![CoreMempoolTx {
                tx,
                witnesses: None,
                time: 1_700_000_000,
                fee_delta: 25,
            }],
            deltas: vec![([7u8; 32], -4)],
            unbroadcast: vec![[8u8; 32]],
        };
        let bytes = encode(&dump).unwrap();
        let loaded = decode(&bytes).unwrap();
        assert_eq!(loaded.txs.len(), 1);
        assert_eq!(loaded.txs[0].fee_delta, 25);
        assert_eq!(loaded.txs[0].time, 1_700_000_000);
        assert_eq!(loaded.deltas, vec![([7u8; 32], -4)]);
        assert_eq!(loaded.unbroadcast, vec![[8u8; 32]]);
    }

    #[test]
    fn version_two_xor_decodes() {
        let mut bytes = encode(&CoreMempoolDat::default()).unwrap();
        bytes[0] = 2;
        let key = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut keyed = bytes[..8].to_vec();
        keyed.push(8);
        keyed.extend_from_slice(&key);
        let start = keyed.len();
        keyed.extend_from_slice(&bytes[8..]);
        for (offset, byte) in keyed[start..].iter_mut().enumerate() {
            *byte ^= key[(start + offset) % 8];
        }
        let loaded = decode(&keyed).unwrap();
        assert!(loaded.txs.is_empty());
    }
}
