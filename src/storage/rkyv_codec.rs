//! rkyv-backed storage values for zero-copy reads from mmap'd LMDB pages.
//!
//! Values written with [`encode_utxo`] / [`RkyvUtxo`] can be accessed in-place via
//! [`access_utxo`] without deserializing into owned structures when only fields are needed.
//! Use [`decode_utxo`] when an owned [`UTXO`] is required.

use anyhow::{Context, Result};
use blvm_protocol::types::UTXO;
use rkyv::rancor::Error;
use rkyv::{access, deserialize, to_bytes, Archive, Deserialize, Serialize};

/// rkyv mirror of [`UTXO`] for on-disk / LMDB storage.
#[derive(Archive, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[rkyv(derive(Debug))]
pub struct RkyvUtxo {
    pub value: i64,
    pub script_pubkey: Vec<u8>,
    pub height: u64,
    pub is_coinbase: bool,
}

impl From<&UTXO> for RkyvUtxo {
    fn from(u: &UTXO) -> Self {
        Self {
            value: u.value,
            script_pubkey: u.script_pubkey.to_vec(),
            height: u.height,
            is_coinbase: u.is_coinbase,
        }
    }
}

impl From<RkyvUtxo> for UTXO {
    fn from(r: RkyvUtxo) -> Self {
        Self {
            value: r.value,
            script_pubkey: r.script_pubkey.into(),
            height: r.height,
            is_coinbase: r.is_coinbase,
        }
    }
}

/// Storage value encoding for UTXO rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ValueCodec {
    #[default]
    Bincode,
    Rkyv,
}

impl ValueCodec {
    /// Effective codec: `BLVM_STORAGE_VALUE_CODEC` env, then heed3 default (rkyv), else bincode.
    pub fn effective() -> Self {
        if let Ok(raw) = std::env::var("BLVM_STORAGE_VALUE_CODEC") {
            return match raw.to_ascii_lowercase().as_str() {
                "rkyv" => Self::Rkyv,
                "bincode" => Self::Bincode,
                other => {
                    tracing::warn!("Unknown BLVM_STORAGE_VALUE_CODEC={other:?}; using bincode");
                    Self::Bincode
                }
            };
        }
        #[cfg(feature = "heed3")]
        if std::env::var("BLVM_DATABASE_BACKEND")
            .map(|s| s.eq_ignore_ascii_case("heed3"))
            .unwrap_or(false)
        {
            return Self::Rkyv;
        }
        Self::Bincode
    }
}

/// Serialize a UTXO for disk storage.
pub fn encode_utxo(codec: ValueCodec, utxo: &UTXO) -> Result<Vec<u8>> {
    match codec {
        ValueCodec::Bincode => bincode::serialize(utxo).context("bincode encode UTXO"),
        ValueCodec::Rkyv => {
            let r = RkyvUtxo::from(utxo);
            to_bytes::<Error>(&r)
                .map(|b| b.into_vec())
                .map_err(|e| anyhow::anyhow!("rkyv encode UTXO: {e}"))
        }
    }
}

/// Validated zero-copy access to an archived UTXO in a mmap'd LMDB value buffer.
pub fn access_utxo(bytes: &[u8]) -> Result<&<RkyvUtxo as Archive>::Archived> {
    if bytes.is_empty() {
        return Err(anyhow::anyhow!("rkyv access UTXO: empty buffer"));
    }
    access::<<RkyvUtxo as Archive>::Archived, Error>(bytes)
        .map_err(|e| anyhow::anyhow!("rkyv access UTXO: {e}"))
}

/// Decode a stored UTXO (rkyv or bincode). Tries rkyv first when `codec` is `Rkyv`.
pub fn decode_utxo(codec: ValueCodec, bytes: &[u8]) -> Result<UTXO> {
    match codec {
        ValueCodec::Bincode => bincode::deserialize(bytes).context("bincode decode UTXO"),
        ValueCodec::Rkyv => decode_utxo_rkyv(bytes),
    }
}

/// Decode with auto-detection: validated rkyv access first, bincode fallback for mixed stores.
pub fn decode_utxo_auto(bytes: &[u8]) -> Result<UTXO> {
    if let Ok(archived) = access_utxo(bytes) {
        return deserialize::<RkyvUtxo, Error>(archived)
            .map(Into::into)
            .map_err(|e| anyhow::anyhow!("rkyv deserialize UTXO: {e}"));
    }
    bincode::deserialize(bytes).context("bincode decode UTXO (fallback)")
}

fn decode_utxo_rkyv(bytes: &[u8]) -> Result<UTXO> {
    let archived = access_utxo(bytes)?;
    deserialize::<RkyvUtxo, Error>(archived)
        .map(Into::into)
        .map_err(|e| anyhow::anyhow!("rkyv deserialize UTXO: {e}"))
}

/// Zero-copy field read: build owned UTXO from archived view (copies script only).
pub fn utxo_from_archived(archived: &<RkyvUtxo as Archive>::Archived) -> UTXO {
    UTXO {
        value: archived.value.into(),
        script_pubkey: archived.script_pubkey.to_vec().into(),
        height: archived.height.into(),
        is_coinbase: archived.is_coinbase,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blvm_protocol::types::UTXO;

    fn sample_utxo() -> UTXO {
        UTXO {
            value: 50_000_000,
            script_pubkey: vec![0x76, 0xa9, 0x14].into(),
            height: 100,
            is_coinbase: false,
        }
    }

    #[test]
    fn rkyv_utxo_roundtrip() {
        let u = sample_utxo();
        let bytes = encode_utxo(ValueCodec::Rkyv, &u).unwrap();
        let archived = access_utxo(&bytes).unwrap();
        assert_eq!(archived.value, 50_000_000);
        let decoded = decode_utxo(ValueCodec::Rkyv, &bytes).unwrap();
        assert_eq!(decoded, u);
    }

    #[test]
    fn bincode_still_works() {
        let u = sample_utxo();
        let bytes = encode_utxo(ValueCodec::Bincode, &u).unwrap();
        let decoded = decode_utxo(ValueCodec::Bincode, &bytes).unwrap();
        assert_eq!(decoded, u);
    }

    #[test]
    fn rkyv_rejects_corrupt_bytes() {
        assert!(access_utxo(&[0u8; 4]).is_err());
        assert!(decode_utxo(ValueCodec::Rkyv, &[0xFF, 0xFF, 0xFF, 0xFF]).is_err());
    }
}
