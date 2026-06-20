//! Bitcoin Core database format parser
//!
//! Parses Bitcoin Core chainstate coins (`Coin`), `blocks/index` entries
//! (`CDiskBlockIndex`), and related key encodings.

use anyhow::{Context, Result};
use blvm_protocol::BlockHeader;

/// Block status: data present in blk*.dat (Core `BLOCK_HAVE_DATA`).
pub const BLOCK_HAVE_DATA: u32 = 8;
/// Block status: undo data present (Core `BLOCK_HAVE_UNDO`).
pub const BLOCK_HAVE_UNDO: u32 = 16;

/// Parsed Core UTXO (coin).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitcoinCoreCoin {
    pub script: Vec<u8>,
    pub amount: u64,
    pub height: u32,
    pub is_coinbase: bool,
}

/// Parsed Core disk block index (header + file location).
#[derive(Debug, Clone)]
pub struct BitcoinCoreDiskBlockIndex {
    pub height: u32,
    pub status: u32,
    pub n_tx: u32,
    pub n_file: i32,
    pub n_data_pos: u32,
    pub n_undo_pos: u32,
    pub header: BlockHeader,
    pub block_hash: [u8; 32],
}

/// Legacy coin parser (pre-0.15-style tests only).
pub fn parse_coin(data: &[u8]) -> Result<BitcoinCoreCoin> {
    let mut offset = 0;

    let (code, code_len) = read_compact_size(data, offset)?;
    offset += code_len;

    let script_type = (code & 0x1f) as u8;
    let script_len = if script_type == 0 {
        let (len, len_bytes) = read_compact_size(data, offset)?;
        offset += len_bytes;
        len as usize
    } else {
        script_type as usize
    };

    if offset + script_len > data.len() {
        return Err(anyhow::anyhow!("Insufficient data for script"));
    }

    let script = data[offset..offset + script_len].to_vec();
    offset += script_len;

    if offset + 8 > data.len() {
        return Err(anyhow::anyhow!("Insufficient data for amount"));
    }
    let amount = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
    offset += 8;

    if offset + 4 > data.len() {
        return Err(anyhow::anyhow!("Insufficient data for height"));
    }
    let height = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
    offset += 4;

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

/// Parse modern Core `Coin` value (`Coin::Unserialize` + `TxOutCompression`).
pub fn parse_core_coin(data: &[u8]) -> Result<BitcoinCoreCoin> {
    let (code, len) = read_core_varint(data, 0)?;
    let height = (code >> 1) as u32;
    let is_coinbase = (code & 1) != 0;
    let mut offset = len;

    let (compressed_amount, alen) = read_core_varint(data, offset)?;
    offset += alen;
    let amount = decompress_amount(compressed_amount);

    let (script, slen) = decompress_script_pubkey(data, offset)?;
    offset += slen;
    if offset != data.len() {
        // Trailing bytes can occur on upgraded DBs; tolerate if we parsed the txout.
        tracing::debug!(
            "parse_core_coin: {} trailing bytes after script",
            data.len() - offset
        );
    }

    Ok(BitcoinCoreCoin {
        script,
        amount,
        height,
        is_coinbase,
    })
}

/// Parse Core `CDiskBlockIndex` from `blocks/index` values.
pub fn parse_disk_block_index(data: &[u8]) -> Result<BitcoinCoreDiskBlockIndex> {
    let mut offset = 0;

    let (_version, len) = read_core_varint(data, offset)?;
    offset += len;

    let (height, len) = read_core_varint(data, offset)?;
    offset += len;

    let (status, len) = read_core_varint(data, offset)?;
    offset += len;

    let (n_tx, len) = read_core_varint(data, offset)?;
    offset += len;

    let mut n_file = 0i32;
    if (status as u32) & (BLOCK_HAVE_DATA | BLOCK_HAVE_UNDO) != 0 {
        let (f, len) = read_core_varint(data, offset)?;
        offset += len;
        n_file = f as i32;
    }

    let mut n_data_pos = 0u32;
    if (status as u32) & BLOCK_HAVE_DATA != 0 {
        let (pos, len) = read_core_varint(data, offset)?;
        offset += len;
        n_data_pos = pos as u32;
    }

    let mut n_undo_pos = 0u32;
    if (status as u32) & BLOCK_HAVE_UNDO != 0 {
        let (pos, len) = read_core_varint(data, offset)?;
        offset += len;
        n_undo_pos = pos as u32;
    }

    if offset + 80 > data.len() {
        return Err(anyhow::anyhow!("Block index data too short for header"));
    }

    let n_version = i32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
    offset += 4;

    let mut hash_prev = [0u8; 32];
    hash_prev.copy_from_slice(&data[offset..offset + 32]);
    offset += 32;

    let mut hash_merkle_root = [0u8; 32];
    hash_merkle_root.copy_from_slice(&data[offset..offset + 32]);
    offset += 32;

    let n_time = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
    offset += 4;

    let n_bits = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
    offset += 4;

    let n_nonce = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());

    let header = BlockHeader {
        version: n_version as i64,
        prev_block_hash: hash_prev,
        merkle_root: hash_merkle_root,
        timestamp: n_time as u64,
        bits: n_bits as u64,
        nonce: n_nonce as u64,
    };

    let block_hash = compute_block_header_hash(&header);

    Ok(BitcoinCoreDiskBlockIndex {
        height: height as u32,
        status: status as u32,
        n_tx: n_tx as u32,
        n_file,
        n_data_pos,
        n_undo_pos,
        header,
        block_hash,
    })
}

/// Legacy fixed-layout parser kept for old unit tests.
pub fn parse_block_index(data: &[u8]) -> Result<LegacyBlockIndex> {
    if data.len() < 104 {
        return Err(anyhow::anyhow!("Block index data too short"));
    }

    let height = u32::from_le_bytes(data[0..4].try_into().unwrap());
    let status = u32::from_le_bytes(data[4..8].try_into().unwrap());
    let n_tx = u32::from_le_bytes(data[8..12].try_into().unwrap());
    let n_file = u32::from_le_bytes(data[12..16].try_into().unwrap());
    let n_data_pos = u32::from_le_bytes(data[16..20].try_into().unwrap());
    let n_undo_pos = u32::from_le_bytes(data[20..24].try_into().unwrap());
    let n_version = u32::from_le_bytes(data[24..28].try_into().unwrap());
    let mut hash_prev = [0u8; 32];
    hash_prev.copy_from_slice(&data[28..60]);
    let mut hash_merkle_root = [0u8; 32];
    hash_merkle_root.copy_from_slice(&data[60..92]);
    let n_time = u32::from_le_bytes(data[92..96].try_into().unwrap());
    let n_bits = u32::from_le_bytes(data[96..100].try_into().unwrap());
    let n_nonce = u32::from_le_bytes(data[100..104].try_into().unwrap());

    Ok(LegacyBlockIndex {
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

/// Legacy block index shape (tests / old call sites).
#[derive(Debug, Clone)]
pub struct LegacyBlockIndex {
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

/// Strip a single-byte Core DB prefix from a key.
pub fn convert_key(core_key: &[u8]) -> Result<Vec<u8>> {
    if core_key.is_empty() {
        return Err(anyhow::anyhow!("Empty key"));
    }
    Ok(core_key[1..].to_vec())
}

/// First byte of a Core LevelDB key.
pub fn get_key_prefix(core_key: &[u8]) -> Option<u8> {
    core_key.first().copied()
}

/// Map Core `'C'` / `'c'` coin key → BLVM outpoint key (32-byte txid + 8-byte BE vout).
pub fn core_utxo_key_to_outpoint_key(core_key: &[u8]) -> Result<Vec<u8>> {
    let prefix = core_key
        .first()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("Empty coin key"))?;
    if prefix != b'C' && prefix != b'c' {
        return Err(anyhow::anyhow!("Not a coin key: prefix {}", prefix));
    }
    if core_key.len() < 34 {
        return Err(anyhow::anyhow!("Coin key too short"));
    }
    let txid = &core_key[1..33];
    let (vout, _) = read_core_varint(core_key, 33)?;
    let mut key = Vec::with_capacity(40);
    key.extend_from_slice(txid);
    key.extend_from_slice(&vout.to_be_bytes());
    Ok(key)
}

/// Read Core `DB_BEST_BLOCK` (`'B'`) value — 32-byte block hash.
pub fn parse_best_block_value(value: &[u8]) -> Result<[u8; 32]> {
    if value.len() != 32 {
        return Err(anyhow::anyhow!(
            "Best block value must be 32 bytes, got {}",
            value.len()
        ));
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(value);
    Ok(hash)
}

/// Block index key: `'b'` + 32-byte block hash.
pub fn parse_block_index_key(key: &[u8]) -> Result<[u8; 32]> {
    if key.len() != 33 || key[0] != b'b' {
        return Err(anyhow::anyhow!("Invalid block index key length or prefix"));
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&key[1..33]);
    Ok(hash)
}

/// Core `ReadVarInt` (base-128, used in chainstate and block index).
pub fn read_core_varint(data: &[u8], offset: usize) -> Result<(u64, usize)> {
    let mut n: u64 = 0;
    let mut pos = offset;
    loop {
        if pos >= data.len() {
            return Err(anyhow::anyhow!("Insufficient data for Core VarInt"));
        }
        let ch = data[pos];
        pos += 1;
        if n > (u64::MAX >> 7) {
            return Err(anyhow::anyhow!("Core VarInt overflow"));
        }
        n = (n << 7) | (ch & 0x7F) as u64;
        if ch & 0x80 != 0 {
            if n == u64::MAX {
                return Err(anyhow::anyhow!("Core VarInt overflow"));
            }
            n += 1;
        } else {
            return Ok((n, pos - offset));
        }
    }
}

/// Bitcoin P2P compact-size VarInt (used by legacy test coin parser).
fn read_compact_size(data: &[u8], offset: usize) -> Result<(u64, usize)> {
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
        let value = u16::from_le_bytes(data[offset + 1..offset + 3].try_into().unwrap()) as u64;
        Ok((value, 3))
    } else if first_byte == 0xfe {
        if offset + 5 > data.len() {
            return Err(anyhow::anyhow!("Insufficient data for VarInt (0xfe)"));
        }
        let value = u32::from_le_bytes(data[offset + 1..offset + 5].try_into().unwrap()) as u64;
        Ok((value, 5))
    } else if offset + 9 > data.len() {
        Err(anyhow::anyhow!("Insufficient data for VarInt (0xff)"))
    } else {
        let value = u64::from_le_bytes(data[offset + 1..offset + 9].try_into().unwrap());
        Ok((value, 9))
    }
}

/// Core `DecompressAmount`.
pub fn decompress_amount(x: u64) -> u64 {
    if x == 0 {
        return 0;
    }
    let mut x = x - 1;
    let e = x % 10;
    x /= 10;
    let n = if e < 9 {
        let d = (x % 9) + 1;
        x /= 9;
        x * 10 + d
    } else {
        x + 1
    };
    let mut n = n;
    for _ in 0..e {
        n *= 10;
    }
    n
}

fn decompress_script_pubkey(data: &[u8], offset: usize) -> Result<(Vec<u8>, usize)> {
    const SPECIAL_SCRIPTS: u64 = 6;
    let (n_size, len) = read_core_varint(data, offset)?;
    let mut pos = offset + len;

    if n_size < SPECIAL_SCRIPTS {
        let size = special_script_size(n_size as u32) as usize;
        if pos + size > data.len() {
            return Err(anyhow::anyhow!("Insufficient data for compressed script"));
        }
        let compressed = &data[pos..pos + size];
        pos += size;
        let script = decompress_script(n_size as u32, compressed)?;
        return Ok((script, pos - offset));
    }

    let script_len = (n_size - SPECIAL_SCRIPTS) as usize;
    if script_len > 10_000 {
        return Err(anyhow::anyhow!("Script length {} too large", script_len));
    }
    if pos + script_len > data.len() {
        return Err(anyhow::anyhow!("Insufficient data for script body"));
    }
    let script = data[pos..pos + script_len].to_vec();
    pos += script_len;
    Ok((script, pos - offset))
}

fn special_script_size(n_size: u32) -> u32 {
    match n_size {
        0 | 1 => 20,
        2..=5 => 32,
        _ => 0,
    }
}

fn decompress_script(n_size: u32, compressed: &[u8]) -> Result<Vec<u8>> {
    match n_size {
        0 => {
            if compressed.len() != 20 {
                return Err(anyhow::anyhow!("Invalid P2PKH compressed script size"));
            }
            let mut script = vec![
                0x76, 0xa9, 0x14, // OP_DUP OP_HASH160 20
            ];
            script.extend_from_slice(compressed);
            script.extend_from_slice(&[0x88, 0xac]); // OP_EQUALVERIFY OP_CHECKSIG
            Ok(script)
        }
        1 => {
            if compressed.len() != 20 {
                return Err(anyhow::anyhow!("Invalid P2SH compressed script size"));
            }
            let mut script = vec![0xa9, 0x14]; // OP_HASH160 20
            script.extend_from_slice(compressed);
            script.push(0x87); // OP_EQUAL
            Ok(script)
        }
        2 | 3 => {
            if compressed.len() != 32 {
                return Err(anyhow::anyhow!("Invalid compressed pubkey size"));
            }
            let mut script = vec![0x21, n_size as u8]; // 33-byte push
            script.extend_from_slice(compressed);
            script.push(0xac); // OP_CHECKSIG
            Ok(script)
        }
        4 | 5 => {
            if compressed.len() != 32 {
                return Err(anyhow::anyhow!("Invalid compressed pubkey size"));
            }
            // Uncompressed pubkey recovery omitted — store compressed P2PK form.
            let mut script = vec![0x21, (n_size - 2) as u8];
            script.extend_from_slice(compressed);
            script.push(0xac);
            Ok(script)
        }
        _ => Err(anyhow::anyhow!("Unknown special script type {}", n_size)),
    }
}

fn compute_block_header_hash(header: &BlockHeader) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut buf = [0u8; 80];
    buf[0..4].copy_from_slice(&(header.version as i32).to_le_bytes());
    buf[4..36].copy_from_slice(&header.prev_block_hash);
    buf[36..68].copy_from_slice(&header.merkle_root);
    buf[68..72].copy_from_slice(&(header.timestamp as u32).to_le_bytes());
    buf[72..76].copy_from_slice(&(header.bits as u32).to_le_bytes());
    buf[76..80].copy_from_slice(&(header.nonce as u32).to_le_bytes());
    let first = Sha256::digest(buf);
    let second = Sha256::digest(first);
    let mut out = [0u8; 32];
    out.copy_from_slice(&second);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_core_varint_single() {
        let (v, n) = read_core_varint(&[0x02], 0).unwrap();
        assert_eq!(v, 2);
        assert_eq!(n, 1);
    }

    #[test]
    fn test_read_core_varint_multibyte() {
        // 300 encodes as 0x81 0x2c in Core varint
        let (v, n) = read_core_varint(&[0x81, 0x2c], 0).unwrap();
        assert_eq!(v, 300);
        assert_eq!(n, 2);
    }

    #[test]
    fn test_decompress_amount_zero() {
        assert_eq!(decompress_amount(0), 0);
    }

    #[test]
    fn test_decompress_amount_satoshis() {
        // 1.00 BTC = 100_000_000 sats compresses to 9
        assert_eq!(decompress_amount(9), 100_000_000);
    }

    #[test]
    fn test_parse_core_coin_minimal() {
        // height=1, non-coinbase → code=2; amount 0; empty script
        let data = vec![0x02, 0x00, 0x06];
        let coin = parse_core_coin(&data).unwrap();
        assert_eq!(coin.height, 1);
        assert!(!coin.is_coinbase);
        assert_eq!(coin.amount, 0);
        assert!(coin.script.is_empty());
    }

    #[test]
    fn test_core_utxo_key_to_outpoint_key() {
        let mut key = vec![b'C'];
        key.extend_from_slice(&[0xAB; 32]);
        key.push(0x00); // vout 0 (Core varint)
        let out = core_utxo_key_to_outpoint_key(&key).unwrap();
        assert_eq!(out.len(), 40);
        assert_eq!(&out[0..32], &[0xAB; 32]);
        assert_eq!(u64::from_be_bytes(out[32..40].try_into().unwrap()), 0);
    }

    #[test]
    fn test_parse_disk_block_index_minimal() {
        let mut header = vec![0u8; 80];
        header[0..4].copy_from_slice(&1i32.to_le_bytes()); // version
        header[68..72].copy_from_slice(&1234567890u32.to_le_bytes()); // nTime
        header[72..76].copy_from_slice(&0x1d00ffffu32.to_le_bytes()); // bits
        header[76..80].copy_from_slice(&12345u32.to_le_bytes()); // nonce

        let mut data = vec![
            0x00, // dummy disk version
            0x00, // height 0
            0x0c, // status 12 = HAVE_DATA(8) | VALID_TREE(4) — need file+pos
            0x00, // n_tx 0
            0x00, // n_file 0
            0x00, // n_data_pos 0
        ];
        data.extend_from_slice(&header);

        let idx = parse_disk_block_index(&data).unwrap();
        assert_eq!(idx.height, 0);
        assert_eq!(idx.n_file, 0);
        assert_eq!(idx.header.timestamp, 1234567890);
    }

    #[test]
    fn test_convert_key_and_prefix() {
        let core_key = b"c\x01\x02\x03";
        assert_eq!(get_key_prefix(core_key), Some(b'c'));
        assert_eq!(convert_key(core_key).unwrap(), b"\x01\x02\x03");
    }
}
