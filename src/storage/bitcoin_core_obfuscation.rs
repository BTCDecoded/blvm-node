//! Bitcoin Core LevelDB value obfuscation (`CDBWrapper` XOR key).
//!
//! Chainstate and `blocks/index` values are XOR-scrambled at rest. The key lives
//! under the `\0obfuscate_key` entry (stored unobfuscated, serialized as a vector).

use anyhow::Result;
use std::sync::Arc;

/// LevelDB key for the 8-byte obfuscation secret (`dbwrapper.h`), serialized as a Core string.
pub const CORE_OBFUSCATION_KEY: &[u8] = b"\0obfuscate_key";

const KEY_LEN: usize = 8;

fn core_obfuscation_storage_key() -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + CORE_OBFUSCATION_KEY.len());
    out.push(CORE_OBFUSCATION_KEY.len() as u8);
    out.extend_from_slice(CORE_OBFUSCATION_KEY);
    out
}

/// Core chainstate / block-index XOR key loaded from `\0obfuscate_key`.
#[derive(Clone, Copy, Debug, Default)]
pub struct CoreDbObfuscation {
    key: [u8; KEY_LEN],
}

impl CoreDbObfuscation {
    /// Load obfuscation key from a Core LevelDB (`default` tree).
    pub fn load(db: &Arc<dyn crate::storage::database::Database>) -> Result<Self> {
        let tree = db.open_tree("default")?;
        let storage_key = core_obfuscation_storage_key();
        let Some(raw) = tree.get(&storage_key)? else {
            return Ok(Self::default());
        };
        let key = parse_obfuscation_key_value(&raw).unwrap_or([0u8; KEY_LEN]);
        Ok(Self { key })
    }

    pub fn active(&self) -> bool {
        self.key != [0u8; KEY_LEN]
    }

    /// Deobfuscate a value read from Core LevelDB (in place).
    pub fn deobfuscate_value(&self, entry_key: &[u8], value: &mut [u8]) {
        if !self.active() || value.is_empty() {
            return;
        }
        // The obfuscation key entry itself is never obfuscated.
        if entry_key == CORE_OBFUSCATION_KEY
            || entry_key == core_obfuscation_storage_key().as_slice()
        {
            return;
        }
        for (i, byte) in value.iter_mut().enumerate() {
            *byte ^= self.key[i % KEY_LEN];
        }
    }
}

/// Parse `Obfuscation` / vector-serialized key bytes from a LevelDB value.
fn parse_obfuscation_key_value(raw: &[u8]) -> Option<[u8; KEY_LEN]> {
    if raw.len() == KEY_LEN {
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(raw);
        return Some(key);
    }
    // Bitcoin Core stores the key as `std::vector<uint8_t>` → compact-size + bytes.
    if raw.is_empty() {
        return None;
    }
    let (len, header) = read_compact_size(raw, 0).ok()?;
    if len as usize != KEY_LEN || header + KEY_LEN > raw.len() {
        return None;
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&raw[header..header + KEY_LEN]);
    Some(key)
}

fn read_compact_size(data: &[u8], offset: usize) -> Result<(u64, usize)> {
    if offset >= data.len() {
        anyhow::bail!("compact size: eof");
    }
    let first = data[offset];
    if first < 0xfd {
        return Ok((first as u64, 1));
    }
    if first == 0xfd {
        if offset + 3 > data.len() {
            anyhow::bail!("compact size: short 0xfd");
        }
        let v = u16::from_le_bytes([data[offset + 1], data[offset + 2]]) as u64;
        return Ok((v, 3));
    }
    if first == 0xfe {
        if offset + 5 > data.len() {
            anyhow::bail!("compact size: short 0xfe");
        }
        let v = u32::from_le_bytes([
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
            data[offset + 4],
        ]) as u64;
        return Ok((v, 5));
    }
    if offset + 9 > data.len() {
        anyhow::bail!("compact size: short 0xff");
    }
    let v = u64::from_le_bytes(data[offset + 1..offset + 9].try_into().unwrap());
    Ok((v, 9))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_obfuscation_key_vector_encoding() {
        let mut raw = vec![0x08];
        raw.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            parse_obfuscation_key_value(&raw),
            Some([1, 2, 3, 4, 5, 6, 7, 8])
        );
    }
}
