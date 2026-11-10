//! UTXO on-disk encoding: bincode (default) or rkyv (heed3 / LMDB mmap zero-copy reads).

use anyhow::{Context, Result};
use blvm_protocol::types::UTXO;

/// How UTXO rows are serialized in storage trees (`utxos`, `ibd_utxos`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ValueCodec {
    #[default]
    Bincode,
    Rkyv,
}

impl ValueCodec {
    /// Resolve codec for an open [`Database`](crate::storage::database::Database).
    pub fn for_database(db: &dyn crate::storage::database::Database) -> Self {
        #[cfg(feature = "heed3")]
        {
            use crate::storage::database::Heed3Database;
            if db.as_any().downcast_ref::<Heed3Database>().is_some() {
                return Self::Rkyv;
            }
        }
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
        Self::Bincode
    }
}

/// Serialize a UTXO for storage.
pub fn encode_utxo_with_codec(codec: ValueCodec, utxo: &UTXO) -> Result<Vec<u8>> {
    match codec {
        ValueCodec::Bincode => bincode::serialize(utxo).context("bincode encode UTXO"),
        ValueCodec::Rkyv => {
            #[cfg(feature = "heed3")]
            {
                crate::storage::rkyv_codec::encode_utxo(
                    crate::storage::rkyv_codec::ValueCodec::Rkyv,
                    utxo,
                )
            }
            #[cfg(not(feature = "heed3"))]
            {
                Err(anyhow::anyhow!(
                    "rkyv UTXO encoding requires the heed3 feature"
                ))
            }
        }
    }
}

/// Deserialize a stored UTXO row.
pub fn decode_utxo_with_codec(codec: ValueCodec, bytes: &[u8]) -> Result<UTXO> {
    match codec {
        ValueCodec::Bincode => bincode::deserialize(bytes).context("bincode decode UTXO"),
        ValueCodec::Rkyv => {
            #[cfg(feature = "heed3")]
            {
                crate::storage::rkyv_codec::decode_utxo_auto(bytes)
            }
            #[cfg(not(feature = "heed3"))]
            {
                Err(anyhow::anyhow!(
                    "rkyv UTXO decoding requires the heed3 feature"
                ))
            }
        }
    }
}

/// Encode using the codec implied by `db`.
pub fn encode_utxo(db: &dyn crate::storage::database::Database, utxo: &UTXO) -> Result<Vec<u8>> {
    encode_utxo_with_codec(ValueCodec::for_database(db), utxo)
}

/// Decode using the codec implied by `db`.
pub fn decode_utxo(db: &dyn crate::storage::database::Database, bytes: &[u8]) -> Result<UTXO> {
    decode_utxo_with_codec(ValueCodec::for_database(db), bytes)
}
