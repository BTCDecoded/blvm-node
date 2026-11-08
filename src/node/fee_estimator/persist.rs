//! Estimator state file.
//!
//! `bincode` payload, written to a temp file in the same directory and renamed
//! over the destination. A missing file is empty state. Bad magic, a version
//! mismatch, a bucket-parameter mismatch, a decode error, or a height older
//! than `max_state_age_blocks` is logged and ignored. Unconfirmed transactions
//! are not stored.

use super::history::ExportedHistory;
use anyhow::Result;
use blvm_protocol::Hash;
use std::path::{Path, PathBuf};
use tracing::warn;

/// `b"BLVMFEE\0"`.
pub(crate) const MAGIC: [u8; 8] = *b"BLVMFEE\0";
/// First on-disk layout.
pub(crate) const VERSION: u32 = 1;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct PersistedState {
    pub magic: [u8; 8],
    pub version: u32,
    pub bucket_params_hash: u64,
    pub best_seen_height: u64,
    pub tip_hash: Hash,
    pub history: ExportedHistory,
    pub health: Vec<(u64, u64, u64)>,
}

pub(crate) fn write_atomic(path: &Path, state: &PersistedState) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let bytes = bincode::serialize(state)?;
    let tmp = temp_path(path);
    std::fs::write(&tmp, &bytes)?;
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err.into());
    }
    Ok(())
}

/// `Ok(None)` is a missing file. `Ok(Some(Err(reason)))` is a file this process
/// will not apply. `Err` is an I/O failure other than "not found".
pub(crate) fn read_state(
    path: &Path,
) -> Result<Option<std::result::Result<PersistedState, &'static str>>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let state: PersistedState = match bincode::deserialize(&bytes) {
        Ok(state) => state,
        Err(_) => return Ok(Some(Err("decode error"))),
    };
    if state.magic != MAGIC {
        return Ok(Some(Err("bad magic")));
    }
    if state.version != VERSION {
        return Ok(Some(Err("version mismatch")));
    }
    Ok(Some(Ok(state)))
}

pub(crate) fn age_exceeded(
    current_height: u64,
    best_seen_height: u64,
    max_state_age_blocks: u64,
) -> bool {
    current_height.saturating_sub(best_seen_height) > max_state_age_blocks
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("fee_estimator.dat"))
        .to_owned();
    name.push(".tmp");
    path.with_file_name(name)
}

pub(crate) fn warn_discard(path: &Path, reason: &str) {
    warn!(
        "fee estimator state at {} discarded ({reason}); continuing with empty state",
        path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FeeEstimatorConfig;
    use crate::node::fee_estimator::{BlockObservation, FeeEstimator, TxAdmission};
    use std::time::Duration;

    fn sample(path_tag: &str) -> (FeeEstimator, PathBuf) {
        let estimator = FeeEstimator::new(FeeEstimatorConfig::default(), true);
        let txid = [7u8; 32];
        estimator.on_tx_admitted(TxAdmission {
            txid,
            fee_sats: 500,
            vsize: 100,
            admit_height: 0,
            parents: Vec::new(),
            from_reload: false,
            from_package: false,
        });
        estimator.on_block_connected(&BlockObservation {
            height: 1,
            hash: [9u8; 32],
            weight: 4_000,
            tx_weights: vec![(txid, 400)],
        });
        let dir = std::env::temp_dir().join(format!(
            "blvm-fee-state-{}-{}-{path_tag}",
            std::process::id(),
            txid[0]
        ));
        let _ = std::fs::create_dir_all(&dir);
        (estimator, dir.join("fee_estimator.dat"))
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(temp_path(path));
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }

    #[test]
    fn a_round_trip_restores_history_health_and_best_height() {
        let (estimator, path) = sample("round");
        estimator.save_state(&path).unwrap();
        assert!(!temp_path(&path).exists());
        let loaded = FeeEstimator::new(FeeEstimatorConfig::default(), true);
        loaded.load_state(&path, 1).unwrap();
        assert_eq!(
            loaded.history_short_tx_count(),
            estimator.history_short_tx_count()
        );
        assert!(loaded.history_short_tx_count() > 0.0);
        loaded.on_block_connected(&BlockObservation {
            height: 1,
            hash: [9u8; 32],
            weight: 4_000,
            tx_weights: Vec::new(),
        });
        assert_eq!(loaded.hook_counts().ignored_blocks, 1);
        let health = loaded
            .inner
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .health
            .len();
        assert_eq!(health, 1);
        cleanup(&path);
    }

    #[test]
    fn corrupt_or_stale_files_leave_the_estimator_empty() {
        let (estimator, path) = sample("bad");
        estimator.save_state(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();

        std::fs::write(&path, &bytes[..4]).unwrap();
        let truncated = FeeEstimator::new(FeeEstimatorConfig::default(), true);
        truncated.load_state(&path, 1).unwrap();
        assert_eq!(truncated.history_short_tx_count(), 0.0);

        let mut state: PersistedState = bincode::deserialize(&bytes).unwrap();
        state.magic = *b"NOTAFEE\0";
        std::fs::write(&path, bincode::serialize(&state).unwrap()).unwrap();
        let magic = FeeEstimator::new(FeeEstimatorConfig::default(), true);
        magic.load_state(&path, 1).unwrap();
        assert_eq!(magic.history_short_tx_count(), 0.0);

        state.magic = MAGIC;
        state.version = VERSION + 1;
        std::fs::write(&path, bincode::serialize(&state).unwrap()).unwrap();
        let version = FeeEstimator::new(FeeEstimatorConfig::default(), true);
        version.load_state(&path, 1).unwrap();
        assert_eq!(version.history_short_tx_count(), 0.0);

        state.version = VERSION;
        state.bucket_params_hash ^= 1;
        state.history.bucket_params_hash ^= 1;
        std::fs::write(&path, bincode::serialize(&state).unwrap()).unwrap();
        let buckets = FeeEstimator::new(FeeEstimatorConfig::default(), true);
        buckets.load_state(&path, 1).unwrap();
        assert_eq!(buckets.history_short_tx_count(), 0.0);

        state.bucket_params_hash ^= 1;
        state.history.bucket_params_hash ^= 1;
        std::fs::write(&path, bincode::serialize(&state).unwrap()).unwrap();
        let stale = FeeEstimator::new(FeeEstimatorConfig::default(), true);
        stale.load_state(&path, 1 + 6048 + 1).unwrap();
        assert_eq!(stale.history_short_tx_count(), 0.0);

        let missing = FeeEstimator::new(FeeEstimatorConfig::default(), true);
        missing
            .load_state(Path::new("/tmp/blvm-fee-state-missing-no-such-file.dat"), 1)
            .unwrap();
        assert_eq!(missing.history_short_tx_count(), 0.0);
        cleanup(&path);
    }

    #[tokio::test]
    async fn an_interval_block_writes_the_file_off_the_caller() {
        let dir =
            std::env::temp_dir().join(format!("blvm-fee-state-interval-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("fee_estimator.dat");
        let estimator = FeeEstimator::new(FeeEstimatorConfig::default(), true);
        let mut cfg = FeeEstimatorConfig::default();
        cfg.persist_interval_blocks = 1;
        cfg.state_path = path.display().to_string();
        estimator.configure(cfg, true).unwrap();
        estimator.on_block_connected(&BlockObservation {
            height: 1,
            hash: [3u8; 32],
            weight: 4_000,
            tx_weights: Vec::new(),
        });
        for _ in 0..50 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(path.exists());
        cleanup(&path);
    }
}
