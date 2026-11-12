use super::super::types::{OutputId, OutputKV};
use super::*;

fn key_at(i: usize) -> [u8; 36] {
    let mut k = [0u8; 36];
    // Directory prefix is the first 4 bytes of the txid. Put the index in k[0]
    // so 8192 sorted keys spread across ~128 prefixes (~64 entries / ~4 KiB).
    k[0] = (i / 32) as u8;
    k[1] = (i % 32) as u8;
    k[2..6].copy_from_slice(&(i as u32).to_be_bytes());
    k
}

/// dest-bc 660k: `preads≈cands` — F19 cannot shrink one directory `[lo,hi)`.
#[test]
fn dest_bc_single_directory_bucket_is_never_shrunk_by_f19() {
    let kv = OutputKV::SIZE;
    let dest_bc_avg = (250 * 1024) / kv;
    let cap_64 = ((64 * 1024) / kv).max(1);
    let spans = [(0, dest_bc_avg)];
    let capped = coalesce_pread_spans(&spans, cap_64);
    assert_eq!(capped.len(), 1);
    assert_eq!(
        capped[0].1.saturating_sub(capped[0].0),
        dest_bc_avg,
        "F19 must not shrink a single dest-bc-sized directory bucket"
    );
}

/// dest-bc 660k: adjacent directory buckets glued into ~250 KiB (avg) / 1755 KiB (max).
#[test]
fn dest_bc_660k_pread_glue_splits_under_64kib_cap() {
    let kv = OutputKV::SIZE;
    let dest_bc_avg_entries = (250 * 1024) / kv; // ~4571
    let cap_64 = ((64 * 1024) / kv).max(1);
    assert!(dest_bc_avg_entries > cap_64);

    // Directory buckets are ~80 entries (~4 KiB). dest-bc glue is chaining them.
    let piece = 80usize;
    let n = dest_bc_avg_entries / piece;
    let spans: Vec<(usize, usize)> = (0..n).map(|i| (i * piece, (i + 1) * piece)).collect();
    let unlimited = coalesce_pread_spans(&spans, usize::MAX);
    assert_eq!(unlimited.len(), 1);
    assert_eq!(unlimited[0].0, 0);
    assert_eq!(unlimited[0].1, n * piece);
    assert!(
        unlimited[0].1.saturating_sub(unlimited[0].0) > cap_64,
        "unlimited dest-bc glue must exceed F19 64 KiB"
    );

    let capped = coalesce_pread_spans(&spans, cap_64);
    assert!(
        capped.len() >= 3,
        "64 KiB cap must split dest-bc 250 KiB glue, got {} ranges",
        capped.len()
    );
    for (lo, hi, _, _) in &capped {
        assert!(hi.saturating_sub(*lo) <= cap_64);
    }

    // dest-bc max_pread_kb=1755: 4 KiB buckets chained vs 64 KiB.
    let max_entries = (1755 * 1024) / kv;
    let n_max = max_entries / piece;
    let chain: Vec<(usize, usize)> = (0..n_max).map(|i| (i * piece, (i + 1) * piece)).collect();
    let unlimited_max = coalesce_pread_spans(&chain, usize::MAX);
    assert_eq!(unlimited_max.len(), 1);
    let capped_max = coalesce_pread_spans(&chain, cap_64);
    assert!(
        capped_max.len() >= 10,
        "1755 KiB dest-bc max must split into many 64 KiB ranges, got {}",
        capped_max.len()
    );
    for (lo, hi, _, _) in &capped_max {
        assert!(hi.saturating_sub(*lo) <= cap_64);
    }
}

#[test]
fn dest_ba_660k_15kib_avg_stays_one_pread_under_64kib() {
    let kv = OutputKV::SIZE;
    let dest_ba_avg = (15 * 1024) / kv; // ~274
    let cap_64 = ((64 * 1024) / kv).max(1);
    let spans = [(0, 80), (80, dest_ba_avg)];
    let capped = coalesce_pread_spans(&spans, cap_64);
    assert_eq!(
        capped.len(),
        1,
        "dest-ba 660k 15 KiB average must still coalesce under 64 KiB"
    );
}

/// R-367: compact input stream advises DONTNEED per consumed range. Lookup pread does not.
#[serial_test::serial(ibd)]
#[test]
fn r367_compact_input_dontneed_counts_when_enabled() {
    unsafe {
        std::env::remove_var("BLVM_IBD_DISK_MMAP");
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::set_var("BLVM_IBD_COMPACT_DONTNEED", "0");
    }
    TEST_DONTNEED_CALLS.store(0, std::sync::atomic::Ordering::Relaxed);
    let tmp = tempfile::tempdir().expect("tempdir");
    let entries: Vec<OutputKV> = (0..10)
        .map(|i| OutputKV::new_add(key_at(i), 100, 1000 + i as u64))
        .collect();
    let seg = DiskSegment::write_from_slice(tmp.path(), 0, (100, 100), &entries)
        .expect("write_from_slice");
    let mut reader = seg.stream();
    let mut n = 0usize;
    while reader.advance().expect("advance").is_some() {
        n += 1;
    }
    assert_eq!(n, 10);
    assert_eq!(
        TEST_DONTNEED_CALLS.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "BLVM_IBD_COMPACT_DONTNEED=0 must not advise"
    );
    TEST_DONTNEED_CALLS.store(0, std::sync::atomic::Ordering::Relaxed);
    let keys: Vec<_> = (0..10).map(key_at).collect();
    let mut ids = vec![OutputId::MAX; keys.len()];
    seg.batch_lookup(&keys, &mut ids, 0, 200).expect("lookup");
    assert_eq!(
        TEST_DONTNEED_CALLS.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "lookup pread must not DONTNEED"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_COMPACT_DONTNEED");
    }
    TEST_DONTNEED_CALLS.store(0, std::sync::atomic::Ordering::Relaxed);
    let mut reader = seg.stream();
    while reader.advance().expect("advance").is_some() {}
    assert_eq!(
        TEST_DONTNEED_CALLS.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "unset must not advise"
    );
    unsafe {
        std::env::set_var("BLVM_IBD_COMPACT_DONTNEED", "1");
    }
    TEST_DONTNEED_CALLS.store(0, std::sync::atomic::Ordering::Relaxed);
    let mut reader = seg.stream();
    while reader.advance().expect("advance").is_some() {}
    assert_eq!(
        TEST_DONTNEED_CALLS.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "BLVM_IBD_COMPACT_DONTNEED=1 must advise the one consumed range"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_COMPACT_DONTNEED");
    }
    std::mem::forget(tmp);
}

#[serial_test::serial(ibd)]
#[test]
fn disk_pread_max_kb_default_unlimited_zero_unlimited() {
    unsafe {
        std::env::remove_var("BLVM_IBD_DISK_PREAD_MAX_KB");
        assert_eq!(disk_pread_max_kb_from_env(), 0);
        assert_eq!(disk_pread_max_entries(), usize::MAX);
        std::env::set_var("BLVM_IBD_DISK_PREAD_MAX_KB", "");
        assert_eq!(disk_pread_max_kb_from_env(), 0);
        std::env::set_var("BLVM_IBD_DISK_PREAD_MAX_KB", "0");
        assert_eq!(disk_pread_max_kb_from_env(), 0);
        std::env::set_var("BLVM_IBD_DISK_PREAD_MAX_KB", "64");
        assert_eq!(disk_pread_max_kb_from_env(), 64);
        std::env::remove_var("BLVM_IBD_DISK_PREAD_MAX_KB");
    }
}

/// Real segment: many evenly-spaced keys glue into one huge pread when unlimited;
/// 64 KiB cap splits bytes but still resolves every id.
#[serial_test::serial(ibd)]
#[test]
fn dest_bc_pread_cap_still_resolves_glued_keys() {
    unsafe {
        std::env::remove_var("BLVM_IBD_DISK_MMAP");
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::set_var("BLVM_IBD_DISK_PREAD_MAX_KB", "0");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    const N: usize = 8192;
    let entries: Vec<OutputKV> = (0..N)
        .map(|i| OutputKV::new_add(key_at(i), 100, 1000 + i as u64))
        .collect();
    let seg = DiskSegment::write_from_slice(tmp.path(), 0, (100, 100), &entries)
        .expect("write_from_slice");
    // ~100 keys across the first ~4.5k entries (~250 KiB of OutputKV).
    let query_idx: Vec<usize> = (0..100).map(|i| i * 45).filter(|&i| i < N).collect();
    let keys: Vec<_> = query_idx.iter().map(|&i| key_at(i)).collect();

    reset_disk_io_stats();
    let mut ids = vec![OutputId::MAX; keys.len()];
    seg.batch_lookup(&keys, &mut ids, 0, 200)
        .expect("unlimited");
    let (preads_unlim, kb_unlim, max_unlim, _, _) = take_disk_io_stats();
    for (j, &i) in query_idx.iter().enumerate() {
        assert_eq!(ids[j], 1000 + i as u64, "unlimited miss at {i}");
    }
    assert!(
        kb_unlim >= 200,
        "dest-bc-shaped unlimited glue should read hundreds of KiB, got {kb_unlim}"
    );

    unsafe {
        std::env::set_var("BLVM_IBD_DISK_PREAD_MAX_KB", "64");
    }
    reset_disk_io_stats();
    let mut ids_cap = vec![OutputId::MAX; keys.len()];
    seg.batch_lookup(&keys, &mut ids_cap, 0, 200)
        .expect("capped");
    let (preads_cap, _kb_cap, max_cap, _, _) = take_disk_io_stats();
    for (j, &i) in query_idx.iter().enumerate() {
        assert_eq!(ids_cap[j], 1000 + i as u64, "capped miss at {i}");
    }
    assert!(
        preads_cap >= preads_unlim,
        "64 KiB cap must not issue fewer preads ({preads_cap} vs unlimited {preads_unlim})"
    );
    assert!(
        max_cap <= 64,
        "capped max_pread_kb must be ≤64, got {max_cap} (unlimited max {max_unlim})"
    );

    unsafe {
        std::env::remove_var("BLVM_IBD_DISK_PREAD_MAX_KB");
    }
    std::mem::forget(tmp);
}

/// dest-bc pair GC writes `capacity=COMPACT_MAX` (20M) even when survivors are
/// 2M. Bloom must be sized to the actual write count, not the caller cap.
#[test]
fn dest_bc_write_from_iter_bloom_sized_to_actual_not_cap() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let entries: Vec<OutputKV> = (0u16..200)
        .map(|i| {
            let mut k = [0u8; 36];
            k[..2].copy_from_slice(&i.to_be_bytes());
            OutputKV::new_add(k, 1, u64::from(i))
        })
        .collect();
    let fat = BloomFilter::new_for_capacity(20_000).mem_bytes();
    let seg = DiskSegment::write_from_iter(tmp.path(), 0, 20_000, entries.into_iter())
        .expect("write_from_iter");
    assert!(
        seg.ram_bytes() < fat,
        "bloom must match 200 entries, not 20k cap: ram={} fat={fat}",
        seg.ram_bytes()
    );
    let mut k = [0u8; 36];
    k[..2].copy_from_slice(&0u16.to_be_bytes());
    let mut ids = [OutputId::MAX];
    seg.batch_lookup(&[k], &mut ids, 0, 10).expect("lookup");
    assert_eq!(ids[0], 0);
    std::mem::forget(tmp);
}

/// R-342: `probe_narrow` returns a window holding every equal-key entry, for every key
/// position (including runs straddling page edges) and for absent keys.
#[test]
fn r342_probe_narrow_contains_every_equal_key_run() {
    let page = 7usize;
    // Sorted keys with runs of 1–3 equal entries; k[0..4] fixed (one directory bucket).
    let mut keys: Vec<[u8; 36]> = Vec::new();
    for v in 0..600u32 {
        let mut k = [0u8; 36];
        k[0..4].copy_from_slice(&0xAB_CD_EF_01u32.to_be_bytes());
        k[32..36].copy_from_slice(&(v * 3).to_be_bytes());
        for _ in 0..(1 + (v % 3) as usize) {
            keys.push(k);
        }
    }
    let n = keys.len();
    let mut reads = 0usize;
    for v in 0..700u32 {
        let mut key = [0u8; 36];
        key[0..4].copy_from_slice(&0xAB_CD_EF_01u32.to_be_bytes());
        key[32..36].copy_from_slice(&(v * 3 - (v % 2) * 1).to_be_bytes()); // half absent
        let (lo, hi) = probe_narrow(0, n, page, &key, |a, b| {
            reads += 1;
            assert!(a < b && b <= n, "page {a}..{b}");
            Ok((keys[a], keys[b - 1]))
        })
        .expect("probe");
        assert!(lo <= hi && hi <= n);
        let first = keys.partition_point(|k| *k < key);
        let last = keys.partition_point(|k| *k <= key);
        if first < last {
            assert!(
                lo <= first && last <= hi,
                "run {first}..{last} not in {lo}..{hi} v={v}"
            );
        }
        assert!(hi - lo <= 5 * page, "window too wide {}", hi - lo);
    }
    assert!(reads <= 700 * 8, "too many page reads {reads}");
}

/// R-342: a fan-out transaction's bucket (one txid prefix, 8000 vouts ≈ 437 KiB) is
/// resolved through page probes: correct id, ≤ 64 KiB read per lookup, Delete honoured,
/// absent key stays MAX.
#[test]
fn r342_fat_bucket_probe_resolves_with_small_preads() {
    unsafe {
        std::env::remove_var("BLVM_IBD_DISK_MMAP");
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_DISK_PREAD_MAX_KB");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    const N: usize = 8000;
    let fat_key = |vout: u32| {
        let mut k = [0u8; 36];
        k[0..4].copy_from_slice(&0x7F_00_00_01u32.to_be_bytes());
        k[4..8].copy_from_slice(&0xDE_AD_BE_EFu32.to_be_bytes());
        k[32..36].copy_from_slice(&vout.to_be_bytes());
        k
    };
    let mut entries: Vec<OutputKV> = (0..N as u32)
        .map(|v| OutputKV::new_add(fat_key(v), 100, 5000 + v as u64))
        .collect();
    // vout 4242 spent in the same segment at a later height: Add then Delete.
    entries.push(OutputKV::new_delete(fat_key(4242), 150));
    // Some ordinary keys in other buckets.
    for i in 0..2000usize {
        entries.push(OutputKV::new_add(key_at(i), 100, 1000 + i as u64));
    }
    entries.sort();
    let seg = DiskSegment::write_from_slice(tmp.path(), 0, (100, 150), &entries)
        .expect("write_from_slice");
    assert!(
        disk_probe_min_entries() < N,
        "default probe threshold (64 KiB) must be below the fat bucket"
    );

    for &vout in &[0u32, 1, 4000, 4241, 4243, 7998, 7999] {
        reset_disk_io_stats();
        let mut ids = vec![OutputId::MAX; 1];
        seg.batch_lookup(&[fat_key(vout)], &mut ids, 0, 200)
            .expect("lookup");
        let (preads, kb, max_kb, cands, _) = take_disk_io_stats();
        assert_eq!(ids[0], 5000 + vout as u64, "vout {vout}");
        assert_eq!(cands, 1);
        assert!(
            kb <= 64,
            "vout {vout}: read {kb} KiB in {preads} preads (max {max_kb})"
        );
        assert!(
            preads >= 2,
            "vout {vout}: expected page probes, got {preads} preads"
        );
    }
    // Spent in-segment → DELETED sentinel.
    let mut ids = vec![OutputId::MAX; 1];
    seg.batch_lookup(&[fat_key(4242)], &mut ids, 0, 200)
        .expect("lookup");
    assert_eq!(ids[0], OUTPUT_ID_DELETED);
    // Height window excludes the Delete → the Add is visible.
    let mut ids = vec![OutputId::MAX; 1];
    seg.batch_lookup(&[fat_key(4242)], &mut ids, 0, 120)
        .expect("lookup");
    assert_eq!(ids[0], 5000 + 4242);
    // Absent vout (bloom may pass) → stays MAX.
    let mut ids = vec![OutputId::MAX; 1];
    seg.batch_lookup(&[fat_key(9_000_000)], &mut ids, 0, 200)
        .expect("lookup");
    assert_eq!(ids[0], OutputId::MAX);
    // Ordinary small bucket still resolves.
    let mut ids = vec![OutputId::MAX; 1];
    seg.batch_lookup(&[key_at(777)], &mut ids, 0, 200)
        .expect("lookup");
    assert_eq!(ids[0], 1000 + 777);
    std::mem::forget(tmp);
}

/// R-345: k candidates in one fat bucket (a block spending 6 outputs of one fan-out tx)
/// are glued into one range; each is probed, total read stays far under the bucket.
#[test]
#[serial_test::serial(ibd)]
fn r345_glued_fat_bucket_probes_each_candidate() {
    unsafe {
        std::env::remove_var("BLVM_IBD_DISK_MMAP");
        std::env::remove_var("BLVM_IBD_HOT_PIN");
        std::env::remove_var("BLVM_IBD_DISK_PREAD_MAX_KB");
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    const N: usize = 8000;
    let fat_key = |vout: u32| {
        let mut k = [0u8; 36];
        k[0..4].copy_from_slice(&0x7F_00_00_02u32.to_be_bytes());
        k[4..8].copy_from_slice(&0xCA_FE_BA_BEu32.to_be_bytes());
        k[32..36].copy_from_slice(&vout.to_be_bytes());
        k
    };
    let mut entries: Vec<OutputKV> = (0..N as u32)
        .map(|v| OutputKV::new_add(fat_key(v), 100, 7000 + v as u64))
        .collect();
    for i in 0..2000usize {
        entries.push(OutputKV::new_add(key_at(i), 100, 1000 + i as u64));
    }
    entries.sort();
    let seg = DiskSegment::write_from_slice(tmp.path(), 1, (100, 100), &entries)
        .expect("write_from_slice");
    let vouts: Vec<u32> = (0..6u32).map(|i| i * 1301 % N as u32).collect();
    let keys: Vec<_> = vouts.iter().map(|&v| fat_key(v)).collect();
    reset_disk_io_stats();
    let mut ids = vec![OutputId::MAX; keys.len()];
    seg.batch_lookup(&keys, &mut ids, 0, 200).expect("lookup");
    let (preads, kb, max_kb, cands, _) = take_disk_io_stats();
    for (j, &v) in vouts.iter().enumerate() {
        assert_eq!(ids[j], 7000 + v as u64, "vout {v}");
    }
    assert_eq!(cands, 6);
    let bucket_kb = (N * OutputKV::SIZE / 1024) as u64;
    assert!(
        kb < bucket_kb,
        "6 probed candidates read {kb} KiB in {preads} preads (bucket {bucket_kb} KiB)"
    );
    assert!(preads >= 6 * 3, "each candidate probed: {preads} preads");
    assert!(max_kb <= 24, "probe window max {max_kb} KiB");
    // 400 candidates in the same bucket (dense) → whole-bucket read is cheaper; still correct.
    let vouts2: Vec<u32> = (0..400u32).map(|i| i * 17 % N as u32).collect();
    let keys2: Vec<_> = vouts2.iter().map(|&v| fat_key(v)).collect();
    reset_disk_io_stats();
    let mut ids2 = vec![OutputId::MAX; keys2.len()];
    seg.batch_lookup(&keys2, &mut ids2, 0, 200)
        .expect("lookup dense");
    let (preads2, _kb2, _, cands2, _) = take_disk_io_stats();
    for (j, &v) in vouts2.iter().enumerate() {
        assert_eq!(ids2[j], 7000 + v as u64, "dense vout {v}");
    }
    assert_eq!(cands2, 400);
    assert_eq!(
        preads2, 1,
        "dense candidates fall back to one whole-bucket read"
    );
    std::mem::forget(tmp);
}
