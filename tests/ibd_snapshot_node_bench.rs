//! Node-layer IBD snapshot benchmark.
//!
//! Exercises the FULL validation hot path used during real IBD:
//! - IbdUtxoStore with DashMap cache
//! - UTXO map building from prefetched data (single-pass)
//! - Witness Arc handling (pre-segwit empty witness reuse)
//! - validate_block_only → connect_block_ibd
//! - apply_utxo_delta (direct delta, no SyncBatch intermediate)
//! - block_input_keys_into (reusable buffer)
//!
//! Run:
//!   cargo test -p blvm-node --test ibd_snapshot_node_bench --features production --release -- --ignored bench_node_hot_path --nocapture

use blvm_consensus::block::{compute_block_tx_ids, connect_block_ibd};
use blvm_consensus::bip_validation::Bip30Index;
use blvm_consensus::segwit::Witness;
use blvm_consensus::types::{Block, Network, OutPoint, UTXO, UtxoSet};
use blvm_consensus::ValidationResult;
use blvm_node::storage::disk_utxo::{
    block_input_keys_into, key_to_outpoint, outpoint_to_key, OutPointKey,
};
use blvm_node::storage::ibd_utxo_store::IbdUtxoStore;
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

fn snapshot_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("BLVM_IBD_SNAPSHOT_DIR") {
        let p = PathBuf::from(d);
        if p.exists() {
            return Some(p);
        }
    }
    let candidates = [
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ibd-snapshots-20260307-192410"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

fn load_snapshot(
    dir: &Path,
) -> Option<(Block, Vec<Vec<Witness>>, UtxoSet)> {
    if !dir.join("block.bin").exists() {
        return None;
    }
    let block: Block =
        bincode::deserialize_from(std::io::BufReader::new(std::fs::File::open(dir.join("block.bin")).ok()?)).ok()?;
    let witnesses: Vec<Vec<Witness>> =
        bincode::deserialize_from(std::io::BufReader::new(std::fs::File::open(dir.join("witnesses.bin")).ok()?)).ok()?;
    let raw: std::collections::HashMap<OutPoint, UTXO> =
        bincode::deserialize_from(std::io::BufReader::new(std::fs::File::open(dir.join("utxo_set.bin")).ok()?)).ok()?;
    let utxo_set: UtxoSet = raw.into_iter().map(|(k, v)| (k, Arc::new(v))).collect();
    Some((block, witnesses, utxo_set))
}

/// Simulate the full node validation hot path for one block.
///
/// This mirrors the per-block code in parallel_ibd.rs validation loop:
/// 1. block_input_keys_into (reusable buf)
/// 2. Build UTXO map from prefetched data
/// 3. Build witness Arc (pre-segwit reuse)
/// 4. Precompute tx_ids
/// 5. connect_block_ibd
/// 6. apply_utxo_delta
#[inline(never)]
fn node_hot_path_once(
    store: &IbdUtxoStore,
    block: &Block,
    block_arc: &Arc<Block>,
    witnesses: &[Vec<Witness>],
    utxo_template: &UtxoSet,
    height: u64,
    keys_buf: &mut Vec<OutPointKey>,
    utxo_base_buf: &mut UtxoSet,
    keys_missing_buf: &mut Vec<OutPointKey>,
    supplement_cache_buf: &mut Vec<OutPointKey>,
    bip30_index: &mut Bip30Index,
) -> f64 {
    let t = Instant::now();

    // 1. Collect input keys (reusable buffer)
    block_input_keys_into(block, keys_buf);

    // 2. Build prefetched map (simulate what prefetch workers produce)
    let prefetched: FxHashMap<OutPointKey, Arc<UTXO>> = keys_buf
        .iter()
        .filter_map(|k| {
            let op = key_to_outpoint(k);
            utxo_template.get(&op).map(|arc| (*k, Arc::clone(arc)))
        })
        .collect();

    // 3. Build UTXO map from prefetch (clear + rebuild path)
    utxo_base_buf.clear();
    utxo_base_buf.reserve(keys_buf.len());
    keys_missing_buf.clear();
    for k in keys_buf.iter() {
        if let Some(arc) = prefetched.get(k) {
            utxo_base_buf.insert(key_to_outpoint(k), Arc::clone(arc));
        } else {
            keys_missing_buf.push(*k);
        }
    }
    if !keys_missing_buf.is_empty() {
        store.supplement_utxo_map_with_buf(utxo_base_buf, keys_missing_buf, supplement_cache_buf);
    }

    // 4. Build witness Arc (pre-segwit: empty witnesses via Arc)
    let witnesses_arc: Arc<Vec<Vec<Witness>>> = if witnesses.is_empty()
        || witnesses.iter().all(|w| w.iter().all(|v| v.is_empty()))
    {
        Arc::new(
            block
                .transactions
                .iter()
                .map(|tx| vec![Witness::default(); tx.inputs.len()])
                .collect(),
        )
    } else {
        Arc::new(witnesses.to_vec())
    };
    let witnesses_to_use: &[Vec<Witness>] = witnesses_arc.as_ref();

    // 5. Precompute tx_ids
    let tx_ids = compute_block_tx_ids(block);

    // 6. connect_block_ibd (the actual validation)
    let owned_utxo = std::mem::take(utxo_base_buf);
    let t_validate = Instant::now();
    let (result, new_utxo, _tx_ids_out, utxo_delta) = connect_block_ibd(
        block,
        witnesses_to_use,
        owned_utxo,
        height,
        None::<&[blvm_consensus::types::BlockHeader]>,
        0u64,
        Network::Mainnet,
        Some(bip30_index),
        Some(&tx_ids),
        Some(Arc::clone(block_arc)),
        Some(&witnesses_arc),
    )
    .expect("connect_block_ibd");
    let validate_ms = t_validate.elapsed().as_secs_f64() * 1000.0;
    *utxo_base_buf = new_utxo;

    match result {
        ValidationResult::Valid => {}
        ValidationResult::Invalid(reason) => panic!("height {} invalid: {}", height, reason),
    }

    // 7. apply_utxo_delta (direct, no SyncBatch)
    let t_delta = Instant::now();
    if let Some(ref delta) = utxo_delta {
        store.apply_utxo_delta(delta);
    }
    let delta_ms = t_delta.elapsed().as_secs_f64() * 1000.0;

    let total_ms = t.elapsed().as_secs_f64() * 1000.0;
    let prep_ms = total_ms - validate_ms - delta_ms;
    if height >= 100_000 && height % 50_000 == 0 {
        eprintln!("  [BREAKDOWN h={}] total={:.2}ms prep={:.2}ms validate={:.2}ms delta={:.2}ms",
            height, total_ms, prep_ms, validate_ms, delta_ms);
    }

    total_ms
}

#[test]
#[ignore = "Requires snapshot data"]
fn bench_node_hot_path() {
    let base = match snapshot_dir() {
        Some(d) => d,
        None => {
            eprintln!("Skip: no snapshot dir");
            return;
        }
    };
    let iterations: u32 = std::env::var("BENCH_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let height_filter: Option<u64> = std::env::var("BENCH_HEIGHT")
        .ok()
        .and_then(|s| s.parse().ok());

    let mut heights: Vec<u64> = std::fs::read_dir(&base)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_prefix("height_")?
                .parse()
                .ok()
        })
        .collect();
    heights.sort_unstable();
    if let Some(hf) = height_filter {
        heights.retain(|h| *h == hf);
    }

    let store = Arc::new(IbdUtxoStore::new_memory_only());

    // Reusable buffers (same as real IBD validation loop)
    let mut keys_buf: Vec<OutPointKey> = Vec::new();
    let mut utxo_base_buf: UtxoSet = UtxoSet::default();
    let mut keys_missing_buf: Vec<OutPointKey> = Vec::new();
    let mut supplement_cache_buf: Vec<OutPointKey> = Vec::new();
    let mut bip30_index = Bip30Index::default();

    eprintln!("=== Node Hot-Path Benchmark ({} iters) ===", iterations);
    eprintln!("height,txs,inputs,min_ms,median_ms,mean_ms,p95_ms,max_ms,bps");

    let mut focus_medians: Vec<f64> = Vec::new();

    for &h in &heights {
        let dir = base.join(format!("height_{}", h));
        let (block, mut witnesses, utxo_template) = match load_snapshot(&dir) {
            Some(x) => x,
            None => continue,
        };
        if witnesses.len() != block.transactions.len() {
            witnesses = block
                .transactions
                .iter()
                .map(|tx| (0..tx.inputs.len()).map(|_| Vec::new()).collect())
                .collect();
        }
        let block_arc = Arc::new(block.clone());
        let n_txs = block.transactions.len();
        let n_inputs: usize = block.transactions.iter().map(|tx| tx.inputs.len()).sum();

        // Seed the store's cache with UTXOs for this snapshot
        for (op, arc) in &utxo_template {
            let key = outpoint_to_key(op);
            store.cache_insert_and_track(key, Arc::clone(arc));
        }

        // Warmup
        let _ = node_hot_path_once(
            &store,
            &block,
            &block_arc,
            &witnesses,
            &utxo_template,
            h,
            &mut keys_buf,
            &mut utxo_base_buf,
            &mut keys_missing_buf,
            &mut supplement_cache_buf,
            &mut bip30_index,
        );

        let mut times: Vec<f64> = Vec::with_capacity(iterations as usize);
        for _ in 0..iterations {
            // Re-seed store for each iteration (delta applied previous iter)
            for (op, arc) in &utxo_template {
                let key = outpoint_to_key(op);
                store.cache_insert_and_track(key, Arc::clone(arc));
            }

            let ms = node_hot_path_once(
                &store,
                &block,
                &block_arc,
                &witnesses,
                &utxo_template,
                h,
                &mut keys_buf,
                &mut utxo_base_buf,
                &mut keys_missing_buf,
                &mut supplement_cache_buf,
                &mut bip30_index,
            );
            times.push(ms);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let min = times[0];
        let median = times[times.len() / 2];
        let mean = times.iter().sum::<f64>() / times.len() as f64;
        let p95 = times[(times.len() as f64 * 0.95) as usize];
        let max = *times.last().unwrap();
        let bps = 1000.0 / median;

        eprintln!(
            "{},{},{},{:.2},{:.2},{:.2},{:.2},{:.2},{:.0}",
            h, n_txs, n_inputs, min, median, mean, p95, max, bps
        );

        if h >= 100_000 {
            focus_medians.push(median);
        }
    }

    if !focus_medians.is_empty() {
        let avg = focus_medians.iter().sum::<f64>() / focus_medians.len() as f64;
        eprintln!("\n=== 100k+ Node Hot-Path Summary ===");
        eprintln!("avg median={:.2}ms ({:.0} bps)", avg, 1000.0 / avg);
    }
}
