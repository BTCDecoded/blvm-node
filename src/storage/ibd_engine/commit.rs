//! Commit slots for the UTXO engine.
//!
//! A directory with a readable commit file is the coin tip. The sidecar, the
//! heed watermark, and the export height are not. A directory with no commit
//! file keeps the old resume path.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::types::OutputKV;

pub const MAGIC: &[u8; 8] = b"BLVMC001";
pub const VERSION: u32 = 1;
pub const RECORD_SIZE: usize = 56;

const HEADER_LEN: usize = 92;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRecord {
    pub generation: u64,
    pub height: i32,
    pub tip_hash: [u8; 32],
    pub table_len: u64,
    pub wal_start: u64,
    pub wal_len: u64,
    pub undo_len: u64,
    pub segments: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preimage {
    pub table_len: u64,
    pub wal_len: u64,
    pub undo_len: u64,
}

#[derive(Debug)]
pub enum LoadCommit {
    Absent,
    Record(CommitRecord),
}

fn beside(table: &Path, suffix: &str) -> PathBuf {
    let mut p = table.as_os_str().to_owned();
    p.push(suffix);
    PathBuf::from(p)
}

fn commit_paths(table: &Path) -> (PathBuf, PathBuf) {
    (beside(table, ".commit_a"), beside(table, ".commit_b"))
}

pub fn wal_path(table: &Path) -> PathBuf {
    beside(table, ".commit_wal")
}

pub fn undo_path(table: &Path) -> PathBuf {
    beside(table, ".commit_undo")
}

fn parent_dir(table: &Path) -> &Path {
    table
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

pub fn engine_dir(table_path: &Path) -> Option<&Path> {
    table_path.parent().filter(|p| !p.as_os_str().is_empty())
}

fn encode(rec: &CommitRecord) -> Vec<u8> {
    let mut body = Vec::with_capacity(HEADER_LEN + 32);
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&VERSION.to_le_bytes());
    body.extend_from_slice(&rec.generation.to_le_bytes());
    body.extend_from_slice(&rec.height.to_le_bytes());
    body.extend_from_slice(&rec.tip_hash);
    body.extend_from_slice(&rec.table_len.to_le_bytes());
    body.extend_from_slice(&rec.wal_start.to_le_bytes());
    body.extend_from_slice(&rec.wal_len.to_le_bytes());
    body.extend_from_slice(&rec.undo_len.to_le_bytes());
    let n = u32::try_from(rec.segments.len()).unwrap_or(u32::MAX);
    body.extend_from_slice(&n.to_le_bytes());
    debug_assert_eq!(body.len(), HEADER_LEN);
    for name in &rec.segments {
        let bytes = name.as_bytes();
        let len = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
        body.extend_from_slice(&len.to_le_bytes());
        body.extend_from_slice(bytes);
    }
    let sum = Sha256::digest(&body);
    body.extend_from_slice(&sum);
    body
}

fn decode(bytes: &[u8]) -> Option<CommitRecord> {
    if bytes.len() < HEADER_LEN + 32 {
        return None;
    }
    let (body, sum) = bytes.split_at(bytes.len() - 32);
    let expect = Sha256::digest(body);
    if sum.len() != expect.len() || sum.iter().zip(expect.iter()).any(|(a, b)| a != b) {
        return None;
    }
    if body.len() < HEADER_LEN || &body[..8] != MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(body[8..12].try_into().ok()?);
    if version != VERSION {
        return None;
    }
    let generation = u64::from_le_bytes(body[12..20].try_into().ok()?);
    let height = i32::from_le_bytes(body[20..24].try_into().ok()?);
    let mut tip_hash = [0u8; 32];
    tip_hash.copy_from_slice(&body[24..56]);
    let table_len = u64::from_le_bytes(body[56..64].try_into().ok()?);
    let wal_start = u64::from_le_bytes(body[64..72].try_into().ok()?);
    let wal_len = u64::from_le_bytes(body[72..80].try_into().ok()?);
    let undo_len = u64::from_le_bytes(body[80..88].try_into().ok()?);
    let n = u32::from_le_bytes(body[88..92].try_into().ok()?) as usize;
    let mut segments = Vec::with_capacity(n);
    let mut at = HEADER_LEN;
    for _ in 0..n {
        if at + 2 > body.len() {
            return None;
        }
        let len = u16::from_le_bytes(body[at..at + 2].try_into().ok()?) as usize;
        at += 2;
        if at + len > body.len() {
            return None;
        }
        let name = std::str::from_utf8(&body[at..at + len]).ok()?.to_string();
        at += len;
        segments.push(name);
    }
    if at != body.len() {
        return None;
    }
    Some(CommitRecord {
        generation,
        height,
        tip_hash,
        table_len,
        wal_start,
        wal_len,
        undo_len,
        segments,
    })
}

fn read_slot(path: &Path) -> Option<CommitRecord> {
    let bytes = std::fs::read(path).ok()?;
    decode(&bytes)
}

/// Highest valid generation. `Absent` when neither slot exists. Error when a
/// slot file exists and neither one checksums as version 1.
pub fn load(dir: &Path) -> anyhow::Result<LoadCommit> {
    let (a, b) = commit_paths(dir);
    let ra = read_slot(&a);
    let rb = read_slot(&b);
    match (ra, rb) {
        (None, None) if !a.exists() && !b.exists() => Ok(LoadCommit::Absent),
        (None, None) => anyhow::bail!("commit slots torn or unknown version in {}", dir.display()),
        (Some(r), None) => Ok(LoadCommit::Record(r)),
        (None, Some(r)) => Ok(LoadCommit::Record(r)),
        (Some(left), Some(right)) => {
            Ok(LoadCommit::Record(if right.generation >= left.generation {
                right
            } else {
                left
            }))
        }
    }
}

pub fn winning_height(dir: &Path) -> Option<i32> {
    match load(dir).ok()? {
        LoadCommit::Record(r) => Some(r.height),
        LoadCommit::Absent => None,
    }
}

/// Unlink the winning slot so the other generation becomes the tip.
/// One file. The engine directory stays. Errors when there is no second slot,
/// and does not unlink the only slot.
pub fn demote_winner(table: &Path) -> anyhow::Result<i32> {
    let (a, b) = commit_paths(table);
    let ra = read_slot(&a);
    let rb = read_slot(&b);
    let (winner, kept_height) = match (ra, rb) {
        (Some(left), Some(right)) if right.generation >= left.generation => (b, left.height),
        (Some(left), Some(right)) => (a, right.height),
        _ => anyhow::bail!("commit has no previous slot"),
    };
    std::fs::remove_file(&winner)?;
    let _ = fsync_dir(parent_dir(table));
    Ok(kept_height)
}

fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Write the inactive slot, fsync it, then fsync the directory.
pub fn publish(dir: &Path, mut rec: CommitRecord) -> anyhow::Result<CommitRecord> {
    std::fs::create_dir_all(parent_dir(dir))?;
    let (a, b) = commit_paths(dir);
    let prev = match load(dir)? {
        LoadCommit::Record(r) => Some(r),
        LoadCommit::Absent => None,
    };
    rec.generation = prev
        .as_ref()
        .map(|p| p.generation.saturating_add(1))
        .unwrap_or(1);
    let inactive = match &prev {
        None => a.clone(),
        Some(p) => {
            if read_slot(&a).map(|r| r.generation) == Some(p.generation) {
                b.clone()
            } else {
                a.clone()
            }
        }
    };
    let bytes = encode(&rec);
    let tmp = inactive.with_extension("tmp");
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &inactive)?;
    fsync_dir(parent_dir(dir))?;
    Ok(rec)
}

pub fn encode_kv(kv: &OutputKV) -> [u8; RECORD_SIZE] {
    let mut out = [0u8; RECORD_SIZE];
    out[..36].copy_from_slice(&kv.key);
    out[36..40].copy_from_slice(&kv.height.to_le_bytes());
    out[40] = kv.op as u8;
    out[48..56].copy_from_slice(&kv.id.to_le_bytes());
    out
}

pub fn decode_kv(buf: &[u8; RECORD_SIZE]) -> OutputKV {
    let mut key = [0u8; 36];
    key.copy_from_slice(&buf[..36]);
    let height = i32::from_le_bytes(buf[36..40].try_into().unwrap());
    let op = buf[40] as i8;
    let id = u64::from_le_bytes(buf[48..56].try_into().unwrap());
    OutputKV {
        key,
        height,
        op,
        _pad: [0; 3],
        id,
    }
}

pub fn append_wal(dir: &Path, entries: &[OutputKV]) -> anyhow::Result<u64> {
    let path = wal_path(dir);
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    for e in entries {
        f.write_all(&encode_kv(e))?;
    }
    f.flush()?;
    Ok(f.metadata()?.len())
}

pub fn wal_len(dir: &Path) -> u64 {
    std::fs::metadata(wal_path(dir))
        .map(|m| m.len())
        .unwrap_or(0)
}

pub fn undo_len(dir: &Path) -> u64 {
    std::fs::metadata(undo_path(dir))
        .map(|m| m.len())
        .unwrap_or(0)
}

pub fn read_wal(dir: &Path, start: u64, end: u64) -> anyhow::Result<Vec<OutputKV>> {
    if end == start {
        return Ok(Vec::new());
    }
    if end < start || !(end - start).is_multiple_of(RECORD_SIZE as u64) {
        anyhow::bail!("wal range {start}..{end} is not a multiple of {RECORD_SIZE}");
    }
    let mut f = File::open(wal_path(dir))?;
    f.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; (end - start) as usize];
    f.read_exact(&mut buf)?;
    let mut out = Vec::with_capacity(buf.len() / RECORD_SIZE);
    for chunk in buf.chunks_exact(RECORD_SIZE) {
        let mut arr = [0u8; RECORD_SIZE];
        arr.copy_from_slice(chunk);
        out.push(decode_kv(&arr));
    }
    Ok(out)
}

/// `payload` is the already-encoded `BlockUndoLog`. The preimage is the
/// lengths from before this block.
pub fn append_undo(dir: &Path, pre: Preimage, payload: &[u8]) -> anyhow::Result<u64> {
    let path = undo_path(dir);
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    f.write_all(&pre.table_len.to_le_bytes())?;
    f.write_all(&pre.wal_len.to_le_bytes())?;
    f.write_all(&pre.undo_len.to_le_bytes())?;
    let n = u32::try_from(payload.len()).map_err(|_| anyhow::anyhow!("undo payload too large"))?;
    f.write_all(&n.to_le_bytes())?;
    f.write_all(payload)?;
    f.flush()?;
    Ok(f.metadata()?.len())
}

pub fn truncate_file(path: &Path, len: u64) -> std::io::Result<()> {
    if !path.exists() {
        if len == 0 {
            return Ok(());
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "truncate missing file",
        ));
    }
    let f = OpenOptions::new().write(true).open(path)?;
    f.set_len(len)?;
    f.sync_all()?;
    Ok(())
}

pub fn sync_wal_and_undo(dir: &Path) -> std::io::Result<()> {
    if wal_path(dir).exists() {
        OpenOptions::new()
            .write(true)
            .open(wal_path(dir))?
            .sync_all()?;
    }
    if undo_path(dir).exists() {
        OpenOptions::new()
            .write(true)
            .open(undo_path(dir))?
            .sync_all()?;
    }
    Ok(())
}

pub fn truncate_logs(dir: &Path, record: &CommitRecord) -> std::io::Result<()> {
    truncate_file(&wal_path(dir), record.wal_len)?;
    truncate_file(&undo_path(dir), record.undo_len)?;
    Ok(())
}

pub fn segment_names(seg_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(seg_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if name.starts_with("seg_") && name.ends_with(".bin") {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(dir: &Path) -> PathBuf {
        dir.join("utxo_table.bin")
    }

    fn rec(height: i32) -> CommitRecord {
        CommitRecord {
            generation: 0,
            height,
            tip_hash: [height as u8; 32],
            table_len: 0,
            wal_start: 0,
            wal_len: 0,
            undo_len: 0,
            segments: vec![],
        }
    }

    #[test]
    fn torn_inactive_slot_keeps_the_other_generation() {
        let dir = tempfile::tempdir().unwrap();
        publish(&table(dir.path()), rec(1)).unwrap();
        publish(&table(dir.path()), rec(2)).unwrap();
        std::fs::write(beside(&table(dir.path()), ".commit_b"), b"torn").unwrap();
        let LoadCommit::Record(got) = load(&table(dir.path())).unwrap() else {
            panic!("expected a record");
        };
        assert_eq!(got.height, 1);
    }

    #[test]
    fn both_slots_torn_refuses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(beside(&table(dir.path()), ".commit_a"), b"aa").unwrap();
        std::fs::write(beside(&table(dir.path()), ".commit_b"), b"bb").unwrap();
        assert!(load(&table(dir.path())).is_err());
    }

    #[test]
    fn absent_when_no_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            load(&table(dir.path())).unwrap(),
            LoadCommit::Absent
        ));
    }

    #[test]
    fn unknown_version_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = encode(&rec(3));
        bytes[8..12].copy_from_slice(&99u32.to_le_bytes());
        let sum = Sha256::digest(&bytes[..bytes.len() - 32]);
        let n = bytes.len();
        bytes[n - 32..].copy_from_slice(&sum);
        std::fs::write(beside(&table(dir.path()), ".commit_a"), bytes).unwrap();
        assert!(load(&table(dir.path())).is_err());
    }

    #[test]
    fn higher_generation_wins() {
        let dir = tempfile::tempdir().unwrap();
        publish(&table(dir.path()), rec(4)).unwrap();
        publish(&table(dir.path()), rec(5)).unwrap();
        let LoadCommit::Record(got) = load(&table(dir.path())).unwrap() else {
            panic!("record");
        };
        assert_eq!(got.height, 5);
        assert!(got.generation > 1);
    }

    #[test]
    fn wal_suffix_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let kv = OutputKV::new_add([7u8; 36], 1, 99);
        append_wal(&table(dir.path()), &[kv]).unwrap();
        let extra = OutputKV::new_add([8u8; 36], 2, 100);
        append_wal(&table(dir.path()), &[extra]).unwrap();
        let end = RECORD_SIZE as u64;
        truncate_file(&wal_path(&table(dir.path())), end).unwrap();
        let got = read_wal(&table(dir.path()), 0, end).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, 99);
    }

    #[test]
    fn demote_winner_keeps_the_previous_generation() {
        let dir = tempfile::tempdir().unwrap();
        publish(&table(dir.path()), rec(1)).unwrap();
        publish(&table(dir.path()), rec(2)).unwrap();
        let kept = demote_winner(&table(dir.path())).unwrap();
        assert_eq!(kept, 1);
        let LoadCommit::Record(got) = load(&table(dir.path())).unwrap() else {
            panic!("record");
        };
        assert_eq!(got.height, 1);
    }

    #[test]
    fn demote_refuses_when_only_one_slot() {
        let dir = tempfile::tempdir().unwrap();
        publish(&table(dir.path()), rec(1)).unwrap();
        assert!(demote_winner(&table(dir.path())).is_err());
        let LoadCommit::Record(got) = load(&table(dir.path())).unwrap() else {
            panic!("record");
        };
        assert_eq!(got.height, 1);
    }

    #[test]
    fn kv_roundtrip_keeps_delete_op() {
        let kv = OutputKV::new_delete([1u8; 36], 12);
        let back = decode_kv(&encode_kv(&kv));
        assert!(back.is_delete());
        assert_eq!(back.height, 12);
        assert_eq!(back.key, kv.key);
    }
}
