//! Incremental MuHash3072 over the `ibd_utxos` tree during parallel IBD.

use crate::storage::chainstate::ChainState;
use crate::storage::disk_utxo::key_to_outpoint;
use crate::storage::Storage;
use anyhow::{Context, Result};
use blvm_muhash::{serialize_coin_for_muhash, MuHash3072};
use blvm_protocol::types::UTXO;

pub(crate) fn load_ibd_muhash_from_chain(chain: &ChainState) -> Result<MuHash3072> {
    Ok(match chain.get_ibd_utxo_muhash_running()? {
        Some(bytes) => MuHash3072::deserialize_running_state(&bytes),
        None => MuHash3072::new(),
    })
}

#[inline]
fn utxo_muhash_preimage(op: &blvm_protocol::types::OutPoint, utxo: &UTXO) -> Vec<u8> {
    serialize_coin_for_muhash(
        &op.hash,
        op.index,
        utxo.height as u32,
        utxo.is_coinbase,
        utxo.value,
        utxo.script_pubkey.as_ref(),
    )
}

/// Full-tree scan vs persisted rolling MuHash (optional integrity check).
pub(crate) fn verify_ibd_utxo_muhash_startup(storage: &Storage) -> Result<()> {
    let chain = storage.chain();
    let Some(bytes) = chain.get_ibd_utxo_muhash_running()? else {
        tracing::warn!(
            "BLVM_VERIFY_IBD_UTXO_MUHASH: no ibd_utxo_muhash_running in chain_info — skipping verify \
             (legacy DB or before first MuHash checkpoint)"
        );
        return Ok(());
    };

    let tree = storage.open_tree("ibd_utxos")?;
    let mut scan = MuHash3072::new();

    // heed3 fast path: scan_heed3 streams (k, v) slices from mmap'd LMDB pages —
    // no Vec<u8> allocation per entry for either key or value bytes.
    // Falls through to the iter path if the tree is not a heed3 instance.
    #[cfg(feature = "heed3")]
    if let Some(heed3_tree) = tree.as_heed3_tree() {
        heed3_tree.scan_heed3(|k, v| {
            if k.len() != 40 {
                return Ok(());
            }
            let mut key = [0u8; 40];
            key.copy_from_slice(&k[..40]);
            let op = key_to_outpoint(&key);
            let pre = if let Ok(archived) = crate::storage::rkyv_codec::access_utxo(v) {
                serialize_coin_for_muhash(
                    &op.hash,
                    op.index,
                    u64::from(archived.height) as u32,
                    archived.is_coinbase,
                    archived.value.into(),
                    archived.script_pubkey.as_slice(),
                )
            } else {
                // Bincode fallback for rows written before rkyv migration.
                let utxo: UTXO = bincode::deserialize(v)
                    .with_context(|| format!("decode ibd_utxos row {:?}", &k[..8]))?;
                utxo_muhash_preimage(&op, &utxo)
            };
            scan.insert_mut(&pre);
            Ok(())
        })?;
    } else {
        // heed3 enabled but this tree is not a Heed3Tree (should not normally happen).
        for row in tree.iter() {
            let (k, v) = row?;
            if k.len() != 40 {
                continue;
            }
            let mut key = [0u8; 40];
            key.copy_from_slice(&k[..40]);
            let op = key_to_outpoint(&key);
            let pre = if let Ok(archived) = crate::storage::rkyv_codec::access_utxo(&v) {
                serialize_coin_for_muhash(
                    &op.hash,
                    op.index,
                    u64::from(archived.height) as u32,
                    archived.is_coinbase,
                    archived.value.into(),
                    archived.script_pubkey.as_slice(),
                )
            } else {
                let utxo: UTXO = bincode::deserialize(&v)
                    .with_context(|| format!("decode ibd_utxos row {:?}", &k[..8]))?;
                utxo_muhash_preimage(&op, &utxo)
            };
            scan.insert_mut(&pre);
        }
    }

    // Non-heed3 build: all rows are bincode; use the owned-bytes iter path directly.
    #[cfg(not(feature = "heed3"))]
    for row in tree.iter() {
        let (k, v) = row?;
        if k.len() != 40 {
            continue;
        }
        let mut key = [0u8; 40];
        key.copy_from_slice(&k[..40]);
        let op = key_to_outpoint(&key);
        let utxo: UTXO = bincode::deserialize(&v)
            .with_context(|| format!("decode ibd_utxos row {:?}", &k[..8]))?;
        let pre = utxo_muhash_preimage(&op, &utxo);
        scan.insert_mut(&pre);
    }

    let expected = scan.finalize();
    let got = MuHash3072::deserialize_running_state(&bytes).finalize();
    if expected != got {
        anyhow::bail!(
            "IBD UTXO MuHash verify failed: full-tree scan finalized {:02x?} != persisted running state finalized {:02x?}",
            &expected[..],
            &got[..]
        );
    }
    tracing::info!("BLVM_VERIFY_IBD_UTXO_MUHASH: ibd_utxos MuHash OK");
    Ok(())
}

#[cfg(all(test, feature = "heed3"))]
mod verify_tests {
    use super::*;
    use crate::storage::database::DatabaseBackend;
    use crate::storage::disk_utxo::outpoint_to_key;
    use crate::storage::utxo_value_codec::{encode_utxo_with_codec, ValueCodec};
    use crate::storage::Storage;
    use blvm_muhash::{MuHash3072, MUHASH_RUNNING_STATE_BYTES};
    use blvm_protocol::types::{OutPoint, UTXO};
    use tempfile::TempDir;

    #[test]
    fn verify_ibd_utxo_muhash_startup_heed3_scan() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::with_backend(temp_dir.path(), DatabaseBackend::Heed3).unwrap();
        let tree = storage.open_tree("ibd_utxos").unwrap();

        let mut rolling = MuHash3072::new();
        for i in 0..32u64 {
            let op = OutPoint {
                hash: [i as u8; 32],
                index: 0,
            };
            let utxo = UTXO {
                value: 10_000 + i as i64,
                script_pubkey: vec![0x51].into(),
                height: i,
                is_coinbase: i == 0,
            };
            let key = outpoint_to_key(&op);
            tree.insert(
                &key,
                &encode_utxo_with_codec(ValueCodec::Rkyv, &utxo).unwrap(),
            )
            .unwrap();
            rolling.insert_mut(&utxo_muhash_preimage(&op, &utxo));
        }

        let running: [u8; MUHASH_RUNNING_STATE_BYTES] = rolling.serialize_running_state();
        storage
            .chain()
            .persist_ibd_utxo_flush_checkpoint(31, &running)
            .unwrap();

        verify_ibd_utxo_muhash_startup(&storage).expect("heed3 scan_heed3 MuHash verify");
    }
}
