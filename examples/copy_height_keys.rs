//! Height-key copy of headers/bodies/witnesses for Stage 0 local engine bench.
//! Does not copy warehouse chain_info or checkpoint trees.
//!
//! ```bash
//! cargo run --example copy_height_keys --release --features production,heed3,ibd-dev -- \
//!   /mnt/data/blvm-wan-bodies-400k /mnt/data/blvm-ibd-stage0-180-220k 180000 220000
//! cargo run --example copy_height_keys --release --features production,heed3,ibd-dev -- \
//!   --verify /mnt/data/blvm-ibd-stage0-180-220k 180000 220000
//! cargo run --example copy_height_keys --release --features production,heed3,ibd-dev -- \
//!   --restore-meta /mnt/data/blvm-engine-180k /mnt/data/blvm-ibd-stage0-180-220k
//! ```
use anyhow::{Context, Result, bail};
use blvm_node::storage::Storage;
use blvm_node::storage::blockstore::block_height_row_key;
use std::path::{Path, PathBuf};
use std::process::Command;

fn avail_gb(path: &Path) -> f64 {
    // Free space on the filesystem holding `path` (the destination), not a fixed mount.
    // The destination may not exist yet on the first call — use its nearest existing ancestor.
    let mut probe = path;
    while !probe.exists() {
        match probe.parent() {
            Some(p) => probe = p,
            None => break,
        }
    }
    let out = Command::new("df")
        .args(["-B1", "--output=avail"])
        .arg(probe)
        .output()
        .ok();
    let Some(out) = out else {
        return 0.0;
    };
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines()
        .nth(1)
        .and_then(|l| l.trim().parse::<f64>().ok())
        .map(|b| b / 1024.0 / 1024.0 / 1024.0)
        .unwrap_or(0.0)
}

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(|s| s.as_str()) == Some("--restore-meta") {
        args.remove(0);
        let src = PathBuf::from(
            args.first()
                .context("usage: copy_height_keys --restore-meta SRC_FIXTURE DST")?,
        );
        let dst = PathBuf::from(args.get(1).context("missing DST")?);
        return restore_meta(&src, &dst);
    }
    let verify = args.first().map(|s| s.as_str()) == Some("--verify");
    if verify {
        args.remove(0);
        let dst = PathBuf::from(
            args.first()
                .context("usage: copy_height_keys --verify DST_DATA_DIR START END")?,
        );
        let start: u64 = args.get(1).context("missing START")?.parse()?;
        let end: u64 = args.get(2).context("missing END")?.parse()?;
        return verify_range(&dst, start, end);
    }
    let src_dir = PathBuf::from(
        args.first()
            .context("usage: copy_height_keys SRC_DATA_DIR DST_DATA_DIR START END")?,
    );
    let dst_dir = PathBuf::from(args.get(1).context("missing DST")?);
    let start: u64 = args.get(2).context("missing START")?.parse()?;
    let end: u64 = args.get(3).context("missing END")?.parse()?;
    anyhow::ensure!(end >= start, "END < START");
    if src_dir == dst_dir {
        bail!("SRC and DST must differ");
    }
    let free = avail_gb(&dst_dir);
    eprintln!(
        "free_before={free:.1}G range={start}-{end} want={}",
        end - start + 1
    );
    if free < 24.0 {
        bail!(
            "ABORT: {free:.1}G free — Stage 0 copy must not push toward 8G stop (Slice A priority)"
        );
    }

    eprintln!("opening SRC {src_dir:?}");
    let src = Storage::new(&src_dir)?;
    eprintln!("opening DST {dst_dir:?}");
    let dst = Storage::new(&dst_dir)?;

    let s_hi = src.open_tree("height_index")?;
    let s_hdr = src.open_tree("headers")?;
    let s_blk = src.open_tree("blocks")?;
    let s_wit = src.open_tree("witnesses")?;
    let s_meta = src.open_tree("block_metadata")?;
    let d_hi = dst.open_tree("height_index")?;
    let d_h2h = dst.open_tree("hash_to_height")?;
    let d_hdr = dst.open_tree("headers")?;
    let d_blk = dst.open_tree("blocks")?;
    let d_wit = dst.open_tree("witnesses")?;
    let d_meta = dst.open_tree("block_metadata")?;

    let mut copied = 0u64;
    let mut missing_hash = 0u64;
    let mut missing_body = 0u64;
    let mut first_miss: Option<u64> = None;
    const BATCH: u64 = 128;
    let mut hi_b = d_hi.batch()?;
    let mut h2h_b = d_h2h.batch()?;
    let mut hdr_b = d_hdr.batch()?;
    let mut blk_b = d_blk.batch()?;
    let mut wit_b = d_wit.batch()?;
    let mut meta_b = d_meta.batch()?;

    for h in start..=end {
        if h % 2000 == 0 {
            let f = avail_gb(&dst_dir);
            eprintln!("h={h} copied={copied} miss_body={missing_body} free={f:.1}G");
            if f < 16.0 {
                bail!("ABORT: free {f:.1}G during copy (8G stop / Slice A)");
            }
        }
        let hb = h.to_be_bytes();
        let Some(hash_vec) = s_hi.get(&hb)? else {
            missing_hash += 1;
            if first_miss.is_none() {
                first_miss = Some(h);
            }
            continue;
        };
        if hash_vec.len() != 32 {
            missing_hash += 1;
            continue;
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&hash_vec);
        let row = block_height_row_key(h, &hash);
        hi_b.put(&hb, &hash_vec);
        h2h_b.put(&hash, &hb);
        if let Some(v) = s_hdr
            .get(row.as_slice())?
            .or(s_hdr.get(hash.as_slice())?)
        {
            hdr_b.put(row.as_slice(), &v);
            hdr_b.put(hash.as_slice(), &v);
        }
        match s_blk.get(row.as_slice())?.or(s_blk.get(hash.as_slice())?) {
            Some(v) => {
                blk_b.put(row.as_slice(), &v);
                copied += 1;
            }
            None => {
                missing_body += 1;
                if first_miss.is_none() {
                    first_miss = Some(h);
                }
            }
        }
        if let Some(v) = s_wit
            .get(row.as_slice())?
            .or(s_wit.get(hash.as_slice())?)
        {
            wit_b.put(row.as_slice(), &v);
        }
        if let Some(v) = s_meta
            .get(hash.as_slice())?
            .or(s_meta.get(row.as_slice())?)
        {
            meta_b.put(hash.as_slice(), &v);
        }

        if (h - start + 1) % BATCH == 0 {
            hi_b.commit()?;
            h2h_b.commit()?;
            hdr_b.commit()?;
            blk_b.commit()?;
            wit_b.commit()?;
            meta_b.commit()?;
            hi_b = d_hi.batch()?;
            h2h_b = d_h2h.batch()?;
            hdr_b = d_hdr.batch()?;
            blk_b = d_blk.batch()?;
            wit_b = d_wit.batch()?;
            meta_b = d_meta.batch()?;
        }
    }
    hi_b.commit()?;
    h2h_b.commit()?;
    hdr_b.commit()?;
    blk_b.commit()?;
    wit_b.commit()?;
    meta_b.commit()?;
    dst.flush()?;
    eprintln!(
        "done copied_bodies={copied} missing_hash={missing_hash} missing_body={missing_body} first_miss={first_miss:?} free={:.1}G",
        avail_gb(&dst_dir)
    );
    if missing_body > 0 || missing_hash > 0 {
        bail!("copy incomplete missing_hash={missing_hash} missing_body={missing_body}");
    }
    Ok(())
}

fn verify_range(dst_dir: &Path, start: u64, end: u64) -> Result<()> {
    let dst = Storage::new(dst_dir)?;
    let bs = dst.blocks();
    let mut ok = 0u64;
    let mut miss = 0u64;
    let mut first = None;
    for h in start..=end {
        let Some(hash) = bs.get_hash_by_height(h)? else {
            miss += 1;
            if first.is_none() {
                first = Some(("hash", h));
            }
            continue;
        };
        if bs.has_block_body(&hash)? {
            ok += 1;
        } else {
            miss += 1;
            if first.is_none() {
                first = Some(("body", h));
            }
        }
    }
    let want = end - start + 1;
    println!("verify ok={ok} miss={miss} want={want} first_miss={first:?}");
    if ok != want || miss != 0 {
        bail!("verify FAIL");
    }
    Ok(())
}

const META_TREES: &[&str] = &[
    "chain_info",
    "ibd_utxos",
    "ibd_utxos_ckpt_a",
    "ibd_utxos_ckpt_b",
    "spent_outputs",
    "chain_tips",
    "work_cache",
    "chainwork_cache",
    "utxo_stats_cache",
    "network_hashrate_cache",
];

fn restore_meta(src_dir: &Path, dst_dir: &Path) -> Result<()> {
    eprintln!("restore-meta SRC={src_dir:?} DST={dst_dir:?}");
    let src = Storage::new(src_dir)?;
    let dst = Storage::new(dst_dir)?;
    for name in META_TREES {
        let s = src.open_tree(name)?;
        let d = dst.open_tree(name)?;
        d.clear()?;
        let mut n = 0usize;
        let mut batch = d.batch()?;
        for kv in s.iter() {
            let (k, v) = kv?;
            batch.put(&k, &v);
            n += 1;
            if n % 10_000 == 0 {
                batch.commit()?;
                batch = d.batch()?;
            }
        }
        batch.commit()?;
        eprintln!("  {name} restored n={n}");
    }
    dst.flush()?;
    eprintln!("restore-meta done");
    Ok(())
}
