//! Bitcoin Core database format parser
//!
//! Parses Bitcoin Core's custom serialization format for coins, block indexes, etc.
//! Bitcoin Core uses custom VarInt encoding and script serialization.

use anyhow::{Context, Result};

/// Bitcoin Core coin (UTXO) structure
#[derive(Debug, Clone)]
pub struct BitcoinCoreCoin {
    pub script: Vec<u8>,
    pub amount: u64,
    pub height: u32,
    pub is_coinbase: bool,
}

/// Bitcoin Core block index entry
#[derive(Debug, Clone)]
pub struct BitcoinCoreBlockIndex {
    pub height: u32,
    pub status: u32,
    pub n_tx: u32,
    pub n_file: u32,
    pub n_data_pos: u32,
    pub n_undo_pos: u32,
    pub n_version: u32,
    pub hash_prev: [u8; 32],
    pub hash_merkle_root: [u8; 32],
    pub n_time: u32,
    pub n_bits: u32,
    pub n_nonce: u32,
}

/// Parse Bitcoin Core's coin format
///
/// Bitcoin Core coin format:
/// - VarInt: code (script type/compression flag)
/// - Script bytes (variable length)
/// - Amount (u64, little-endian)
/// - Height (u32, little-endian)
/// - Coinbase flag (u8, 0 or 1)
pub fn parse_coin(data: &[u8]) -> Result<BitcoinCoreCoin> {
    let mut offset = 0;

    // Read VarInt for code
    let (code, code_len) = read_varint(data, offset)?;
    offset += code_len;

    // Determine script length and compression
    let script_type = (code & 0x1f) as u8;
    let is_compressed = (code & 0x20) != 0;

    // Read script
    let script_len = if script_type == 0 {
        // Uncompressed script - read length as VarInt
        let (len, len_bytes) = read_varint(data, offset)?;
        offset += len_bytes;
        len as usize
    } else {
        // Compressed script - length is determined by script_type
        script_type as usize
    };

    if offset + script_len > data.len() {
        return Err(anyhow::anyhow!("Insufficient data for script"));
    }

    let script = data[offset..offset + script_len].to_vec();
    offset += script_len;

    // Read amount (u64, little-endian)
    if offset + 8 > data.len() {
        return Err(anyhow::anyhow!("Insufficient data for amount"));
    }
    let amount = u64::from_le_bytes(
        data[offset..offset + 8]
            .try_into()
            .context("Failed to read amount")?,
    );
    offset += 8;

    // Read height (u32, little-endian)
    if offset + 4 > data.len() {
        return Err(anyhow::anyhow!("Insufficient data for height"));
    }
    let height = u32::from_le_bytes(
        data[offset..offset + 4]
            .try_into()
            .context("Failed to read height")?,
    );
    offset += 4;

    // Read coinbase flag (u8)
    if offset >= data.len() {
        return Err(anyhow::anyhow!("Insufficient data for coinbase flag"));
    }
    let is_coinbase = data[offset] != 0;

    Ok(BitcoinCoreCoin {
        script,
        amount,
        height,
        is_coinbase,
    })
}

/// Parse Bitcoin Core's block index format
///
/// Bitcoin Core block index is a serialized CBlockIndex structure.
/// This is a simplified parser - the full format is more complex.
pub fn parse_block_index(data: &[u8]) -> Result<BitcoinCoreBlockIndex> {
    if data.len() < 80 {
        return Err(anyhow::anyhow!("Block index data too short"));
    }

    let mut offset = 0;

    // Read height (u32)
    let height = u32::from_le_bytes(
        data[offset..offset + 4]
            .try_into()
            .context("Failed to read height")?,
    );
    offset += 4;

    // Read status (u32)
    let status = u32::from_le_bytes(
        data[offset..offset + 4]
            .try_into()
            .context("Failed to read status")?,
    );
    offset += 4;

    // Read n_tx (u32)
    let n_tx = u32::from_le_bytes(
        data[offset..offset + 4]
            .try_into()
            .context("Failed to read n_tx")?,
    );
    offset += 4;

    // Read n_file (u32)
    let n_file = u32::from_le_bytes(
        data[offset..offset + 4]
            .try_into()
            .context("Failed to read n_file")?,
    );
    offset += 4;

    // Read n_data_pos (u32)
    let n_data_pos = u32::from_le_bytes(
        data[offset..offset + 4]
            .try_into()
            .context("Failed to read n_data_pos")?,
    );
    offset += 4;

    // Read n_undo_pos (u32)
    let n_undo_pos = u32::from_le_bytes(
        data[offset..offset + 4]
            .try_into()
            .context("Failed to read n_undo_pos")?,
    );
    offset += 4;

    // Read n_version (u32)
    let n_version = u32::from_le_bytes(
        data[offset..offset + 4]
            .try_into()
            .context("Failed to read n_version")?,
    );
    offset += 4;

    // Read hash_prev (32 bytes)
    let hash_prev: [u8; 32] = data[offset..offset + 32]
        .try_into()
        .context("Failed to read hash_prev")?;
    offset += 32;

    // Read hash_merkle_root (32 bytes)
    let hash_merkle_root: [u8; 32] = data[offset..offset + 32]
        .try_into()
        .context("Failed to read hash_merkle_root")?;
    offset += 32;

    // Read n_time (u32)
    let n_time = u32::from_le_bytes(
        data[offset..offset + 4]
            .try_into()
            .context("Failed to read n_time")?,
    );
    offset += 4;

    // Read n_bits (u32)
    let n_bits = u32::from_le_bytes(
        data[offset..offset + 4]
            .try_into()
            .context("Failed to read n_bits")?,
    );
    offset += 4;

    // Read n_nonce (u32)
    let n_nonce = u32::from_le_bytes(
        data[offset..offset + 4]
            .try_into()
            .context("Failed to read n_nonce")?,
    );

    Ok(BitcoinCoreBlockIndex {
        height,
        status,
        n_tx,
        n_file,
        n_data_pos,
        n_undo_pos,
        n_version,
        hash_prev,
        hash_merkle_root,
        n_time,
        n_bits,
        n_nonce,
    })
}

/// Convert Bitcoin Core key to BLVM format
///
/// Bitcoin Core uses single-byte prefixes:
/// - 'c' for coins (UTXOs)
/// - 'b' for block index
/// - 'B' for block hash mapping
/// - 'f' for flags
/// - 't' for transaction index
///
/// This function extracts the actual key from the prefixed format.
pub fn convert_key(core_key: &[u8]) -> Result<Vec<u8>> {
    if core_key.is_empty() {
        return Err(anyhow::anyhow!("Empty key"));
    }

    // Return key without the prefix byte
    Ok(core_key[1..].to_vec())
}

/// Get key prefix from Bitcoin Core key
pub fn get_key_prefix(core_key: &[u8]) -> Option<u8> {
    core_key.first().copied()
}

/// Read Bitcoin Core VarInt
///
/// Bitcoin Core uses a custom VarInt encoding:
/// - If value < 0xfd: single byte
/// - If value < 0xffff: 0xfd + 2 bytes (little-endian)
/// - If value < 0xffffffff: 0xfe + 4 bytes (little-endian)
/// - Otherwise: 0xff + 8 bytes (little-endian)
fn read_varint(data: &[u8], offset: usize) -> Result<(u64, usize)> {
    if offset >= data.len() {
        return Err(anyhow::anyhow!("Insufficient data for VarInt"));
    }

    let first_byte = data[offset];

    if first_byte < 0xfd {
        Ok((first_byte as u64, 1))
    } else if first_byte == 0xfd {
        if offset + 3 > data.len() {
            return Err(anyhow::anyhow!("Insufficient data for VarInt (0xfd)"));
        }
        let value = u16::from_le_bytes(
            data[offset + 1..offset + 3]
                .try_into()
                .context("Failed to read VarInt (0xfd)")?,
        ) as u64;
        Ok((value, 3))
    } else if first_byte == 0xfe {
        if offset + 5 > data.len() {
            return Err(anyhow::anyhow!("Insufficient data for VarInt (0xfe)"));
        }
        let value = u32::from_le_bytes(
            data[offset + 1..offset + 5]
                .try_into()
                .context("Failed to read VarInt (0xfe)")?,
        ) as u64;
        Ok((value, 5))
    } else {
        // 0xff
        if offset + 9 > data.len() {
            return Err(anyhow::anyhow!("Insufficient data for VarInt (0xff)"));
        }
        let value = u64::from_le_bytes(
            data[offset + 1..offset + 9]
                .try_into()
                .context("Failed to read VarInt (0xff)")?,
        );
        Ok((value, 9))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_varint_single_byte() {
        let data = [0x42];
        let (value, len) = read_varint(&data, 0).unwrap();
        assert_eq!(value, 0x42);
        assert_eq!(len, 1);
    }

    #[test]
    fn test_read_varint_two_bytes() {
        let data = [0xfd, 0x34, 0x12];
        let (value, len) = read_varint(&data, 0).unwrap();
        assert_eq!(value, 0x1234);
        assert_eq!(len, 3);
    }

    #[test]
    fn test_convert_key() {
        let core_key = b"c\x01\x02\x03";
        let converted = convert_key(core_key).unwrap();
        assert_eq!(converted, b"\x01\x02\x03");
    }

    #[test]
    fn test_get_key_prefix() {
        let core_key = b"c\x01\x02\x03";
        assert_eq!(get_key_prefix(core_key), Some(b'c'));
    }
}

