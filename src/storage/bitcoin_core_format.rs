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

/// One unspent output from a Bitcoin Core 0.14 `CCoins` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyOutput {
    pub vout: u32,
    pub coin: BitcoinCoreCoin,
}

/// Read a 0.14 `CCoins` value (`CCoins::Unserialize` in Bitcoin Core 0.14.2).
///
/// The key is `c` plus the txid. The vout is the availability index, not a
/// suffix of the key. An empty value is an error.
pub fn parse_coin(data: &[u8]) -> Result<Vec<LegacyOutput>> {
    let mut offset = 0;
    let (_version, n) = read_core_varint(data, offset)?;
    offset += n;
    let (n_code, n) = read_core_varint(data, offset)?;
    offset += n;
    let is_coinbase = (n_code & 1) != 0;
    let mut available = vec![(n_code & 2) != 0, (n_code & 4) != 0];
    let mut mask_code = n_code / 8 + u64::from((n_code & 6) == 0);
    while mask_code > 0 {
        if offset >= data.len() {
            return Err(anyhow::anyhow!("Insufficient data for Core VarInt"));
        }
        let byte = data[offset];
        offset += 1;
        for bit in 0..8 {
            available.push((byte & (1 << bit)) != 0);
        }
        if byte != 0 {
            mask_code -= 1;
        }
    }
    let mut outputs = Vec::new();
    for (vout, is_available) in available.into_iter().enumerate() {
        if !is_available {
            continue;
        }
        let (compressed_amount, n) = read_core_varint(data, offset)?;
        offset += n;
        let amount = decompress_amount(compressed_amount)?;
        let (script, n) = decompress_script_pubkey(data, offset)?;
        offset += n;
        outputs.push(LegacyOutput {
            vout: vout as u32,
            coin: BitcoinCoreCoin {
                script,
                amount,
                height: 0,
                is_coinbase,
            },
        });
    }
    let (height, n) = read_core_varint(data, offset)?;
    offset += n;
    if height > u32::MAX as u64 {
        return Err(anyhow::anyhow!("coin height does not fit in u32"));
    }
    if offset != data.len() {
        return Err(anyhow::anyhow!(
            "legacy coin has {} trailing bytes",
            data.len() - offset
        ));
    }
    for output in &mut outputs {
        output.coin.height = height as u32;
    }
    Ok(outputs)
}

/// 40-byte outpoint key for one output of a 0.14 coin.
pub fn legacy_outpoint_key(txid: &[u8; 32], vout: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(40);
    key.extend_from_slice(txid);
    key.extend_from_slice(&u64::from(vout).to_be_bytes());
    key
}

/// Parse modern Core `Coin` value (`Coin::Unserialize` + `TxOutCompression`).
pub fn parse_core_coin(data: &[u8]) -> Result<BitcoinCoreCoin> {
    let (code, len) = read_core_varint(data, 0)?;
    let height_code = code >> 1;
    if height_code > u32::MAX as u64 {
        return Err(anyhow::anyhow!("coin height does not fit in u32"));
    }
    let height = height_code as u32;
    let is_coinbase = (code & 1) != 0;
    let mut offset = len;

    let (compressed_amount, alen) = read_core_varint(data, offset)?;
    offset += alen;
    let amount = decompress_amount(compressed_amount)?;

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
    if prefix == b'c' && core_key.len() < 34 {
        return Err(anyhow::anyhow!("pre-0.15 coin key is not imported"));
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

/// Core `DecompressAmount`. The result must fit in a signed Bitcoin amount.
pub fn decompress_amount(x: u64) -> Result<u64> {
    if x == 0 {
        return Ok(0);
    }
    let mut x = x - 1;
    let e = x % 10;
    x /= 10;
    let mut n = if e < 9 {
        let d = (x % 9) + 1;
        x /= 9;
        x * 10 + d
    } else {
        x + 1
    };
    for _ in 0..e {
        n = n
            .checked_mul(10)
            .ok_or_else(|| anyhow::anyhow!("decompressed amount does not fit in u64"))?;
    }
    if n > i64::MAX as u64 {
        return Err(anyhow::anyhow!("decompressed amount does not fit in i64"));
    }
    Ok(n)
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
            let mut compressed_key = [0u8; 33];
            compressed_key[0] = (n_size - 2) as u8;
            compressed_key[1..].copy_from_slice(compressed);
            let point = blvm_secp256k1::ecdsa::ge_from_compressed(&compressed_key)
                .ok_or_else(|| anyhow::anyhow!("uncompressed pubkey does not decompress"))?;
            let full = blvm_secp256k1::ecdsa::ge_to_uncompressed(&point);
            let mut script = Vec::with_capacity(67);
            script.push(65);
            script.extend_from_slice(&full);
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
        assert_eq!(decompress_amount(0).unwrap(), 0);
    }

    #[test]
    fn test_decompress_amount_satoshis() {
        // 1.00 BTC = 100_000_000 sats compresses to 9
        assert_eq!(decompress_amount(9).unwrap(), 100_000_000);
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

    #[test]
    fn height_code_past_u32_is_rejected() {
        let normal = [0x80, 0x48, 0x00, 0x06];
        let coin = parse_core_coin(&normal).unwrap();
        assert_eq!(coin.height, 100);
        assert!(!coin.is_coinbase);
        let wide = [0x9e, 0xfe, 0xfe, 0xff, 0x00, 0x00, 0x06];
        assert!(parse_core_coin(&wide).is_err());
    }

    #[test]
    fn decompressed_amount_must_fit_in_i64() {
        assert_eq!(decompress_amount(9).unwrap(), 100_000_000);
        assert!(decompress_amount(1000000000000).is_err());
        assert!(decompress_amount(184467440730).is_err());
    }

    #[test]
    fn special_script_4_is_uncompressed_p2pk() {
        let mut x = [0u8; 32];
        x.copy_from_slice(&hex_decode(
            "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
        ));
        let script = decompress_script(4, &x).unwrap();
        assert_eq!(script.len(), 67);
        assert_eq!(script[0], 65);
        assert_eq!(script[1], 0x04);
        assert_eq!(&script[2..34], &x);
        assert_eq!(script[66], 0xac);
    }

    #[test]
    fn legacy_p2pkh_coin_is_vout_zero() {
        let mut value = vec![0x01, 0x02, 0x00, 0x00];
        value.extend_from_slice(&[0x11; 20]);
        value.push(0x64);
        let outputs = parse_coin(&value).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].vout, 0);
        assert_eq!(outputs[0].coin.amount, 0);
        assert_eq!(outputs[0].coin.height, 100);
        assert!(!outputs[0].coin.is_coinbase);
        assert_eq!(outputs[0].coin.script.len(), 25);
        assert_eq!(&outputs[0].coin.script[..3], &[0x76, 0xa9, 0x14]);
        assert_eq!(&outputs[0].coin.script[3..23], &[0x11; 20]);
        assert_eq!(&outputs[0].coin.script[23..], &[0x88, 0xac]);
        assert!(parse_coin(&[]).is_err());

        let mut later = vec![0x01, 0x00, 0x01, 0x00, 0x00];
        later.extend_from_slice(&[0x22; 20]);
        later.push(0x64);
        let outputs = parse_coin(&later).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].vout, 2);
        assert_eq!(outputs[0].coin.height, 100);
    }

    fn hex_decode(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }
}
