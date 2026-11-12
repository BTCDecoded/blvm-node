use super::*;
use crate::storage::Storage;
use tempfile::TempDir;

#[test]
fn apply_autorepair_soft_rolls_back_watermark_clears_marker() {
    // Default (no BLVM_IBD_AGGRESSIVE_REPAIR): marker is consumed, ibd_utxos preserved,
    // watermark rolled back one checkpoint interval for incremental replay.
    let _aggressive_guard = AggressiveRepairEnvGuard::cleared();
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path();
    let storage = Storage::new(data_dir).unwrap();

    storage
        .chain()
        .force_set_ibd_utxo_watermark(680_811)
        .unwrap();
    let tree = storage.open_tree("ibd_utxos").unwrap();
    tree.insert(b"tkey", b"tval").unwrap();
    storage.flush().unwrap();

    set_ibd_utxo_repair_flag(data_dir).unwrap();
    assert!(ibd_utxo_repair_flag_present(data_dir));

    apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

    assert!(
        !ibd_utxo_repair_flag_present(data_dir),
        "marker must be removed so later restarts do not loop"
    );
    assert_eq!(
        storage.chain().get_utxo_watermark().unwrap(),
        Some(680_600),
        "soft repair must roll back one 200-block interval"
    );
    assert!(
        !tree.is_empty().unwrap(),
        "soft repair must NOT wipe ibd_utxos — destructive wipe is opt-in via env"
    );
}

#[test]
fn apply_autorepair_aggressive_wipes_state_on_env_flag() {
    let _aggressive_guard = AggressiveRepairEnvGuard::set();
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path();
    let storage = Storage::new(data_dir).unwrap();

    storage.chain().set_utxo_watermark(999).unwrap();
    let tree = storage.open_tree("ibd_utxos").unwrap();
    tree.insert(b"tkey", b"tval").unwrap();
    storage.flush().unwrap();

    set_ibd_utxo_repair_flag(data_dir).unwrap();
    apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

    assert!(!ibd_utxo_repair_flag_present(data_dir));
    assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(0));
    assert!(tree.is_empty().unwrap());
}

/// Lock around autorepair-related env vars so parallel tests do not leak.
struct AutorepairEnvGuard;
impl AutorepairEnvGuard {
    fn cleared() -> crate::ibd_test_lock::Guard {
        let g = crate::ibd_test_lock::guard();
        unsafe {
            std::env::remove_var("BLVM_IBD_AGGRESSIVE_REPAIR");
            std::env::remove_var("BLVM_IBD_DEFER_CHECKPOINT_INTERVAL");
            std::env::set_var("BLVM_IBD_ENGINE", "0");
        }
        g
    }
}

/// Lock around `BLVM_IBD_AGGRESSIVE_REPAIR` so the soft/aggressive tests don't race.
/// Cargo runs tests in parallel by default; an env-set in one test would otherwise leak
/// into the other.
struct AggressiveRepairEnvGuard;
impl AggressiveRepairEnvGuard {
    fn set() -> crate::ibd_test_lock::Guard {
        let g = AutorepairEnvGuard::cleared();
        unsafe {
            std::env::set_var("BLVM_IBD_AGGRESSIVE_REPAIR", "1");
            std::env::set_var("BLVM_IBD_ENGINE", "0");
        }
        g
    }
    fn cleared() -> crate::ibd_test_lock::Guard {
        AutorepairEnvGuard::cleared()
    }
}

#[test]
fn apply_autorepair_no_op_when_marker_missing_preserves_watermark() {
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path();
    let storage = Storage::new(data_dir).unwrap();
    storage.chain().set_utxo_watermark(42).unwrap();

    apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

    assert!(!ibd_utxo_repair_flag_present(data_dir));
    assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(42));
}

#[test]
fn validation_error_suggests_utxo_repair_matching_substrings() {
    assert!(validation_error_suggests_utxo_repair(&anyhow::anyhow!(
        "connect: UTXO not found for input x"
    )));
    assert!(validation_error_suggests_utxo_repair(&anyhow::anyhow!(
        "IBD UTXO mutex poisoned"
    )));
    assert!(!validation_error_suggests_utxo_repair(&anyhow::anyhow!(
        "bad peer disconnect"
    )));
    // av2: the inner miss is real, but the parent block already spent the coin.
    let miss = anyhow::anyhow!(
        "UTXO not found for input 0 (prevout 99a725b5d40b4409e0e658cbf508a0dc6673da27f7b6fbdd3b6f6cc3ba45c5ef:1023 tx_idx=1)"
    );
    let (txid, vout) = utxo_miss_prevout(&miss).unwrap();
    assert_eq!(vout, 1023);
    assert_eq!(
        hex::encode(txid),
        "99a725b5d40b4409e0e658cbf508a0dc6673da27f7b6fbdd3b6f6cc3ba45c5ef"
    );
    let wrapped = Err::<(), _>(miss)
        .context(DOUBLE_SPEND_OF_CONNECTED_BLOCK)
        .unwrap_err();
    assert!(
        !validation_error_suggests_utxo_repair(&wrapped),
        "a double-spend of a connected block must not arm snapshot repair"
    );
}

#[test]
fn block_spends_prevout_matches_the_parent_input() {
    use blvm_protocol::{Block, BlockHeader, OutPoint, Transaction, TransactionInput};
    let txid = [0x99u8; 32];
    let spend = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: txid,
                index: 1023,
            },
            sequence: 0xffff_ffff,
            script_sig: Vec::new().into(),
        }]
        .into(),
        outputs: Vec::new().into(),
        lock_time: 0,
    };
    let block = Block::from_parts(BlockHeader::default(), vec![spend].into());
    assert!(block_spends_prevout(&block, &txid, 1023));
    assert!(!block_spends_prevout(&block, &txid, 1024));
}

#[test]
fn apply_autorepair_soft_wipes_standalone_store_when_watermark_zero() {
    let _aggressive_guard = AggressiveRepairEnvGuard::cleared();
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path();
    let storage = Storage::new(data_dir).unwrap();
    storage.chain().force_set_ibd_utxo_watermark(0).unwrap();

    let utxo_store_dir = data_dir.join(crate::storage::database::IBD_UTXO_STORE_SUBDIR);
    std::fs::create_dir_all(&utxo_store_dir).unwrap();
    std::fs::write(utxo_store_dir.join("marker"), b"stale").unwrap();

    set_ibd_utxo_repair_flag(data_dir).unwrap();
    apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

    assert!(
        !utxo_store_dir.exists(),
        "wm=0 soft repair must wipe standalone ibd_utxo_store"
    );
    assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(0));
}

#[test]
fn apply_autorepair_soft_engine_rolls_back_to_prev_slot_when_available() {
    let _guard = crate::ibd_test_lock::guard();
    unsafe {
        std::env::remove_var("BLVM_IBD_AGGRESSIVE_REPAIR");
        std::env::remove_var("BLVM_IBD_DEFER_CHECKPOINT_INTERVAL");
        std::env::set_var("BLVM_IBD_ENGINE", "1");
    }
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path();
    let storage = Storage::new(data_dir).unwrap();

    // Active slot 0 at 48702 is poisoned (meta count 5581); prev slot 1 at 40000 has a
    // plausible tree (enough rows for height/7).
    storage.chain().force_set_engine_ckpt_slot(0).unwrap();
    storage
        .chain()
        .set_engine_ckpt_slot_height(0, 48_702)
        .unwrap();
    storage
        .chain()
        .set_engine_ckpt_slot_height(1, 40_000)
        .unwrap();
    storage
        .chain()
        .force_set_ibd_utxo_watermark(48_702)
        .unwrap();
    storage
        .chain()
        .force_set_engine_export_height(48_702)
        .unwrap();
    storage
        .chain()
        .force_set_engine_export_utxo_count(5_581)
        .unwrap();
    let ckpt_a = storage.open_tree("ibd_utxos_ckpt_a").unwrap();
    ckpt_a.insert(b"poison", b"x").unwrap();
    let ckpt_b = storage.open_tree("ibd_utxos_ckpt_b").unwrap();
    // Plausible floor at h=40000 is height/7 ≈ 5714 — write enough keys.
    for i in 0..8_000u64 {
        let k = i.to_be_bytes();
        ckpt_b.insert(&k, b"y").unwrap();
    }
    storage.flush().unwrap();

    set_ibd_utxo_repair_flag(data_dir).unwrap();
    apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

    assert!(!ibd_utxo_repair_flag_present(data_dir));
    assert_eq!(storage.chain().get_engine_ckpt_slot().unwrap(), 1);
    assert_eq!(
        storage.chain().get_engine_export_height().unwrap(),
        Some(40_000)
    );
    assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(40_000));
    assert_eq!(
        storage.chain().get_engine_export_utxo_count().unwrap(),
        Some(8_000),
        "rollback must set export_utxo_count from prev tree len"
    );
    assert!(
        !ckpt_b.is_empty().unwrap(),
        "prev-slot rollback must preserve the good tree"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_ENGINE");
    }
}

#[test]
fn apply_autorepair_soft_engine_keeps_healthy_active_slot() {
    // Live 2026-07-14 bug: soft repair preferred prev slot even when active looked fine,
    // abandoning a good 426k snapshot for an incomplete 418k tree.
    let _guard = crate::ibd_test_lock::guard();
    unsafe {
        std::env::remove_var("BLVM_IBD_AGGRESSIVE_REPAIR");
        std::env::remove_var("BLVM_IBD_DEFER_CHECKPOINT_INTERVAL");
        std::env::set_var("BLVM_IBD_ENGINE", "1");
    }
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path();
    let storage = Storage::new(data_dir).unwrap();

    storage.chain().force_set_engine_ckpt_slot(1).unwrap();
    storage
        .chain()
        .set_engine_ckpt_slot_height(1, 42_000)
        .unwrap();
    storage
        .chain()
        .set_engine_ckpt_slot_height(0, 40_000)
        .unwrap();
    storage
        .chain()
        .force_set_ibd_utxo_watermark(42_000)
        .unwrap();
    storage
        .chain()
        .force_set_engine_export_height(42_000)
        .unwrap();
    // Plausible count matching tree len.
    let n = 10_000u64;
    storage
        .chain()
        .force_set_engine_export_utxo_count(n)
        .unwrap();
    let ckpt_b = storage.open_tree("ibd_utxos_ckpt_b").unwrap();
    for i in 0..n {
        let k = i.to_be_bytes();
        ckpt_b.insert(&k, b"y").unwrap();
    }
    let ckpt_a = storage.open_tree("ibd_utxos_ckpt_a").unwrap();
    ckpt_a.insert(b"old", b"x").unwrap();
    storage.flush().unwrap();

    set_ibd_utxo_repair_flag(data_dir).unwrap();
    apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

    assert!(!ibd_utxo_repair_flag_present(data_dir));
    assert_eq!(
        storage.chain().get_engine_ckpt_slot().unwrap(),
        1,
        "healthy active must not roll to prev slot"
    );
    assert_eq!(
        storage.chain().get_engine_export_height().unwrap(),
        Some(42_000)
    );
    let wm = storage.chain().get_utxo_watermark().unwrap().unwrap_or(0);
    assert_eq!(
        wm, 42_000,
        "healthy active must keep watermark=export_h (no below-export rollback), got {wm}"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_ENGINE");
    }
}

#[test]
fn apply_autorepair_soft_engine_raises_watermark_up_to_export() {
    // Live 2026-07-15: wm already below export (prior interval rollback) must clamp UP.
    let _guard = crate::ibd_test_lock::guard();
    unsafe {
        std::env::remove_var("BLVM_IBD_AGGRESSIVE_REPAIR");
        std::env::set_var("BLVM_IBD_ENGINE", "1");
    }
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path();
    let storage = Storage::new(data_dir).unwrap();

    storage.chain().force_set_engine_ckpt_slot(1).unwrap();
    storage
        .chain()
        .set_engine_ckpt_slot_height(1, 42_000)
        .unwrap();
    storage
        .chain()
        .force_set_ibd_utxo_watermark(41_250)
        .unwrap();
    storage
        .chain()
        .force_set_engine_export_height(42_000)
        .unwrap();
    let n = 10_000u64; // plausible for h=42k (need ≥ height/7)
    storage
        .chain()
        .force_set_engine_export_utxo_count(n)
        .unwrap();
    let ckpt_b = storage.open_tree("ibd_utxos_ckpt_b").unwrap();
    for i in 0..n {
        let k = i.to_be_bytes();
        ckpt_b.insert(&k, b"y").unwrap();
    }
    storage.flush().unwrap();

    set_ibd_utxo_repair_flag(data_dir).unwrap();
    apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

    assert!(!ibd_utxo_repair_flag_present(data_dir));
    assert_eq!(
        storage.chain().get_utxo_watermark().unwrap(),
        Some(42_000),
        "must raise watermark to export_h"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_ENGINE");
    }
}

#[test]
fn apply_autorepair_soft_engine_wipes_when_utxo_count_implausible() {
    let _guard = crate::ibd_test_lock::guard();
    unsafe {
        std::env::remove_var("BLVM_IBD_AGGRESSIVE_REPAIR");
        std::env::set_var("BLVM_IBD_ENGINE", "1");
    }
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path();
    let storage = Storage::new(data_dir).unwrap();

    storage.chain().force_set_engine_ckpt_slot(0).unwrap();
    storage
        .chain()
        .set_engine_ckpt_slot_height(0, 48_702)
        .unwrap();
    storage
        .chain()
        .set_engine_ckpt_slot_height(1, 40_000)
        .unwrap();
    storage
        .chain()
        .force_set_ibd_utxo_watermark(48_702)
        .unwrap();
    storage
        .chain()
        .force_set_engine_export_height(48_702)
        .unwrap();
    // Record the live poison count (5581 @ 48k) so soft repair refuses both slots.
    storage
        .chain()
        .force_set_engine_export_utxo_count(5_581)
        .unwrap();
    let ckpt_a = storage.open_tree("ibd_utxos_ckpt_a").unwrap();
    ckpt_a.insert(b"poison", b"x").unwrap();
    let ckpt_b = storage.open_tree("ibd_utxos_ckpt_b").unwrap();
    ckpt_b.insert(b"also_poison", b"y").unwrap();
    storage.flush().unwrap();

    set_ibd_utxo_repair_flag(data_dir).unwrap();
    apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

    assert!(!ibd_utxo_repair_flag_present(data_dir));
    assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(0));
    assert_eq!(storage.chain().get_engine_export_height().unwrap(), Some(0));
    assert!(ckpt_a.is_empty().unwrap());
    assert!(ckpt_b.is_empty().unwrap());
    unsafe {
        std::env::remove_var("BLVM_IBD_ENGINE");
    }
}

#[test]
fn checkpoint_utxo_count_plausible_rejects_live_poison() {
    assert!(!checkpoint_utxo_count_plausible(40_000, 5_581));
    assert!(!checkpoint_utxo_count_plausible(48_702, 5_581));
    assert!(checkpoint_utxo_count_plausible(40_000, 80_000));
    assert!(
        !checkpoint_utxo_count_plausible(40_000, 0),
        "count==0 is refuse, not unset-OK"
    );
    // dest-ak 8.2M@516k passed height/7 (~74k); must be poison.
    assert!(!checkpoint_utxo_count_plausible(516_773, 8_236_260));
    assert!(!checkpoint_utxo_count_monotonic(
        516_773, 8_236_260, 16_800_000
    ));
    assert!(checkpoint_utxo_count_plausible(340_000, 16_800_000));
    // Overlay 3.6M@340k with no predecessor is tee/50%, not height*18.
    // A 100k×18 floor would refuse genesis ~1M@221k and abort the post-fix soak.
    assert!(checkpoint_utxo_count_plausible(221_814, 1_154_316));
    assert!(checkpoint_utxo_count_plausible(340_000, 3_600_000));
    assert!(!checkpoint_utxo_count_monotonic(
        340_000, 3_600_000, 16_800_000
    ));
    assert!(!checkpoint_utxo_count_monotonic(
        200_000, 4_000_000, 16_800_000
    ));
    assert!(checkpoint_first_persist_acceptable(
        16_800_000, 16_800_000, true
    ));
    assert!(!checkpoint_first_persist_acceptable(3_600_000, 0, true));
    assert!(!checkpoint_first_persist_acceptable(
        3_600_000,
        200_000_000,
        true
    ));
    // dest-bc overlay persists vs last good 16.8M — 50% gate, not height/7.
    assert!(!checkpoint_persist_acceptable(
        452_846, 0, 16_800_000, 0, true, false
    ));
    assert!(!checkpoint_persist_acceptable(
        340_000, 3_600_000, 16_800_000, 0, true, false
    ));
    assert!(checkpoint_persist_acceptable(
        340_000, 16_800_000, 16_800_000, 16_800_000, true, false
    ));
    assert!(
        checkpoint_persist_acceptable(221_814, 1_154_316, 0, 1_154_316, true, true),
        "genesis first persist ~1M@221k must not trip height*18"
    );
    assert!(checkpoint_persist_acceptable(
        265_774, 2_822_399, 1_154_316, 2_822_399, true, false
    ));
    assert!(!checkpoint_refuse_hard_stop(2));
    assert!(checkpoint_refuse_hard_stop(3));
    assert!(checkpoint_refuse_hard_stop(4));
}

#[test]
fn apply_autorepair_dest_ak_8m_at_516k_must_not_look_healthy() {
    // dest-ak resumed 8236260 @516773 (passes height/7) then MISSING_UTXO.
    // Trees are gone on disk; this fixture reconstructs the metadata+stub tree.
    // Must not take "looks healthy — align watermark and re-seed".
    let _guard = crate::ibd_test_lock::guard();
    unsafe {
        std::env::remove_var("BLVM_IBD_AGGRESSIVE_REPAIR");
        std::env::remove_var("BLVM_IBD_DEFER_CHECKPOINT_INTERVAL");
        std::env::set_var("BLVM_IBD_ENGINE", "1");
    }
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path();
    let storage = Storage::new(data_dir).unwrap();

    storage.chain().force_set_engine_ckpt_slot(0).unwrap();
    storage
        .chain()
        .set_engine_ckpt_slot_height(0, 516_773)
        .unwrap();
    storage
        .chain()
        .force_set_ibd_utxo_watermark(516_773)
        .unwrap();
    storage
        .chain()
        .force_set_engine_export_height(516_773)
        .unwrap();
    storage
        .chain()
        .force_set_engine_export_utxo_count(8_236_260)
        .unwrap();
    storage
        .chain()
        .force_set_engine_prev_accepted_utxo_count(16_800_000)
        .unwrap();
    let ckpt_a = storage.open_tree("ibd_utxos_ckpt_a").unwrap();
    ckpt_a.insert(b"poison-stub", b"x").unwrap();
    storage.flush().unwrap();

    set_ibd_utxo_repair_flag(data_dir).unwrap();
    apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

    assert!(!ibd_utxo_repair_flag_present(data_dir));
    let export_h = storage
        .chain()
        .get_engine_export_height()
        .unwrap()
        .unwrap_or(0);
    let wm = storage.chain().get_utxo_watermark().unwrap().unwrap_or(0);
    assert_eq!(
        export_h, 0,
        "8.2M@516k must not re-seed; genesis reset, got export_h={export_h}"
    );
    assert_eq!(wm, 0, "must not align watermark to poison 516773, got {wm}");
    unsafe {
        std::env::remove_var("BLVM_IBD_ENGINE");
    }
}

#[test]
fn apply_autorepair_soft_engine_wipes_poisoned_ckpt_when_no_prev_slot() {
    let _guard = crate::ibd_test_lock::guard();
    unsafe {
        std::env::remove_var("BLVM_IBD_AGGRESSIVE_REPAIR");
        std::env::remove_var("BLVM_IBD_DEFER_CHECKPOINT_INTERVAL");
        std::env::set_var("BLVM_IBD_ENGINE", "1");
    }
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path();
    let storage = Storage::new(data_dir).unwrap();

    storage
        .chain()
        .force_set_ibd_utxo_watermark(48_702)
        .unwrap();
    storage
        .chain()
        .force_set_engine_export_height(48_702)
        .unwrap();
    storage
        .chain()
        .persist_engine_validation_tip(5_800)
        .unwrap();
    let ckpt = storage.open_tree("ibd_utxos_ckpt_a").unwrap();
    ckpt.insert(b"ckpt", b"poison").unwrap();
    storage.flush().unwrap();

    set_ibd_utxo_repair_flag(data_dir).unwrap();
    apply_ibd_utxo_autorepair_if_needed(&storage, data_dir).unwrap();

    assert!(!ibd_utxo_repair_flag_present(data_dir));
    assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(0));
    assert_eq!(storage.chain().get_engine_export_height().unwrap(), Some(0));
    assert!(
        ckpt.is_empty().unwrap(),
        "soft engine repair with no prior slot must wipe poisoned ckpt trees"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_ENGINE");
    }
}

#[test]
fn soft_repair_rollback_watermark_one_interval() {
    assert_eq!(soft_repair_rollback_watermark(680_811, 200), 680_600);
    assert_eq!(soft_repair_rollback_watermark(680_600, 200), 680_400);
    assert_eq!(soft_repair_rollback_watermark(200, 200), 0);
    assert_eq!(soft_repair_rollback_watermark(0, 200), 0);
    // On-boundary watermark rolls to the previous boundary.
    assert_eq!(soft_repair_rollback_watermark(680_800, 200), 680_600);
}

#[test]
fn reconcile_resets_watermark_when_ibd_utxos_empty() {
    let dir = TempDir::new().unwrap();
    let storage = Storage::new(dir.path()).unwrap();
    storage
        .chain()
        .force_set_ibd_utxo_watermark(418_000)
        .unwrap();
    assert_eq!(
        reconcile_ibd_utxo_watermark_with_disk(&storage, 418_000).unwrap(),
        0
    );
    assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(0));
}

#[test]
fn reconcile_keeps_watermark_when_ibd_utxos_nonempty() {
    let dir = TempDir::new().unwrap();
    let storage = Storage::new(dir.path()).unwrap();
    storage.chain().force_set_ibd_utxo_watermark(100).unwrap();
    storage
        .open_tree("ibd_utxos")
        .unwrap()
        .insert(b"k", b"v")
        .unwrap();
    assert_eq!(
        reconcile_ibd_utxo_watermark_with_disk(&storage, 100).unwrap(),
        100
    );
}

#[test]
fn reconcile_resets_when_export_label_has_empty_ckpt_and_ibd_utxos_rows() {
    let _guard = crate::ibd_test_lock::guard();
    unsafe {
        std::env::set_var("BLVM_IBD_ENGINE", "1");
    }
    let dir = TempDir::new().unwrap();
    let storage = Storage::new(dir.path()).unwrap();
    storage.chain().force_set_engine_ckpt_slot(0).unwrap();
    storage
        .chain()
        .force_set_engine_export_height(155_499)
        .unwrap();
    storage
        .chain()
        .force_set_ibd_utxo_watermark(155_499)
        .unwrap();
    storage
        .open_tree("ibd_utxos")
        .unwrap()
        .insert(b"k", b"v")
        .unwrap();
    assert!(
        storage
            .open_tree("ibd_utxos_ckpt_a")
            .unwrap()
            .is_empty()
            .unwrap()
    );

    assert_eq!(
        reconcile_ibd_utxo_watermark_with_disk(&storage, 155_499).unwrap(),
        0
    );
    assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(0));
    assert_eq!(storage.chain().get_engine_export_height().unwrap(), Some(0));
    unsafe {
        std::env::remove_var("BLVM_IBD_ENGINE");
    }
}

#[test]
fn reconcile_keeps_watermark_when_phase3_canonical_ibd_utxos_is_set() {
    let _guard = crate::ibd_test_lock::guard();
    unsafe {
        std::env::set_var("BLVM_IBD_ENGINE", "1");
    }
    let dir = TempDir::new().unwrap();
    let storage = Storage::new(dir.path()).unwrap();
    storage.chain().force_set_engine_ckpt_slot(0).unwrap();
    storage
        .chain()
        .force_set_engine_export_height(155_512)
        .unwrap();
    storage
        .chain()
        .force_set_ibd_utxo_watermark(155_512)
        .unwrap();
    storage
        .chain()
        .set_ibd_utxo_canonical_tree("ibd_utxos")
        .unwrap();
    storage
        .open_tree("ibd_utxos")
        .unwrap()
        .insert(b"k", b"v")
        .unwrap();
    assert!(
        storage
            .open_tree("ibd_utxos_ckpt_a")
            .unwrap()
            .is_empty()
            .unwrap()
    );

    assert_eq!(
        reconcile_ibd_utxo_watermark_with_disk(&storage, 155_512).unwrap(),
        155_512
    );
    assert_eq!(
        storage.chain().get_engine_export_height().unwrap(),
        Some(155_512)
    );
    let snap = storage.open_engine_resume_snapshot().unwrap();
    let (name, tree) = snap.expect("canonical snapshot");
    assert_eq!(name, "ibd_utxos");
    assert!(!tree.is_empty().unwrap());
    unsafe {
        std::env::remove_var("BLVM_IBD_ENGINE");
    }
}

#[test]
fn resume_snapshot_name_ignores_default_ibd_utxos() {
    assert_eq!(
        crate::storage::engine_resume_snapshot_name("ibd_utxos_ckpt_a", true, None, false),
        Some("ibd_utxos_ckpt_a")
    );
    assert_eq!(
        crate::storage::engine_resume_snapshot_name("ibd_utxos_ckpt_a", false, None, true),
        None
    );
    assert_eq!(
        crate::storage::engine_resume_snapshot_name(
            "ibd_utxos_ckpt_a",
            false,
            Some("ibd_utxos"),
            true
        ),
        Some("ibd_utxos")
    );
    assert_eq!(
        crate::storage::engine_resume_snapshot_name(
            "ibd_utxos_ckpt_a",
            false,
            Some("ibd_utxos_ckpt_a"),
            true
        ),
        None
    );
    assert_eq!(
        crate::storage::engine_resume_snapshot_name(
            "ibd_utxos_ckpt_a",
            false,
            Some("ibd_utxos"),
            false
        ),
        None
    );
}
