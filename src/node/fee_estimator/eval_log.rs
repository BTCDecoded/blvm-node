//! One JSONL line per connected block, taken before that block updates history.
//!
//! Quotes are the published table (both modes, combined, history, and mempool,
//! targets 1, 3, 6, 12, and 24). The band is the 5th, 25th, 50th, and 75th
//! fee-rate percentile by weight of the non-coinbase transactions this
//! estimator was tracking. `external` is empty until a fee-table store exists.
//! The line is appended off the hook thread.

use super::TrackedTx;
use super::history::fee_sat_per_kvb;
use super::types::{BlockObservation, FeeMode, FeeTable, SourceView};
use blvm_protocol::Hash;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::Path;

pub(crate) const EVAL_TARGETS: [u32; 5] = [1, 3, 6, 12, 24];

pub(crate) fn record_line(
    obs: &BlockObservation,
    tracked: &HashMap<Hash, TrackedTx>,
    table: &FeeTable,
) -> String {
    let mut native = Map::new();
    for mode in [FeeMode::Conservative, FeeMode::Economical] {
        let mut views = Map::new();
        for view in [
            SourceView::Combined,
            SourceView::History,
            SourceView::Mempool,
        ] {
            let mut targets = Map::new();
            for target in EVAL_TARGETS {
                let value = match table.quote(target, mode, view) {
                    Ok(quote) => json!(quote.sat_per_kvb),
                    Err(_) => Value::Null,
                };
                targets.insert(target.to_string(), value);
            }
            views.insert(view_name(view).to_string(), Value::Object(targets));
        }
        native.insert(mode_name(mode).to_string(), Value::Object(views));
    }
    let line = json!({
        "height": obs.height,
        "hash": crate::storage::hashing::hash_to_rpc_hex(&obs.hash),
        "band": band_value(obs, tracked),
        "native": native,
        "external": [],
    });
    serde_json::to_string(&line).unwrap_or_else(|_| "{}".to_string())
}

pub(crate) fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    file.flush()
}

fn band_value(obs: &BlockObservation, tracked: &HashMap<Hash, TrackedTx>) -> Value {
    let Some(band) = fee_band(obs, tracked) else {
        return Value::Null;
    };
    json!({ "p5": band[0], "p25": band[1], "p50": band[2], "p75": band[3] })
}

/// `[p5, p25, p50, p75]` in sat/kvB. `None` when the block has no tracked fee.
pub(crate) fn fee_band(
    obs: &BlockObservation,
    tracked: &HashMap<Hash, TrackedTx>,
) -> Option<[u64; 4]> {
    let mut samples = Vec::new();
    for (index, (txid, weight)) in obs.tx_weights.iter().enumerate() {
        if *weight == 0 {
            continue;
        }
        let Some(tx) = tracked.get(txid) else {
            continue;
        };
        let Some(rate) = fee_sat_per_kvb(tx.fee_sats, tx.vsize) else {
            continue;
        };
        samples.push((rate, *weight, index));
    }
    if samples.is_empty() {
        return None;
    }
    samples.sort_by(|left, right| left.0.cmp(&right.0).then(left.2.cmp(&right.2)));
    let pairs: Vec<(u64, u64)> = samples
        .iter()
        .map(|(rate, weight, _)| (*rate, *weight))
        .collect();
    Some([
        weight_percentile(&pairs, 5)?,
        weight_percentile(&pairs, 25)?,
        weight_percentile(&pairs, 50)?,
        weight_percentile(&pairs, 75)?,
    ])
}

/// First sample whose cumulative weight reaches `pct` percent of the total.
fn weight_percentile(samples: &[(u64, u64)], pct: u64) -> Option<u64> {
    let total: u128 = samples.iter().map(|(_, weight)| u128::from(*weight)).sum();
    if total == 0 {
        return None;
    }
    let need = total.saturating_mul(u128::from(pct)) / 100;
    let mut cumulative = 0u128;
    for (rate, weight) in samples {
        cumulative = cumulative.saturating_add(u128::from(*weight));
        if cumulative >= need {
            return Some(*rate);
        }
    }
    samples.last().map(|(rate, _)| *rate)
}

fn mode_name(mode: FeeMode) -> &'static str {
    match mode {
        FeeMode::Conservative => "conservative",
        FeeMode::Economical => "economical",
    }
}

fn view_name(view: SourceView) -> &'static str {
    match view {
        SourceView::Combined => "combined",
        SourceView::History => "history",
        SourceView::Mempool => "mempool",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FeeEstimatorConfig;
    use crate::node::fee_estimator::{FeeEstimator, TxAdmission};
    use std::process::Command;
    use std::time::Duration;

    fn tracked_tx(fee: u64, vsize: u64) -> TrackedTx {
        TrackedTx {
            fee_sats: fee,
            vsize,
            admit_height: 0,
            parents: Vec::new(),
            history_eligible: true,
        }
    }

    #[test]
    fn the_band_is_the_weight_percentile_of_tracked_block_transactions() {
        let mut tracked = HashMap::new();
        tracked.insert([1u8; 32], tracked_tx(100, 100));
        tracked.insert([2u8; 32], tracked_tx(500, 100));
        tracked.insert([3u8; 32], tracked_tx(900, 100));
        let obs = BlockObservation {
            height: 1,
            hash: [9u8; 32],
            weight: 1_200,
            tx_weights: vec![([1u8; 32], 100), ([2u8; 32], 100), ([3u8; 32], 100)],
        };
        let band = fee_band(&obs, &tracked).unwrap();
        assert_eq!(band[0], 1_000);
        assert_eq!(band[2], 5_000);
        assert_eq!(band[3], 9_000);
    }

    #[tokio::test]
    async fn a_connected_block_logs_the_quote_from_before_that_block() {
        let dir = std::env::temp_dir().join(format!("blvm-fee-eval-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("eval.jsonl");
        let _ = std::fs::remove_file(&path);

        let mut cfg = FeeEstimatorConfig::default();
        cfg.health_required = Some(false);
        cfg.projection_full_threshold = 0.0;
        cfg.eval_log_path = Some(path.display().to_string());
        let estimator = FeeEstimator::new(cfg, true);
        let txid = [4u8; 32];
        estimator.on_tx_admitted(TxAdmission {
            txid,
            fee_sats: 1_000,
            vsize: 100,
            admit_height: 0,
            parents: Vec::new(),
            from_reload: false,
            from_package: false,
        });
        estimator.mark_table_stale_for_test();
        let before = estimator.table().await.unwrap();
        let expected = before
            .quote(1, FeeMode::Conservative, SourceView::Combined)
            .unwrap()
            .sat_per_kvb;

        estimator.on_block_connected(&BlockObservation {
            height: 1,
            hash: [5u8; 32],
            weight: 400,
            tx_weights: vec![(txid, 400)],
        });
        let mut text = String::new();
        for _ in 0..100 {
            if let Ok(body) = std::fs::read_to_string(&path) {
                if !body.is_empty() {
                    text = body;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!text.is_empty(), "evaluation log was not written");
        let record: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(record["height"].as_u64(), Some(1));
        assert_eq!(record["band"]["p5"].as_u64(), Some(10_000));
        assert_eq!(record["band"]["p75"].as_u64(), Some(10_000));
        assert_eq!(
            record["native"]["conservative"]["combined"]["1"].as_u64(),
            Some(expected)
        );
        assert!(record["external"].as_array().unwrap().is_empty());
        assert_eq!(estimator.hook_counts().confirmed, 1);

        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/score-fee-eval.py");
        let scored = Command::new("python3")
            .arg(&script)
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            scored.status.success(),
            "{}",
            String::from_utf8_lossy(&scored.stderr)
        );
        let stdout = String::from_utf8(scored.stdout).unwrap();
        assert!(
            stdout.contains("native conservative combined 1: over 0 within 1 under 0"),
            "{stdout}"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn the_scorer_counts_over_within_and_under() {
        let dir = std::env::temp_dir().join(format!("blvm-fee-eval-score-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("eval.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"height":1,"hash":"aa","band":{"p5":1000,"p25":2000,"p50":3000,"p75":5000},"native":{"conservative":{"combined":{"1":1000,"3":6000,"6":500,"12":null,"24":3000},"history":{"1":null,"3":null,"6":null,"12":null,"24":null},"mempool":{"1":null,"3":null,"6":null,"12":null,"24":null}},"economical":{"combined":{"1":null,"3":null,"6":null,"12":null,"24":null},"history":{"1":null,"3":null,"6":null,"12":null,"24":null},"mempool":{"1":null,"3":null,"6":null,"12":null,"24":null}}},"external":[{"estimator":"blend","quotes":{"1":100}},{"estimator":"blend-min","quotes":{"1":5000}}]}"#,
                "\n"
            ),
        )
        .unwrap();
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/score-fee-eval.py");
        let scored = Command::new("python3")
            .arg(&script)
            .arg(&path)
            .output()
            .unwrap();
        let stdout = String::from_utf8(scored.stdout).unwrap();
        assert!(
            scored.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&scored.stderr)
        );
        assert!(
            stdout.contains("native conservative combined 1: over 0 within 1 under 0"),
            "{stdout}"
        );
        assert!(
            stdout.contains("native conservative combined 3: over 1 within 0 under 0"),
            "{stdout}"
        );
        assert!(
            stdout.contains("native conservative combined 6: over 0 within 0 under 1"),
            "{stdout}"
        );
        assert!(
            stdout.contains("blend: over 0 within 0 under 1"),
            "{stdout}"
        );
        assert!(
            stdout.contains("blend-min: over 0 within 1 under 0"),
            "{stdout}"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
