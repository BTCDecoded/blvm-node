//! Incremental MuHash3072 over the `ibd_utxos` tree during parallel IBD.

use crate::storage::Storage;
use crate::storage::chainstate::ChainState;
use crate::storage::disk_utxo::key_to_outpoint;
use anyhow::{Context, Result};
use blvm_muhash::{MuHash3072, serialize_coin_for_muhash};
use blvm_protocol::types::UTXO;

pub(crate) fn load_ibd_muhash_from_chain(chain: &ChainState) -> Result<MuHash3072> {
    Ok(match chain.get_ibd_utxo_muhash_running()? {
        Some(bytes) => MuHash3072::deserialize_running_state(&bytes),
        None => MuHash3072::new(),
    })
}

/// Reset rolling MuHash bytes to the last completed engine checkpoint snapshot before gap replay.
pub(crate) fn reset_engine_resume_muhash_baseline(
    chain: &ChainState,
    export_height: u64,
) -> Result<()> {
    if export_height == 0 {
        return Ok(());
    }
    if let Some(bytes) = chain.get_engine_export_muhash()? {
        chain
            .persist_ibd_utxo_muhash_running_only(&bytes)
            .with_context(|| {
                format!("reset MuHash baseline to export snapshot at height {export_height}")
            })?;
        tracing::info!(
            "IBD engine: MuHash baseline reset to export snapshot at height {export_height}"
        );
        return Ok(());
    }
    if let Some(tip) = chain.get_engine_validation_tip()? {
        if tip > export_height {
            tracing::warn!(
                "IBD engine: export MuHash snapshot missing at height {export_height}; \
                 validation_tip={tip} is ahead — run backfill or wait for next checkpoint export"
            );
        }
    }
    Ok(())
}

/// One-shot backfill: scan the active engine checkpoint tree and persist `ibd_engine_export_muhash`.
///
/// Safe to run offline against the data directory (node stopped). Idempotent when snapshot exists.
pub fn backfill_engine_export_muhash_if_missing(storage: &Storage) -> Result<bool> {
    use crate::storage::ibd_engine::ckpt_tree_for_slot;
    use crate::storage::utxo_value_codec::decode_utxo_with_codec;

    let chain = storage.chain();
    let export_height = match chain.get_engine_export_height()? {
        Some(h) if h > 0 => h,
        _ => return Ok(false),
    };
    if chain.get_engine_export_muhash()?.is_some() {
        return Ok(false);
    }

    let slot = chain.get_engine_ckpt_slot()?;
    let tree_name = ckpt_tree_for_slot(slot);
    let tree = storage
        .open_tree(tree_name)
        .with_context(|| format!("open checkpoint tree {tree_name}"))?;
    if tree.is_empty().unwrap_or(true) {
        anyhow::bail!(
            "cannot backfill export MuHash: {tree_name} empty at export_height={export_height}"
        );
    }

    let codec = storage.utxo_value_codec();
    let t0 = std::time::Instant::now();
    let mut mh = MuHash3072::new();
    let mut count = 0usize;
    for kv in tree.iter() {
        let (key_bytes, val_bytes) = kv?;
        if key_bytes.len() != 40 {
            continue;
        }
        let mut op_key = [0u8; 40];
        op_key.copy_from_slice(&key_bytes);
        let op = key_to_outpoint(&op_key);
        let utxo: UTXO = decode_utxo_with_codec(codec, &val_bytes)?;
        mh.insert_mut(&utxo_muhash_preimage(&op, &utxo));
        count += 1;
        if count % 5_000_000 == 0 {
            tracing::info!(
                "IBD engine MuHash backfill: scanned {} UTXOs from {} ({:.1}s)",
                count,
                tree_name,
                t0.elapsed().as_secs_f64()
            );
        }
    }

    let bytes = mh.serialize_running_state();
    chain.persist_engine_export_muhash_snapshot(&bytes)?;
    chain.persist_ibd_utxo_muhash_running_only(&bytes)?;
    storage.flush()?;

    tracing::info!(
        "IBD engine: backfilled export MuHash from {} at h={} ({} UTXOs, {:.1}s)",
        tree_name,
        export_height,
        count,
        t0.elapsed().as_secs_f64()
    );
    Ok(true)
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

/// Fold one validated block's UTXO delta into a running MuHash (engine IBD hot path).
pub(crate) fn fold_block_utxo_delta_muhash(
    delta: &blvm_protocol::block::UtxoDelta,
    spent_inputs: &blvm_protocol::UtxoSet,
    acc: &mut MuHash3072,
) {
    for (op, utxo) in delta.additions.iter() {
        acc.insert_mut(&utxo_muhash_preimage(op, utxo.as_ref()));
    }
    for dk in delta.deletions.iter() {
        let op = blvm_protocol::utxo_overlay::utxo_deletion_key_to_outpoint(dk);
        if let Some(utxo) = spent_inputs.get(&op) {
            acc.remove_mut(&utxo_muhash_preimage(&op, utxo.as_ref()));
        }
    }
}

/// Engine IBD: fold MuHash from block outputs + spends without building overlay `UtxoDelta`.
pub(crate) fn fold_block_engine_muhash(
    block: &blvm_protocol::Block,
    tx_ids: &[[u8; 32]],
    height: u64,
    session: &crate::storage::ibd_engine::SpendSession,
    acc: &mut MuHash3072,
) {
    use crate::storage::ibd_engine::types::outpoint_to_output_key;
    use blvm_protocol::transaction::is_coinbase;

    for (ti, tx) in block.transactions.iter().enumerate() {
        if ti >= tx_ids.len() {
            break;
        }
        let tx_id = tx_ids[ti];
        let is_cb = is_coinbase(tx);
        for (oi, out) in tx.outputs.iter().enumerate() {
            let op = blvm_protocol::types::OutPoint {
                hash: tx_id,
                index: oi as u32,
            };
            let utxo = UTXO {
                value: out.value,
                script_pubkey: out.script_pubkey.clone().into(),
                height,
                is_coinbase: is_cb,
            };
            acc.insert_mut(&utxo_muhash_preimage(&op, &utxo));
        }
    }

    for tx in block.transactions.iter().skip(1) {
        for input in &tx.inputs {
            let op = input.prevout;
            let key = outpoint_to_output_key(&op);
            let utxo_ref = session
                .key_to_idx
                .get(&key)
                .map(|&idx| session.details[idx].utxo.as_ref())
                .or_else(|| session.local_spends.get(&key).map(|d| d.utxo.as_ref()));
            if let Some(u) = utxo_ref {
                acc.remove_mut(&utxo_muhash_preimage(&op, u));
            }
        }
    }
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

    // Open the IBD UTXO tree from the standalone LMDB when it exists, otherwise fall back to
    // the main storage canonical tree (may be a ckpt after Phase 3 promote).
    let _standalone_db: Option<Box<dyn crate::storage::database::Database>>;
    let tree: std::sync::Arc<dyn crate::storage::database::Tree> = if let Some(root) =
        storage.data_dir()
    {
        let utxo_store_dir = root.join(crate::storage::database::IBD_UTXO_STORE_SUBDIR);
        if utxo_store_dir.exists() {
            match crate::storage::database::create_ibd_utxo_standalone_db(&utxo_store_dir) {
                Ok(db) => match db.open_tree("ibd_utxos") {
                    Ok(t) => {
                        _standalone_db = Some(db);
                        std::sync::Arc::from(t)
                    }
                    Err(e) => {
                        tracing::warn!(
                            "MuHash verify: standalone ibd_utxo open_tree failed ({e}); \
                                 using main storage canonical"
                        );
                        _standalone_db = None;
                        storage.open_ibd_utxo_tree()?
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        "MuHash verify: standalone LMDB open failed ({e}); using main storage canonical"
                    );
                    _standalone_db = None;
                    storage.open_ibd_utxo_tree()?
                }
            }
        } else {
            _standalone_db = None;
            storage.open_ibd_utxo_tree()?
        }
    } else {
        _standalone_db = None;
        storage.open_ibd_utxo_tree()?
    };
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
    use crate::storage::Storage;
    use crate::storage::database::DatabaseBackend;
    use crate::storage::disk_utxo::outpoint_to_key;
    use crate::storage::utxo_value_codec::{ValueCodec, encode_utxo_with_codec};
    use blvm_muhash::{MUHASH_RUNNING_STATE_BYTES, MuHash3072};
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
