use super::super::types::{OutputId, OutputKV};
use super::*;
use std::sync::{Mutex, MutexGuard};

fn hot_pin_env_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[serial_test::serial(ibd)]
#[test]
fn disk_fan_in_env_default_and_clamp() {
    // SAFETY: single-threaded test; env restored before exit.
    unsafe {
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        assert_eq!(disk_fan_in_from_env(), 8);
        std::env::set_var("BLVM_IBD_DISK_FAN_IN", "4");
        assert_eq!(disk_fan_in_from_env(), 4);
        std::env::set_var("BLVM_IBD_DISK_FAN_IN", "1");
        assert_eq!(disk_fan_in_from_env(), 2);
        std::env::set_var("BLVM_IBD_DISK_FAN_IN", "99");
        assert_eq!(disk_fan_in_from_env(), 32);
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
    }
}

/// dest-bc after M0+M6 fold: 20M chunks at front, remaining megas. Next pair
/// must eat a mega, not re-merge chunk 0 with the newest mega.
#[test]
fn dest_bc_stall_pair_indices_eats_mega_not_front_chunk() {
    let counts = [4usize, 4, 4, 9, 9, 9, 9, 9];
    assert_eq!(stall_pair_indices(&counts, 4), Some((3, 7)));
}

/// dest-bc 144G: old Adds in a tiny front seg, Deletes in the newest mega.
#[test]
fn dest_bc_stall_pair_indices_oldest_add_plus_newest_mega() {
    let counts = [1usize, 1, 1, 9];
    assert_eq!(stall_pair_indices(&counts, 4), Some((0, 3)));
}

/// dest-bc Deletes live in later megas, not the newest 20M spill. Pair both
/// megas — `(1, 3)` would merge a mega with a chunk and leave the spend.
#[test]
fn dest_bc_stall_pair_indices_last_oversized_not_newest_chunk() {
    let counts = [4usize, 9, 9, 4];
    assert_eq!(stall_pair_indices(&counts, 4), Some((1, 2)));
    let front_mega = [9usize, 4, 4, 4];
    assert_eq!(stall_pair_indices(&front_mega, 4), Some((0, 3)));
}

#[test]
fn dest_bc_checkpoint_allcold_write_ok_dest_ba_not_144g() {
    assert!(checkpoint_allcold_write_ok(1, 9, 8, 4));
    assert!(checkpoint_allcold_write_ok(
        5,
        20_000_000 * 5,
        8,
        20_000_000
    ));
    assert!(checkpoint_allcold_write_ok(9, 18, 8, 20_000_000));
    assert!(
        !checkpoint_allcold_write_ok(18, 72, 8, 4),
        "dest-bc leftover 20M chunks must not AllCold-write 144G"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn register_seg_hot_pins_seed_when_eligible() {
    let _guard = hot_pin_env_lock();
    // SAFETY: exclusive via hot_pin_env_lock; env restored before exit.
    unsafe {
        std::env::set_var("BLVM_IBD_HOT_PIN", "1");
        std::env::set_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES", "2");
        std::env::set_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES", "1000");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_SEGS");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let k0 = {
        let mut k = [0u8; 36];
        k[0] = 9;
        k
    };
    let k1 = {
        let mut k = [0u8; 36];
        k[0] = 10;
        k
    };
    let entries = vec![
        OutputKV::new_add(k0, 100, 1000),
        OutputKV::new_add(k1, 101, 1001),
    ];
    // Streaming seed path: write without pin, then register_seg installs HotPin.
    let seg = DiskSegment::write_from_slice(tmp.path(), 0, (100, 101), &entries)
        .expect("write_from_slice");
    assert!(!seg.has_hot_body());
    disk.register_seg(seg);
    assert!(disk.segments.read()[0].has_hot_body());

    let keys = [k0, k1];
    let mut ids = [OutputId::MAX, OutputId::MAX];
    disk.batch_query(&keys, &mut ids, 200);
    assert_eq!(ids[0], 1000);
    assert_eq!(ids[1], 1001);

    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES");
    }
    std::mem::forget(tmp);
}

#[serial_test::serial(ibd)]
#[test]
fn compact_skips_hot_pin_seed_prefix() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::set_var("BLVM_IBD_HOT_PIN", "1");
        std::env::set_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES", "2");
        std::env::set_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES", "1000");
        std::env::set_var("BLVM_IBD_HOT_PIN_MAX_SEGS", "2");
        std::env::set_var("BLVM_IBD_DISK_FAN_IN", "3");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let mk = |b: u8| {
        let mut k = [0u8; 36];
        k[0] = b;
        k
    };
    disk.register_seg(
        DiskSegment::write_from_slice(
            tmp.path(),
            0,
            (1, 1),
            &[
                OutputKV::new_add(mk(1), 1, 10),
                OutputKV::new_add(mk(2), 1, 11),
            ],
        )
        .expect("seed"),
    );
    // MAX_SEGS=2 → seed + newest hot; need ≥3 cold spills → push 4 megas.
    for i in 0u8..4 {
        let b = 10 + i * 2;
        let h = 10 + i32::from(i);
        disk.push_run_no_compact(MemoryRun::build(vec![
            OutputKV::new_add(mk(b), h, 100 + u64::from(i)),
            OutputKV::new_add(mk(b + 1), h, 200 + u64::from(i)),
        ]))
        .expect("spill");
    }
    assert!(
        disk.segments.read()[0].has_hot_body(),
        "seed pinned before compact"
    );
    assert!(
        disk.compactable_count() >= 3,
        "cold={}",
        disk.compactable_count()
    );
    // Bypass min-interval throttle used by compact_oldest_if_needed.
    disk.do_compact_plain().expect("compact");
    let segs = disk.segments.read();
    assert!(segs[0].has_hot_body(), "seed HotPin must survive compact");
    assert!(
        segs.iter().filter(|s| s.has_hot_body()).count() >= 1,
        "at least seed still pinned"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_SEGS");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
    }
    std::mem::forget(tmp);
}

#[serial_test::serial(ibd)]
#[test]
fn hot_pin_max_segs2_keeps_seed_and_newest() {
    let _guard = hot_pin_env_lock();
    // SAFETY: exclusive via hot_pin_env_lock; env restored before exit.
    unsafe {
        std::env::set_var("BLVM_IBD_HOT_PIN", "1");
        std::env::set_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES", "2");
        std::env::set_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES", "1000");
        std::env::set_var("BLVM_IBD_HOT_PIN_MAX_SEGS", "2");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let mk = |b: u8| {
        let mut k = [0u8; 36];
        k[0] = b;
        k
    };
    let seed = vec![
        OutputKV::new_add(mk(1), 1, 10),
        OutputKV::new_add(mk(2), 1, 11),
    ];
    let seg = DiskSegment::write_from_slice(tmp.path(), 0, (1, 1), &seed).expect("seed write");
    disk.register_seg(seg);
    disk.push_run_no_compact(MemoryRun::build(vec![
        OutputKV::new_add(mk(3), 2, 20),
        OutputKV::new_add(mk(4), 2, 21),
    ]))
    .expect("spill1");
    disk.push_run_no_compact(MemoryRun::build(vec![
        OutputKV::new_add(mk(5), 3, 30),
        OutputKV::new_add(mk(6), 3, 31),
    ]))
    .expect("spill2");
    let segs = disk.segments.read();
    assert_eq!(segs.len(), 3);
    assert!(segs[0].has_hot_body(), "seed must stay pinned");
    assert!(!segs[1].has_hot_body(), "middle spill must be trimmed");
    assert!(segs[2].has_hot_body(), "newest spill must be pinned");
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_SEGS");
    }
    std::mem::forget(tmp);
}

#[serial_test::serial(ibd)]
#[test]
fn hot_pin_max_segs3_keeps_seed_and_two_newest() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::set_var("BLVM_IBD_HOT_PIN", "1");
        std::env::set_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES", "2");
        std::env::set_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES", "1000");
        std::env::set_var("BLVM_IBD_HOT_PIN_MAX_SEGS", "3");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let mk = |b: u8| {
        let mut k = [0u8; 36];
        k[0] = b;
        k
    };
    let seed = vec![
        OutputKV::new_add(mk(1), 1, 10),
        OutputKV::new_add(mk(2), 1, 11),
    ];
    disk.register_seg(DiskSegment::write_from_slice(tmp.path(), 0, (1, 1), &seed).expect("seed"));
    for (i, base) in [(2u8, 20u64), (3, 30), (4, 40)].into_iter().enumerate() {
        let h = (i + 2) as i32;
        disk.push_run_no_compact(MemoryRun::build(vec![
            OutputKV::new_add(mk(base.0), h, base.1),
            OutputKV::new_add(mk(base.0 + 1), h, base.1 + 1),
        ]))
        .expect("spill");
    }
    let segs = disk.segments.read();
    assert_eq!(segs.len(), 4);
    assert!(segs[0].has_hot_body(), "seed");
    assert!(!segs[1].has_hot_body(), "oldest spill trimmed");
    assert!(segs[2].has_hot_body(), "2nd newest");
    assert!(segs[3].has_hot_body(), "newest");
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_SEGS");
    }
    std::mem::forget(tmp);
}

#[serial_test::serial(ibd)]
#[test]
fn hot_pin_serves_batch_query_and_clears_on_demand() {
    let _guard = hot_pin_env_lock();
    // SAFETY: exclusive via hot_pin_env_lock; env restored before exit.
    unsafe {
        std::env::set_var("BLVM_IBD_HOT_PIN", "1");
        std::env::set_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES", "2");
        std::env::set_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES", "1000");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_SEGS");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let k0 = {
        let mut k = [0u8; 36];
        k[0] = 1;
        k
    };
    let k1 = {
        let mut k = [0u8; 36];
        k[0] = 2;
        k
    };
    let run = MemoryRun::build(vec![
        OutputKV::new_add(k0, 10, 100),
        OutputKV::new_add(k1, 11, 101),
    ]);
    disk.push_run_no_compact(run).expect("push");
    assert!(disk.segments.read()[0].has_hot_body());

    let keys = [k0, k1];
    let mut ids = [OutputId::MAX, OutputId::MAX];
    disk.batch_query(&keys, &mut ids, 100);
    assert_eq!(ids[0], 100);
    assert_eq!(ids[1], 101);

    disk.clear_all_hot_pins();
    assert!(!disk.segments.read()[0].has_hot_body());
    // Still correct via pread after pin drop.
    let mut ids2 = [OutputId::MAX, OutputId::MAX];
    disk.batch_query(&keys, &mut ids2, 100);
    assert_eq!(ids2[0], 100);
    assert_eq!(ids2[1], 101);

    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES");
    }
    std::mem::forget(tmp);
}

#[serial_test::serial(ibd)]
#[test]
fn clear_hot_pins_keep_seed_preserves_oldest() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::set_var("BLVM_IBD_HOT_PIN", "1");
        std::env::set_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES", "2");
        std::env::set_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES", "1000");
        std::env::set_var("BLVM_IBD_HOT_PIN_MAX_SEGS", "2");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let mk = |b: u8| {
        let mut k = [0u8; 36];
        k[0] = b;
        k
    };
    disk.push_run_no_compact(MemoryRun::build(vec![
        OutputKV::new_add(mk(1), 10, 100),
        OutputKV::new_add(mk(2), 11, 101),
    ]))
    .expect("seed");
    disk.push_run_no_compact(MemoryRun::build(vec![
        OutputKV::new_add(mk(3), 12, 102),
        OutputKV::new_add(mk(4), 13, 103),
    ]))
    .expect("newest");
    disk.clear_hot_pins_keep_seed();
    let segs = disk.segments.read();
    assert!(segs[0].has_hot_body(), "seed survives pressure clear");
    assert!(!segs[1].has_hot_body(), "newest dropped");
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_SEGS");
    }
    std::mem::forget(tmp);
}

#[serial_test::serial(ibd)]
#[test]
fn hot_pin_max_segs_keeps_prior_mega() {
    let _guard = hot_pin_env_lock();
    // SAFETY: exclusive via hot_pin_env_lock; env restored before exit.
    unsafe {
        std::env::set_var("BLVM_IBD_HOT_PIN", "1");
        std::env::set_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES", "2");
        std::env::set_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES", "1000");
        std::env::set_var("BLVM_IBD_HOT_PIN_MAX_SEGS", "2");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let k0 = {
        let mut k = [0u8; 36];
        k[0] = 10;
        k
    };
    let k1 = {
        let mut k = [0u8; 36];
        k[0] = 20;
        k
    };
    let k0b = {
        let mut k = [0u8; 36];
        k[0] = 11;
        k
    };
    let k1b = {
        let mut k = [0u8; 36];
        k[0] = 21;
        k
    };
    disk.push_run_no_compact(MemoryRun::build(vec![
        OutputKV::new_add(k0, 10, 100),
        OutputKV::new_add(k0b, 11, 101),
    ]))
    .expect("push0");
    disk.push_run_no_compact(MemoryRun::build(vec![
        OutputKV::new_add(k1, 20, 200),
        OutputKV::new_add(k1b, 21, 201),
    ]))
    .expect("push1");
    let segs = disk.segments.read();
    assert!(segs[0].has_hot_body(), "prior mega should stay pinned");
    assert!(segs[1].has_hot_body(), "newest mega should be pinned");
    drop(segs);

    // Third pin under max=2: keep seed (oldest) + newest; drop middle.
    let k2 = {
        let mut k = [0u8; 36];
        k[0] = 30;
        k
    };
    let k2b = {
        let mut k = [0u8; 36];
        k[0] = 31;
        k
    };
    disk.push_run_no_compact(MemoryRun::build(vec![
        OutputKV::new_add(k2, 30, 300),
        OutputKV::new_add(k2b, 31, 301),
    ]))
    .expect("push2");
    let segs = disk.segments.read();
    assert!(segs[0].has_hot_body(), "seed/oldest stays pinned");
    assert!(!segs[1].has_hot_body(), "middle spill trimmed");
    assert!(segs[2].has_hot_body(), "newest pinned");

    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_SEGS");
    }
    std::mem::forget(tmp);
}

#[serial_test::serial(ibd)]
#[test]
fn spill_max_entries_size_splits_into_multiple_segments() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::set_var("BLVM_IBD_SPILL_MAX_ENTRIES", "2");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::set_var("BLVM_IBD_HOT_PIN", "1");
        std::env::set_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES", "1");
        std::env::set_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES", "1000");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let mut kvs = Vec::new();
    for i in 0u8..5 {
        let mut k = [0u8; 36];
        k[0] = i + 1;
        kvs.push(OutputKV::new_add(k, 10 + i32::from(i), 100 + u64::from(i)));
    }
    disk.push_run_no_compact(MemoryRun::build(kvs))
        .expect("size-split push");
    assert_eq!(disk.segment_count(), 3, "5 entries @ max=2 → 3 segments");
    let keys: Vec<_> = (0u8..5)
        .map(|i| {
            let mut k = [0u8; 36];
            k[0] = i + 1;
            k
        })
        .collect();
    let mut ids = vec![OutputId::MAX; 5];
    disk.batch_query(&keys, &mut ids, 100);
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(*id, 100 + i as u64);
    }
    unsafe {
        std::env::remove_var("BLVM_IBD_SPILL_MAX_ENTRIES");
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES");
    }
    std::mem::forget(tmp);
}

#[serial_test::serial(ibd)]
#[test]
fn compact_max_entries_default_20m_zero_unlimited() {
    unsafe {
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
        assert_eq!(compact_max_entries_from_env(), COMPACT_MAX_ENTRIES_DEFAULT);
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "");
        assert_eq!(compact_max_entries_from_env(), COMPACT_MAX_ENTRIES_DEFAULT);
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "0");
        assert_eq!(compact_max_entries_from_env(), 0);
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
        assert_eq!(compact_max_entries_from_env(), 4);
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
}

/// dest-bc 650k: fan-in packed 8 segs into one mega. Cap must split compact output
/// (not HP-M4 spill-split) and still resolve every key.
#[serial_test::serial(ibd)]
#[test]
fn dest_bc_compact_max_splits_fanin_mega_and_resolves() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::set_var("BLVM_IBD_DISK_FAN_IN", "3");
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let mk = |i: u8| {
        let mut k = [0u8; 36];
        k[0] = i;
        k
    };
    // 3 cold segs × 3 Adds = 9 live entries. fan_in=3 compact → 4+4+1 under cap=4.
    for s in 0u8..3 {
        let entries: Vec<OutputKV> = (0u8..3)
            .map(|j| {
                let i = s * 3 + j + 1;
                OutputKV::new_add(mk(i), 10 + i32::from(i), 1000 + u64::from(i))
            })
            .collect();
        disk.push_run_no_compact(MemoryRun::build(entries))
            .expect("spill");
    }
    assert_eq!(disk.segment_count(), 3);
    disk.do_compact_plain().expect("compact");
    assert_eq!(
        disk.segment_count(),
        3,
        "9 live entries @ compact_max=4 → 3 chunks (dest-bc mega split)"
    );
    let keys: Vec<_> = (1u8..=9).map(mk).collect();
    let mut ids = vec![OutputId::MAX; 9];
    disk.batch_query(&keys, &mut ids, 100);
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            *id,
            1001 + i as u64,
            "miss after compact split at {}",
            i + 1
        );
    }

    unsafe {
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "0");
    }
    for s in 0u8..3 {
        let i = 20 + s;
        disk.push_run_no_compact(MemoryRun::build(vec![OutputKV::new_add(
            mk(i),
            50,
            2000 + u64::from(i),
        )]))
        .expect("spill2");
    }
    disk.do_compact_plain().expect("unlimited compact");
    let n = disk.segment_count();
    assert!(
        n <= 4,
        "unlimited compact must not keep dest-bc-style 3-way split, segs={n}"
    );

    unsafe {
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    std::mem::forget(tmp);
}

/// dest-bc 660k sat at 7 megas — `fan_in=8` never ran. Oversized cold at
/// compactable ≥4 folds oldest+newest (20M cap), not wait for 8 segs.
#[serial_test::serial(ibd)]
#[test]
fn dest_bc_fanin_stall_compacts_all_cold_below_fan_in() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN"); // default 8 → stall min 4
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let mk = |i: u8| {
        let mut k = [0u8; 36];
        k[0] = i;
        k
    };
    // dest-bc stall: ≥4 cold segs, one mega. 3 tiny + 9-entry mega.
    for t in 0u8..3 {
        disk.push_run_no_compact(MemoryRun::build(vec![OutputKV::new_add(
            mk(100 + t),
            5,
            50 + u64::from(t),
        )]))
        .expect("tiny");
    }
    let entries: Vec<OutputKV> = (1u8..=9)
        .map(|i| OutputKV::new_add(mk(i), 10, 1000 + u64::from(i)))
        .collect();
    disk.push_run_no_compact(MemoryRun::build(entries))
        .expect("one mega");
    assert_eq!(
        disk.segment_count(),
        4,
        "below fan_in=8 — dest-bc stall shape"
    );
    disk.compact_oldest_if_needed()
        .expect("stall pair compact oversized below fan_in");
    let n = disk.segment_count();
    assert_eq!(
        n, 5,
        "oldest tiny+mega @ cap=4 → 3 chunks + 2 leftover tinies (not take-all to 3)"
    );
    let keys: Vec<_> = (1u8..=9).chain(100u8..=102).map(mk).collect();
    let mut ids = vec![OutputId::MAX; keys.len()];
    disk.batch_query(&keys, &mut ids, 100);
    for i in 0..9 {
        assert_eq!(
            ids[i],
            1001 + i as u64,
            "miss after stall pair at {}",
            i + 1
        );
    }
    for i in 0..3 {
        assert_eq!(ids[9 + i], 50 + i as u64, "tiny miss at {}", i);
    }

    unsafe {
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "0");
    }
    let tmp2 = tempfile::tempdir().expect("tempdir2");
    let (disk2, _) = DiskIndex::new_empty(tmp2.path()).expect("DiskIndex");
    let disk2 = Arc::new(disk2);
    for t in 0u8..3 {
        disk2
            .push_run_no_compact(MemoryRun::build(vec![OutputKV::new_add(
                mk(100 + t),
                5,
                50 + u64::from(t),
            )]))
            .expect("tiny2");
    }
    disk2
        .push_run_no_compact(MemoryRun::build(
            (1u8..=9)
                .map(|i| OutputKV::new_add(mk(i), 10, 1000 + u64::from(i)))
                .collect(),
        ))
        .expect("unlimited mega");
    disk2.compact_oldest_if_needed().expect("no stall at 0");
    assert_eq!(
        disk2.segment_count(),
        4,
        "COMPACT_MAX=0 must leave dest-bc mega (no stall pair)"
    );

    unsafe {
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    std::mem::forget(tmp);
    std::mem::forget(tmp2);
}

/// HP-M4 REVERT: a lone mega spill at 1–3 segs must not size-split (300–400k tip).
#[serial_test::serial(ibd)]
#[test]
fn dest_bc_lone_mega_spill_must_not_hp_m4_split() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let mk = |i: u8| {
        let mut k = [0u8; 36];
        k[0] = i;
        k
    };
    disk.push_run_no_compact(MemoryRun::build(
        (1u8..=9)
            .map(|i| OutputKV::new_add(mk(i), 10, 1000 + u64::from(i)))
            .collect(),
    ))
    .expect("lone mega");
    disk.compact_oldest_if_needed().expect("must no-op");
    assert_eq!(
        disk.segment_count(),
        1,
        "1 cold mega < stall min 4 must not HP-M4 split"
    );
    for t in 0u8..2 {
        disk.push_run_no_compact(MemoryRun::build(vec![OutputKV::new_add(
            mk(50 + t),
            5,
            10 + u64::from(t),
        )]))
        .expect("tiny");
    }
    assert_eq!(disk.segment_count(), 3);
    disk.compact_oldest_if_needed().expect("still no-op at 3");
    assert_eq!(
        disk.segment_count(),
        3,
        "3 cold segs is HP-M4 band, not dest-bc 5–7 stall"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    std::mem::forget(tmp);
}

/// dest-bc 144G vs dest-ba 67G: spends of old Adds live in later megas.
/// Stall folds oldest+newest (not take-all of every mega — peak extra is two segs).
#[serial_test::serial(ibd)]
#[test]
fn dest_bc_stall_take_all_gcs_spend_in_later_mega() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
    }
    super::super::set_gc_fence(100);
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let mk = |i: u8| {
        let mut k = [0u8; 36];
        k[0] = i;
        k
    };
    disk.push_run_no_compact(MemoryRun::build(vec![OutputKV::new_add(mk(1), 10, 1000)]))
        .expect("old add");
    for t in 0u8..2 {
        disk.push_run_no_compact(MemoryRun::build(vec![OutputKV::new_add(
            mk(100 + t),
            5,
            50 + u64::from(t),
        )]))
        .expect("tiny");
    }
    let mut later: Vec<OutputKV> = vec![OutputKV::new_delete(mk(1), 50)];
    later.extend((2u8..=9).map(|i| OutputKV::new_add(mk(i), 10, 1000 + u64::from(i))));
    disk.push_run_no_compact(MemoryRun::build(later))
        .expect("later mega with spend");
    assert_eq!(disk.segment_count(), 4);
    let mut pre = [OutputId::MAX];
    disk.batch_query(&[mk(1)], &mut pre, 40);
    assert_eq!(
        pre[0], 1000,
        "Add must resolve before stall pair (before Delete height)"
    );
    disk.compact_oldest_if_needed().expect("stall pair GC");
    let mut post = [OutputId::MAX];
    disk.batch_query(&[mk(1)], &mut post, 40);
    assert_eq!(
        post[0],
        OutputId::MAX,
        "Delete in later mega must GC the dest-bc old Add"
    );
    let keys: Vec<_> = (2u8..=9).chain(100u8..=101).map(mk).collect();
    let mut ids = vec![OutputId::MAX; keys.len()];
    disk.batch_query(&keys, &mut ids, 100);
    for i in 0..8 {
        assert_eq!(ids[i], 1002 + i as u64, "live miss at {}", i + 2);
    }
    assert_eq!(ids[8], 50);
    assert_eq!(ids[9], 51);
    unsafe {
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    super::super::set_gc_fence(i32::MAX);
    std::mem::forget(tmp);
}

/// dest-bc 144G: Delete sits in a later mega; newest is a 20M-shaped chunk.
/// Pairing mega+chunk would miss the spend. Last-oversized pair must GC.
#[serial_test::serial(ibd)]
#[test]
fn dest_bc_stall_pair_gcs_spend_in_later_mega_not_newest_chunk() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
    }
    super::super::set_gc_fence(100);
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let mk = |i: u8| {
        let mut k = [0u8; 36];
        k[0] = i;
        k
    };
    disk.push_run_no_compact(MemoryRun::build(vec![OutputKV::new_add(mk(200), 5, 200)]))
        .expect("tiny front");
    let mega_add: Vec<OutputKV> = std::iter::once(OutputKV::new_add(mk(1), 10, 1000))
        .chain((2u8..=9).map(|i| OutputKV::new_add(mk(i), 10, 1000 + u64::from(i))))
        .collect();
    disk.push_run_no_compact(MemoryRun::build(mega_add))
        .expect("mega with old Add");
    let mega_del: Vec<OutputKV> = std::iter::once(OutputKV::new_delete(mk(1), 50))
        .chain((10u8..=17).map(|i| OutputKV::new_add(mk(i), 10, 1000 + u64::from(i))))
        .collect();
    disk.push_run_no_compact(MemoryRun::build(mega_del))
        .expect("later mega with spend");
    disk.push_run_no_compact(MemoryRun::build(
        (80u8..=82)
            .map(|i| OutputKV::new_add(mk(i), 90, 80 + u64::from(i)))
            .collect(),
    ))
    .expect("newest 20M-shaped chunk");
    assert_eq!(disk.segment_count(), 4);
    let mut pre = [OutputId::MAX];
    disk.batch_query(&[mk(1)], &mut pre, 40);
    assert_eq!(pre[0], 1000, "Add must resolve before pair");
    disk.compact_oldest_if_needed()
        .expect("stall pair last-oversized");
    let mut post = [OutputId::MAX];
    disk.batch_query(&[mk(1)], &mut post, 40);
    assert_eq!(
        post[0],
        OutputId::MAX,
        "Delete in later mega must GC even when newest is a chunk"
    );
    let keys: Vec<_> = (2u8..=17)
        .chain(80u8..=82)
        .chain(std::iter::once(200u8))
        .map(mk)
        .collect();
    let mut ids = vec![OutputId::MAX; keys.len()];
    disk.batch_query(&keys, &mut ids, 100);
    for i in 0..16 {
        assert_eq!(ids[i], 1002 + i as u64, "live miss at {}", i + 2);
    }
    assert_eq!(ids[16], 160);
    assert_eq!(ids[17], 161);
    assert_eq!(ids[18], 162);
    assert_eq!(ids[19], 200);
    unsafe {
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    super::super::set_gc_fence(i32::MAX);
    std::mem::forget(tmp);
}

/// Live IBD calls `compact_oldest_async` after spill, not `compact_oldest_if_needed`.
#[serial_test::serial(ibd)]
#[test]
fn dest_bc_compact_oldest_async_stall_compacts() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let mk = |i: u8| {
        let mut k = [0u8; 36];
        k[0] = i;
        k
    };
    for t in 0u8..3 {
        disk.push_run_no_compact(MemoryRun::build(vec![OutputKV::new_add(
            mk(100 + t),
            5,
            50 + u64::from(t),
        )]))
        .expect("tiny");
    }
    disk.push_run_no_compact(MemoryRun::build(
        (1u8..=9)
            .map(|i| OutputKV::new_add(mk(i), 10, 1000 + u64::from(i)))
            .collect(),
    ))
    .expect("mega");
    assert_eq!(disk.segment_count(), 4);
    disk.compact_oldest_async();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if disk.segment_count() == 5 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        disk.segment_count(),
        5,
        "async compact must stall-pair dest-bc 4-seg mega (WAN path)"
    );
    let keys: Vec<_> = (1u8..=9).map(mk).collect();
    let mut ids = vec![OutputId::MAX; 9];
    disk.batch_query(&keys, &mut ids, 100);
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(*id, 1001 + i as u64, "async miss at {}", i + 1);
    }
    unsafe {
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    std::mem::forget(tmp);
}

#[serial_test::serial(ibd)]
#[test]
fn async_spill_serves_queries_via_pending_then_registers() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::set_var("BLVM_IBD_ASYNC_DISK_SPILL", "1");
        std::env::set_var("BLVM_IBD_HOT_PIN", "1");
        std::env::set_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES", "2");
        std::env::set_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES", "1000");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let k0 = {
        let mut k = [0u8; 36];
        k[0] = 7;
        k
    };
    let k1 = {
        let mut k = [0u8; 36];
        k[0] = 8;
        k
    };
    disk.push_run_no_compact(MemoryRun::build(vec![
        OutputKV::new_add(k0, 10, 100),
        OutputKV::new_add(k1, 11, 101),
    ]))
    .expect("async push");
    // Immediately queryable (pending and/or registered).
    let keys = [k0, k1];
    let mut ids = [OutputId::MAX, OutputId::MAX];
    disk.batch_query(&keys, &mut ids, 100);
    assert_eq!(ids[0], 100);
    assert_eq!(ids[1], 101);
    disk.wait_pending_spills();
    assert_eq!(disk.segment_count(), 1);
    assert!(disk.segments.read()[0].has_hot_body());
    unsafe {
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MIN_ENTRIES");
        std::env::remove_var("BLVM_IBD_HOT_PIN_MAX_ENTRIES");
    }
    std::mem::forget(tmp);
}

/// dest-bc 144G take-all would ENOSPC (65G free). 5 tinies + 1 mega: pair
/// oldest+newest, leave the middle tinies. Take-all would merge all 6.
#[serial_test::serial(ibd)]
#[test]
fn dest_bc_stall_pair_leaves_middle_segs() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let mk = |i: u8| {
        let mut k = [0u8; 36];
        k[0] = i;
        k
    };
    for t in 0u8..5 {
        disk.push_run_no_compact(MemoryRun::build(vec![OutputKV::new_add(
            mk(100 + t),
            5,
            50 + u64::from(t),
        )]))
        .expect("tiny");
    }
    disk.push_run_no_compact(MemoryRun::build(
        (1u8..=9)
            .map(|i| OutputKV::new_add(mk(i), 10, 1000 + u64::from(i)))
            .collect(),
    ))
    .expect("mega");
    assert_eq!(disk.segment_count(), 6);
    disk.compact_oldest_if_needed().expect("stall pair");
    let n = disk.segment_count();
    assert!(
        n >= 6,
        "take-all of 6 segs → 4 chunks; pair must leave middle tinies, segs={n}"
    );
    let keys: Vec<_> = (1u8..=9).chain(100u8..=104).map(mk).collect();
    let mut ids = vec![OutputId::MAX; keys.len()];
    disk.batch_query(&keys, &mut ids, 100);
    for i in 0..9 {
        assert_eq!(ids[i], 1001 + i as u64);
    }
    for i in 0..5 {
        assert_eq!(ids[9 + i], 50 + i as u64, "middle tiny {} dropped", i);
    }
    unsafe {
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    std::mem::forget(tmp);
}

/// dest-bc 7 megas: loop must drain every >cap file (not stall with leftover megas).
#[serial_test::serial(ibd)]
#[test]
fn dest_bc_four_megas_drain_to_compact_cap() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let mk = |i: u8| {
        let mut k = [0u8; 36];
        k[0] = i;
        k
    };
    let mut expect: Vec<([u8; 36], u64)> = Vec::new();
    for s in 0u8..4 {
        let entries: Vec<OutputKV> = (1u8..=9)
            .map(|j| {
                let i = s * 10 + j;
                expect.push((mk(i), 2000 + u64::from(i)));
                OutputKV::new_add(mk(i), 10, 2000 + u64::from(i))
            })
            .collect();
        disk.push_run_no_compact(MemoryRun::build(entries))
            .expect("mega");
    }
    assert_eq!(disk.segment_count(), 4);
    disk.compact_oldest_if_needed().expect("drain megas");
    for s in disk.segments.read().iter() {
        assert!(
            s.entry_count <= 4,
            "leftover dest-bc mega {} entries",
            s.entry_count
        );
    }
    let keys: Vec<_> = expect.iter().map(|(k, _)| *k).collect();
    let mut ids = vec![OutputId::MAX; keys.len()];
    disk.batch_query(&keys, &mut ids, 100);
    for (i, (_, id)) in expect.iter().enumerate() {
        assert_eq!(ids[i], *id, "miss after mega drain at {i}");
    }
    unsafe {
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    std::mem::forget(tmp);
}

/// dest-bc 01:31Z FanIn 8×776M; 20M cap turns that into at-cap chunks. Fan-in
/// of 8×cap with no GC must not rewrite 32× (32 × 160M × 56B during ts 7.4).
#[serial_test::serial(ibd)]
#[test]
fn dest_bc_fanin_at_cap_must_not_rewrite_loop() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
    }
    TEST_COMPACT_WRITE_CALLS.store(0, Ordering::Relaxed);
    LAST_COMPACT_FINISH.store(0, Ordering::Relaxed);
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let mk = |i: u8| {
        let mut k = [0u8; 36];
        k[0] = i;
        k
    };
    for s in 0u8..8 {
        let entries: Vec<OutputKV> = (0u8..4)
            .map(|j| {
                let i = s * 4 + j + 1;
                OutputKV::new_add(mk(i), 10, 3000 + u64::from(i))
            })
            .collect();
        disk.push_run_no_compact(MemoryRun::build(entries))
            .expect("at-cap chunk");
    }
    assert_eq!(disk.segment_count(), 8);
    disk.compact_oldest_if_needed().expect("one fan-in");
    let calls = TEST_COMPACT_WRITE_CALLS.load(Ordering::Relaxed);
    assert!(
        calls <= 2,
        "at-cap fan-in with no GC must not loop 32 rewrites, calls={calls}"
    );
    assert_eq!(disk.segment_count(), 8, "8×4 @ cap=4 stays 8 segs");
    let keys: Vec<_> = (1u8..=32).map(mk).collect();
    let mut ids = vec![OutputId::MAX; 32];
    disk.batch_query(&keys, &mut ids, 100);
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(*id, 3001 + i as u64, "miss after no-op fan-in at {}", i + 1);
    }
    TEST_COMPACT_WRITE_CALLS.store(0, Ordering::Relaxed);
    disk.compact_oldest_if_needed()
        .expect("second kick skipped");
    assert_eq!(
        TEST_COMPACT_WRITE_CALLS.load(Ordering::Relaxed),
        0,
        "fence unchanged after first at-cap pass must not rewrite again"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    std::mem::forget(tmp);
}

fn spill_n_adds_at(disk: &Arc<DiskIndex>, start: u8, n: u8, h: i32, id0: u64) {
    let mk = |b: u8| {
        let mut k = [0u8; 36];
        k[0] = b;
        k
    };
    let entries: Vec<OutputKV> = (0..n)
        .map(|j| OutputKV::new_add(mk(start + j), h, id0 + u64::from(j)))
        .collect();
    disk.push_run_no_compact(MemoryRun::build(entries))
        .expect("spill");
}

/// R-268 358k: 5 overlapping segs sat below fan_in=8. dest-bc was 2.
/// Mid-band compact must collapse 5 tiny segs (GC can shrink / count can fall).
#[serial_test::serial(ibd)]
#[test]
fn r268_five_segs_below_fan_in_compact_to_dest_bc_shape() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    super::super::set_gc_fence(i32::MAX);
    LAST_COMPACT_FINISH.store(0, Ordering::Relaxed);
    TEST_COMPACT_WRITE_CALLS.store(0, Ordering::Relaxed);
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    for s in 0u8..5 {
        spill_n_adds_at(&disk, s * 3 + 1, 3, 10, 6000 + u64::from(s) * 3);
    }
    assert_eq!(disk.segment_count(), 5);
    disk.compact_oldest_if_needed().expect("mid-band compact");
    assert!(
        disk.segment_count() <= 2,
        "5 segs below fan_in=8 must collapse toward dest-bc 358k (≤2), segs={}",
        disk.segment_count()
    );
    let mk = |b: u8| {
        let mut k = [0u8; 36];
        k[0] = b;
        k
    };
    let keys: Vec<_> = (1u8..=15).map(mk).collect();
    let mut ids = vec![OutputId::MAX; keys.len()];
    disk.batch_query(&keys, &mut ids, 100);
    for (i, id) in ids.iter().enumerate() {
        assert_ne!(
            *id,
            OutputId::MAX,
            "key {} dropped by mid-band compact",
            i + 1
        );
    }
    super::super::set_gc_fence(i32::MAX);
    std::mem::forget(tmp);
}

/// HP-M4: 1–3 segs must not fold (stall_split_min = 4 @ fan_in=8).
#[serial_test::serial(ibd)]
#[test]
fn hp_m4_three_segs_do_not_midband_fold() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    LAST_COMPACT_FINISH.store(0, Ordering::Relaxed);
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    for s in 0u8..3 {
        spill_n_adds_at(&disk, s * 3 + 1, 3, 10, 7000 + u64::from(s) * 3);
    }
    assert_eq!(disk.segment_count(), 3);
    disk.compact_oldest_if_needed().expect("no mid-band at 3");
    assert_eq!(disk.segment_count(), 3, "HP-M4 1–3 stay");
    std::mem::forget(tmp);
}

/// Matching-height 358k: candidates scale with live overlapping segs, not prefix bits.
/// Keys live in every seg at h=100; query `before=50` so they stay unresolved and
/// every seg still emits directory candidates (dest-bc 2 segs / 245 cands vs R-268 5 / 670).
#[serial_test::serial(ibd)]
#[test]
fn matching_height_358k_candidate_fanout() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    fn key_at(i: u16) -> [u8; 36] {
        let mut k = [0u8; 36];
        k[0] = (i >> 8) as u8;
        k[1] = i as u8;
        k
    }
    fn measure(n_segs: usize) -> (u64, u64, u64) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
        let disk = Arc::new(disk);
        let n_keys = 64u16;
        for s in 0..n_segs {
            let mut entries: Vec<OutputKV> = (0..n_keys)
                .map(|i| OutputKV::new_add(key_at(i), 100, 10_000 + u64::from(i)))
                .collect();
            // Height window overlap: filler below `before` so the seg is scanned.
            entries.push(OutputKV::new_add(key_at(1000 + s as u16), 10, 1));
            entries.sort_unstable();
            disk.push_run_no_compact(MemoryRun::build(entries))
                .expect("spill");
        }
        assert_eq!(disk.segment_count(), n_segs);
        let keys: Vec<_> = (0..n_keys).map(key_at).collect();
        let mut ids = vec![OutputId::MAX; keys.len()];
        super::super::disk_segment::reset_disk_io_stats();
        disk.batch_query(&keys, &mut ids, 50);
        for id in &ids {
            assert_eq!(
                *id,
                OutputId::MAX,
                "h=100 must stay unresolved at before=50"
            );
        }
        let (_preads, pread_kb, _max, cands, segs) =
            super::super::disk_segment::take_disk_io_stats();
        std::mem::forget(tmp);
        (cands, segs, pread_kb)
    }
    let (c2, s2, kb2) = measure(2);
    let (c5, s5, kb5) = measure(5);
    assert_eq!(s2, 2, "dest-bc 358k segs");
    assert_eq!(s5, 5, "R-268 358k segs");
    assert_eq!(c2, 64 * 2, "cands scale with segs (2)");
    assert_eq!(c5, 64 * 5, "cands scale with segs (5)");
    assert!(
        c5 as f64 / c2 as f64 > 2.0,
        "5-seg fan-out must dominate 2-seg, c2={c2} c5={c5} kb2={kb2} kb5={kb5}"
    );
}

/// 400k apply: fence stuck below journal min_height. First at-cap FanIn must
/// not run (the photocopier). Writes stay 0.
#[serial_test::serial(ibd)]
#[test]
fn fanin_photocopy_skip_when_journal_above_stuck_fence() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
    }
    super::super::set_gc_fence(5);
    TEST_COMPACT_WRITE_CALLS.store(0, Ordering::Relaxed);
    LAST_COMPACT_FINISH.store(0, Ordering::Relaxed);
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    for s in 0u8..8 {
        spill_n_adds_at(&disk, s * 4 + 1, 4, 10, 3000 + u64::from(s) * 4);
    }
    assert_eq!(disk.segment_count(), 8);
    disk.compact_oldest_if_needed().expect("skip photocopy");
    assert_eq!(
        TEST_COMPACT_WRITE_CALLS.load(Ordering::Relaxed),
        0,
        "at-cap + min_h>fence must not FanIn"
    );
    assert_eq!(disk.segment_count(), 8);
    super::super::set_gc_fence(i32::MAX);
    unsafe {
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    std::mem::forget(tmp);
}

/// 7×20M + 2.1M remainder: ceil(sum/cap) >= n even though one file is small.
#[serial_test::serial(ibd)]
#[test]
fn fanin_photocopy_skip_remainder_shape() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
    }
    super::super::set_gc_fence(5);
    TEST_COMPACT_WRITE_CALLS.store(0, Ordering::Relaxed);
    LAST_COMPACT_FINISH.store(0, Ordering::Relaxed);
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    for s in 0u8..7 {
        spill_n_adds_at(&disk, s * 4 + 1, 4, 10, 4000 + u64::from(s) * 4);
    }
    spill_n_adds_at(&disk, 29, 1, 10, 4028);
    assert_eq!(disk.segment_count(), 8);
    disk.compact_oldest_if_needed()
        .expect("skip remainder photocopy");
    assert_eq!(
        TEST_COMPACT_WRITE_CALLS.load(Ordering::Relaxed),
        0,
        "7×cap + leftover still 8→8 without GC"
    );
    super::super::set_gc_fence(i32::MAX);
    unsafe {
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    std::mem::forget(tmp);
}

/// R-366: oldest 8 at the cap is a photocopy. Young window of smaller segs
/// merges; those oldest entry counts stay. A young window that is also
/// count-neutral still skips.
#[serial_test::serial(ibd)]
#[test]
fn r366_young_fanin_when_oldest_is_photocopy_skip() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
    }
    super::super::set_gc_fence(5);
    TEST_COMPACT_WRITE_CALLS.store(0, Ordering::Relaxed);
    LAST_COMPACT_FINISH.store(0, Ordering::Relaxed);
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    for s in 0u8..8 {
        spill_n_adds_at(&disk, s * 4 + 1, 4, 10, 8000 + u64::from(s) * 4);
    }
    for s in 0u8..8 {
        spill_n_adds_at(&disk, 80 + s, 1, 30, 9000 + u64::from(s));
    }
    assert_eq!(disk.segment_count(), 16);
    let (oldest_n, oldest_ptr) = {
        let r = disk.segments.read();
        let n: Vec<usize> = r.iter().take(8).map(|s| s.entry_count).collect();
        let ptr: Vec<usize> = r.iter().take(8).map(|s| Arc::as_ptr(s) as usize).collect();
        (n, ptr)
    };
    assert_eq!(oldest_n, vec![4; 8]);
    disk.compact_oldest_if_needed().expect("young fan-in");
    assert!(
        TEST_COMPACT_WRITE_CALLS.load(Ordering::Relaxed) >= 1,
        "young window below cap must compact"
    );
    assert!(
        disk.segment_count() < 16,
        "young merge must drop segment count, got {}",
        disk.segment_count()
    );
    {
        let r = disk.segments.read();
        let n: Vec<usize> = r.iter().take(8).map(|s| s.entry_count).collect();
        let ptr: Vec<usize> = r.iter().take(8).map(|s| Arc::as_ptr(s) as usize).collect();
        assert_eq!(n, oldest_n, "oldest 8 entry counts must stay");
        assert_eq!(ptr, oldest_ptr, "oldest 8 segments must not be rewritten");
    }
    std::mem::forget(tmp);

    TEST_COMPACT_WRITE_CALLS.store(0, Ordering::Relaxed);
    LAST_COMPACT_FINISH.store(0, Ordering::Relaxed);
    let tmp2 = tempfile::tempdir().expect("tempdir");
    let (disk2, _) = DiskIndex::new_empty(tmp2.path()).expect("DiskIndex");
    let disk2 = Arc::new(disk2);
    for s in 0u8..16 {
        spill_n_adds_at(&disk2, s * 4 + 1, 4, 10, 10000 + u64::from(s) * 4);
    }
    assert_eq!(disk2.segment_count(), 16);
    disk2
        .compact_oldest_if_needed()
        .expect("both windows count-neutral");
    assert_eq!(
        TEST_COMPACT_WRITE_CALLS.load(Ordering::Relaxed),
        0,
        "young window at cap must skip"
    );
    assert_eq!(disk2.segment_count(), 16);
    super::super::set_gc_fence(i32::MAX);
    unsafe {
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    std::mem::forget(tmp2);
}

/// Compact 1/2 shape: 8×3 @ cap=4 → 6 files, count falls, must still run.
#[serial_test::serial(ibd)]
#[test]
fn fanin_below_cap_still_runs_with_stuck_fence() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
    }
    super::super::set_gc_fence(5);
    TEST_COMPACT_WRITE_CALLS.store(0, Ordering::Relaxed);
    LAST_COMPACT_FINISH.store(0, Ordering::Relaxed);
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    for s in 0u8..8 {
        spill_n_adds_at(&disk, s * 3 + 1, 3, 10, 5000 + u64::from(s) * 3);
    }
    disk.compact_oldest_if_needed().expect("below-cap fan-in");
    assert!(
        TEST_COMPACT_WRITE_CALLS.load(Ordering::Relaxed) >= 1,
        "8×3 @ cap=4 must still merge (6 out)"
    );
    assert!(
        disk.segment_count() < 8,
        "below-cap fan-in must reduce count, got {}",
        disk.segment_count()
    );
    super::super::set_gc_fence(i32::MAX);
    unsafe {
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    std::mem::forget(tmp);
}

/// Fence advance into the journal: min_h <= fence, last is MIN → FanIn may run.
#[serial_test::serial(ibd)]
#[test]
fn fanin_runs_after_fence_advances_into_journal() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
    }
    super::super::set_gc_fence(5);
    TEST_COMPACT_WRITE_CALLS.store(0, Ordering::Relaxed);
    LAST_COMPACT_FINISH.store(0, Ordering::Relaxed);
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    for s in 0u8..8 {
        spill_n_adds_at(&disk, s * 4 + 1, 4, 10, 6000 + u64::from(s) * 4);
    }
    disk.compact_oldest_if_needed().expect("skip");
    assert_eq!(TEST_COMPACT_WRITE_CALLS.load(Ordering::Relaxed), 0);
    super::super::set_gc_fence(20);
    disk.compact_oldest_if_needed()
        .expect("fan-in after fence move");
    assert!(
        TEST_COMPACT_WRITE_CALLS.load(Ordering::Relaxed) >= 1,
        "fence into journal must allow FanIn"
    );
    super::super::set_gc_fence(i32::MAX);
    unsafe {
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    std::mem::forget(tmp);
}

fn spill_two_adds(disk: &Arc<DiskIndex>, b0: u8, h: i32, id0: u64) {
    let mk = |b: u8| {
        let mut k = [0u8; 36];
        k[0] = b;
        k
    };
    disk.push_run_no_compact(MemoryRun::build(vec![
        OutputKV::new_add(mk(b0), h, id0),
        OutputKV::new_add(mk(b0 + 1), h, id0 + 1),
    ]))
    .expect("spill");
}

/// dest-bc: last compact pass no-op'd when cold < fan_in=8. 1-seg must 1→1 tee.
#[serial_test::serial(ibd)]
#[test]
fn checkpoint_sink_tees_one_seg_below_fan_in() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::set_var("BLVM_IBD_DISK_FAN_IN", "8");
    }
    super::super::set_gc_fence(100);
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    spill_two_adds(&disk, 1, 10, 100);
    assert_eq!(disk.segment_count(), 1);
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen_cb = std::sync::Arc::clone(&seen);
    let (tee, segs) = disk
        .compact_for_checkpoint_sync_with_sink(
            50,
            Some(move |kv: OutputKV| {
                if kv.is_add() {
                    seen_cb.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            }),
        )
        .expect("sink");
    assert_eq!(segs, 1, "one cold seg at start");
    assert_eq!(tee, 2, "1→1 tee must visit both Adds");
    assert_eq!(seen.load(Ordering::Relaxed), 2);
    unsafe {
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
    }
    super::super::set_gc_fence(i32::MAX);
    std::mem::forget(tmp);
}

/// dest-bc leftover after a good 8-seg export: 2 cold segs < fan_in=8 must still tee.
#[serial_test::serial(ibd)]
#[test]
fn checkpoint_sink_tees_two_segs_below_fan_in() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::set_var("BLVM_IBD_DISK_FAN_IN", "8");
    }
    super::super::set_gc_fence(100);
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    spill_two_adds(&disk, 1, 10, 100);
    spill_two_adds(&disk, 10, 20, 200);
    assert_eq!(disk.segment_count(), 2);
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen_cb = std::sync::Arc::clone(&seen);
    let (tee, segs) = disk
        .compact_for_checkpoint_sync_with_sink(
            50,
            Some(move |kv: OutputKV| {
                if kv.is_add() {
                    seen_cb.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            }),
        )
        .expect("sink");
    assert_eq!(segs, 2);
    assert_eq!(tee, 4, "2-seg last pass must merge and tee all Adds");
    assert_eq!(seen.load(Ordering::Relaxed), 4);
    unsafe {
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
    }
    super::super::set_gc_fence(i32::MAX);
    std::mem::forget(tmp);
}

/// dest-bc leftover after a good fan-in: 9 cold segs → 1 plain 8-way merge + 2 left.
/// Last pass must take_all_cold (not wait for fan_in=8) or persist is overlay-only.
#[serial_test::serial(ibd)]
#[test]
fn checkpoint_sink_tees_nine_segs_leftover_after_fan_in() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::set_var("BLVM_IBD_DISK_FAN_IN", "8");
    }
    super::super::set_gc_fence(100);
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    for i in 0..9u8 {
        spill_two_adds(
            &disk,
            1 + i * 10,
            10 + i32::from(i) * 10,
            100 + u64::from(i) * 100,
        );
    }
    assert_eq!(disk.segment_count(), 9);
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen_cb = std::sync::Arc::clone(&seen);
    let (tee, segs) = disk
        .compact_for_checkpoint_sync_with_sink(
            200,
            Some(move |kv: OutputKV| {
                if kv.is_add() {
                    seen_cb.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            }),
        )
        .expect("sink");
    assert_eq!(segs, 9, "nine cold segs at start (8 fan-in + leftover)");
    assert_eq!(tee, 18, "leftover after fan-in must still tee every Add");
    assert_eq!(seen.load(Ordering::Relaxed), 18);
    unsafe {
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
    }
    super::super::set_gc_fence(i32::MAX);
    std::mem::forget(tmp);
}

/// dest-bc 8×300M AllCold/FanIn would ENOSPC (144G extra). Checkpoint must
/// pair-drain (peak two megas) then tee-scan leftover chunks.
#[serial_test::serial(ibd)]
#[test]
fn dest_bc_checkpoint_must_not_allcold_eight_megas() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
    }
    super::super::set_gc_fence(100);
    TEST_MAX_COMPACT_WRITE_INPUT.store(0, Ordering::Relaxed);
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let mk = |i: u8| {
        let mut k = [0u8; 36];
        k[0] = i;
        k
    };
    let mut expect = 0usize;
    for s in 0u8..8 {
        let entries: Vec<OutputKV> = (1u8..=9)
            .map(|j| {
                expect += 1;
                OutputKV::new_add(mk(s * 10 + j), 10, 2000 + u64::from(s * 10 + j))
            })
            .collect();
        disk.push_run_no_compact(MemoryRun::build(entries))
            .expect("mega");
    }
    assert_eq!(disk.segment_count(), 8);
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen_cb = std::sync::Arc::clone(&seen);
    let (tee, segs) = disk
        .compact_for_checkpoint_sync_with_sink(
            50,
            Some(move |kv: OutputKV| {
                if kv.is_add() {
                    seen_cb.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            }),
        )
        .expect("sink");
    assert_eq!(segs, 8);
    assert_eq!(tee, expect as u64, "tee-scan must visit every dest-bc Add");
    assert_eq!(seen.load(Ordering::Relaxed), expect);
    let peak = TEST_MAX_COMPACT_WRITE_INPUT.load(Ordering::Relaxed);
    assert!(
        peak <= 18,
        "checkpoint write must be a stall pair (2×9), not 8-way/AllCold 72, peak={peak}"
    );
    for s in disk.segments.read().iter() {
        assert!(
            s.entry_count <= 4,
            "pair drain must split dest-bc megas, leftover {}",
            s.entry_count
        );
    }
    unsafe {
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    super::super::set_gc_fence(i32::MAX);
    std::mem::forget(tmp);
}

/// dest-bc pair of old Adds vs newest Deletes can tee 0. Drain must keep
/// eating middle megas (`wrote==0` used to stop).
#[serial_test::serial(ibd)]
#[test]
fn dest_bc_checkpoint_drain_continues_after_pair_gcs_all_adds() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
        std::env::set_var("BLVM_IBD_COMPACT_MAX_ENTRIES", "4");
    }
    super::super::set_gc_fence(100);
    let tmp = tempfile::tempdir().expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let mk = |i: u8| {
        let mut k = [0u8; 36];
        k[0] = i;
        k
    };
    disk.push_run_no_compact(MemoryRun::build(
        (1u8..=9)
            .map(|i| OutputKV::new_add(mk(i), 10, 1000 + u64::from(i)))
            .collect(),
    ))
    .expect("old adds mega");
    for s in 1u8..=2 {
        disk.push_run_no_compact(MemoryRun::build(
            (1u8..=9)
                .map(|j| {
                    let i = s * 10 + j;
                    OutputKV::new_add(mk(i), 10, 2000 + u64::from(i))
                })
                .collect(),
        ))
        .expect("middle mega");
    }
    disk.push_run_no_compact(MemoryRun::build(
        (1u8..=9).map(|i| OutputKV::new_delete(mk(i), 50)).collect(),
    ))
    .expect("newest deletes mega");
    assert_eq!(disk.segment_count(), 4);
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen_cb = std::sync::Arc::clone(&seen);
    let (tee, _) = disk
        .compact_for_checkpoint_sync_with_sink(
            50,
            Some(move |kv: OutputKV| {
                if kv.is_add() {
                    seen_cb.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            }),
        )
        .expect("sink");
    assert_eq!(tee, 18, "spent keys 1-9 GC; middle 18 Adds must still tee");
    assert_eq!(seen.load(Ordering::Relaxed), 18);
    for s in disk.segments.read().iter() {
        assert!(
            s.entry_count <= 4,
            "middle dest-bc megas must drain after a 0-tee pair, leftover {}",
            s.entry_count
        );
    }
    let mut spent = [OutputId::MAX];
    disk.batch_query(&[mk(1)], &mut spent, 40);
    assert_eq!(spent[0], OutputId::MAX, "GCed Add must stay spent");
    unsafe {
        std::env::remove_var("BLVM_IBD_COMPACT_MAX_ENTRIES");
    }
    super::super::set_gc_fence(i32::MAX);
    std::mem::forget(tmp);
}

/// dest-ba 650k shape: `n_segs` × 20M (or `BLVM_IBD_SYNTH_SEG_ENTRIES`) on disk,
/// then `batch_query` dest-ba cand counts. This is the KEEP **measurement**, not a
/// stall-pair unit. `#[ignore]` — ~9 G artifact, not CI.
///
/// Gauge: dest-ba 650k `preads=240` / `disk_ms=8` at 244 cands; dest-ba 660k
/// `preads=2138` / `disk_ms=33` at 2219 cands. Falsify if preads ≫ cands after
/// blooms (union FPR from extra segs) or KiB/pread ≫ dest-ba ~33 at 20M.
#[serial_test::serial(ibd)]
#[ignore]
#[test]
fn dest_ba_shape_synthetic_pread_budget() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_SPILL_MAX_ENTRIES");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
    }
    let per_seg: usize = std::env::var("BLVM_IBD_SYNTH_SEG_ENTRIES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000_000);
    let n_segs: usize = std::env::var("BLVM_IBD_SYNTH_N_SEGS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let tmp = tempfile::Builder::new()
        .prefix("diskindex-synth-")
        .tempdir()
        .expect("tempdir");
    let (disk, _) = DiskIndex::new_empty(tmp.path()).expect("DiskIndex");
    let disk = Arc::new(disk);
    let mk = |i: u64| {
        let mut k = [0u8; 36];
        // Spread into the directory prefix (first 18–20 bits). Sequential
        // u64-in-low-bytes put every dest-ba-sized key in bucket 0 and F19
        // coalesced the whole 20M file (1 GiB/pread) — not dest-ba's layout.
        let mixed = i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        k[..8].copy_from_slice(&mixed.to_be_bytes());
        k
    };
    let t_write = std::time::Instant::now();
    for s in 0..n_segs {
        let base = (s as u64) * (per_seg as u64);
        let mut entries: Vec<OutputKV> = (0..per_seg as u64)
            .map(|j| OutputKV::new_add(mk(base + j), 100, base + j + 1))
            .collect();
        entries.sort_unstable();
        disk.push_run_no_compact(MemoryRun::build_presorted(entries))
            .expect("spill");
        eprintln!(
            "synth wrote seg {}/{} entries={} segs_on_disk={}",
            s + 1,
            n_segs,
            per_seg,
            disk.segment_count()
        );
    }
    let write_s = t_write.elapsed().as_secs_f64();
    assert_eq!(
        disk.segment_count(),
        n_segs,
        "must not compact during spill"
    );
    let ram = disk.bloom_bytes_total();
    let bits = super::super::memory_run::directory_prefix_bits(per_seg);
    let bucket = per_seg / (1usize << bits);
    let kb_bucket = bucket * OutputKV::SIZE / 1024;
    eprintln!(
        "synth_index segs={} per_seg={} write_s={:.1} ram_miB={:.1} prefix_bits={} kb_per_bucket={}",
        n_segs,
        per_seg,
        write_s,
        ram as f64 / (1024.0 * 1024.0),
        bits,
        kb_bucket
    );

    let query = |label: &str, keys: Vec<[u8; 36]>| {
        let n = keys.len();
        let mut ids = vec![OutputId::MAX; n];
        super::super::disk_segment::reset_disk_io_stats();
        let t0 = std::time::Instant::now();
        disk.batch_query(&keys, &mut ids, 10_000);
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let (preads, pread_kb, max_kb, cands, segs) =
            super::super::disk_segment::take_disk_io_stats();
        let hits = ids.iter().filter(|id| **id != OutputId::MAX).count();
        let kib_per = if preads > 0 {
            pread_kb as f64 / preads as f64
        } else {
            0.0
        };
        eprintln!(
            "synth_query {label} keys={n} hits={hits} disk_ms={ms:.1} preads={preads} cands={cands} segs={segs} pread_kb={pread_kb} max_pread_kb={max_kb} kib/pread={kib_per:.1}"
        );
        (preads, cands, ms)
    };

    // dest-ba 650k cand count: keys that live in the newest seg (best case).
    let newest_base = ((n_segs - 1) as u64) * (per_seg as u64);
    let k244: Vec<_> = (0..244u64).map(|j| mk(newest_base + j)).collect();
    let (p244, c244, ms244) = query("244_newest", k244);
    let k2219: Vec<_> = (0..2219u64).map(|j| mk(newest_base + j)).collect();
    let (p2219, c2219, ms2219) = query("2219_newest", k2219);
    // Oldest-seg hits: newest-to-oldest walk; blooms should skip 7 segs.
    let k_old: Vec<_> = (0..244u64).map(mk).collect();
    let (p_old, c_old, ms_old) = query("244_oldest", k_old);
    // Total misses: union FPR across n_segs.
    let miss_base = (n_segs as u64) * (per_seg as u64) + 1_000_000;
    let k_miss: Vec<_> = (0..244u64).map(|j| mk(miss_base + j)).collect();
    let (p_miss, c_miss, ms_miss) = query("244_miss", k_miss);

    eprintln!(
        "synth_vs_dest_ba 650k_preads_gauge=240 got_244_newest={p244} cands={c244} disk_ms={ms244:.1}"
    );
    eprintln!(
        "synth_vs_dest_ba 660k_preads_gauge=2138 got_2219_newest={p2219} cands={c2219} disk_ms={ms2219:.1}"
    );
    eprintln!(
        "synth_fpr oldest_preads={p_old} oldest_cands={c_old} miss_preads={p_miss} miss_cands={c_miss} oldest_ms={ms_old:.1} miss_ms={ms_miss:.1}"
    );

    // Blooms must keep miss preads ≪ n_segs × keys (otherwise 20M cap without GC is net negative).
    assert!(
        p_miss < 244 * n_segs as u64 / 4,
        "union FPR too high: miss preads={p_miss} vs 8×244"
    );
    assert!(p244 > 0 && p2219 > 0, "expected disk preads on hits");
    drop(tmp);
}

/// One large segment at the count-policy file size (70–150M). Prefix 20 must
/// hold bucket cost vs dest-bc 776M @ prefix 16 (251 KiB, 6374 preads).
/// `#[ignore]` — ~4.5 GiB artifact. Keys are prefix-sorted so the directory
/// spreads (sequential low-bytes would pack bucket 0).
#[serial_test::serial(ibd)]
#[ignore]
#[test]
fn prefix20_large_seg_pread_budget() {
    let _guard = hot_pin_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_DISK_MMAP");
        std::env::remove_var("BLVM_IBD_ASYNC_DISK_SPILL");
        std::env::remove_var("BLVM_IBD_DISK_FAN_IN");
    }
    let n: usize = std::env::var("BLVM_IBD_SYNTH_SEG_ENTRIES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(80_000_000);
    let buckets = 1usize << super::super::memory_run::DIRECTORY_PREFIX_BITS_MAX;
    let per_bucket = n.div_ceil(buckets).max(1);
    let mk = |i: u64| {
        let mut k = [0u8; 36];
        let p = i / per_bucket as u64;
        let rem = i % per_bucket as u64;
        k[0] = (p >> 12) as u8;
        k[1] = (p >> 4) as u8;
        k[2] = ((p & 0xF) << 4) as u8;
        k[3] = rem as u8;
        k
    };
    let tmp = tempfile::Builder::new()
        .prefix("prefix20-large-")
        .tempdir()
        .expect("tempdir");
    let t_write = std::time::Instant::now();
    let iter = (0..n as u64).map(|i| OutputKV::new_add(mk(i), 100, i + 1));
    let seg = super::super::disk_segment::DiskSegment::write_from_iter(tmp.path(), 0, n, iter)
        .expect("write 80M");
    let write_s = t_write.elapsed().as_secs_f64();
    let bits = super::super::memory_run::directory_prefix_bits(seg.entry_count);
    let bucket = seg.entry_count / (1usize << bits);
    let kib_bucket = bucket * OutputKV::SIZE / 1024;
    eprintln!(
        "prefix20_seg entries={} write_s={:.1} prefix_bits={} entries/bucket={} kib/bucket={} file_mib={:.1}",
        seg.entry_count,
        write_s,
        bits,
        bucket,
        kib_bucket,
        (seg.entry_count * OutputKV::SIZE) as f64 / (1024.0 * 1024.0)
    );

    let query = |label: &str, keys: Vec<[u8; 36]>| {
        let n = keys.len();
        let mut ids = vec![OutputId::MAX; n];
        super::super::disk_segment::reset_disk_io_stats();
        let t0 = std::time::Instant::now();
        seg.batch_lookup(&keys, &mut ids, 0, 10_000)
            .expect("lookup");
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let (preads, pread_kb, max_kb, cands, segs) =
            super::super::disk_segment::take_disk_io_stats();
        let hits = ids.iter().filter(|id| **id != OutputId::MAX).count();
        let kib_per = if preads > 0 {
            pread_kb as f64 / preads as f64
        } else {
            0.0
        };
        eprintln!(
            "prefix20_query {label} keys={n} hits={hits} disk_ms={ms:.1} preads={preads} \
                 cands={cands} segs={segs} pread_kb={pread_kb} max_pread_kb={max_kb} kib/pread={kib_per:.1}"
        );
        (preads, cands, ms, kib_per)
    };

    let k244: Vec<_> = (0..244u64).map(|j| mk(j * (n as u64 / 244))).collect();
    let (p244, c244, ms244, kib244) = query("244_spread", k244);
    let k2219: Vec<_> = (0..2219u64).map(|j| mk(j * (n as u64 / 2219))).collect();
    let (p2219, c2219, ms2219, kib2219) = query("2219_spread", k2219);
    // High prefix 0xFF.. is past the 80M span (p < 2^20-1). Do not use
    // mk(n+…) — that wraps the 20-bit prefix and collides with live keys.
    let k_miss: Vec<_> = (0..244u64)
        .map(|j| {
            let mut k = [0xFFu8; 36];
            k[35] = j as u8;
            k
        })
        .collect();
    let (p_miss, c_miss, ms_miss, _) = query("244_miss", k_miss);

    eprintln!(
        "prefix20_vs_dest_bc dest_bc=251KiB/6374preads got_kib/bucket={kib_bucket} \
             244_preads={p244} cands={c244} disk_ms={ms244:.1} kib/pread={kib244:.1} \
             2219_preads={p2219} cands={c2219} disk_ms={ms2219:.1} kib/pread={kib2219:.1} \
             miss_preads={p_miss} miss_cands={c_miss} miss_ms={ms_miss:.1}"
    );
    assert_eq!(bits, 20, "80M must saturate prefix 20");
    assert!(
        kib_bucket <= 16,
        "prefix 20 must hold dest-ba-class buckets, got {kib_bucket} KiB"
    );
    assert!(p244 > 0 && p2219 > 0, "expected disk preads on hits");
    assert!(
        p_miss < 32,
        "bloom should skip almost all misses, preads={p_miss}"
    );
    drop(tmp);
}
