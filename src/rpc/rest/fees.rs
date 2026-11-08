//! Fee estimation endpoints
//!
//! GET /api/v1/fees/estimate
//! GET /api/v1/fees/lnd

use crate::node::fee_estimator::{FeeMode, SourceView};
use crate::rpc::mining::MiningRpc;
use anyhow::Result;
use serde_json::{Map, Value, json};
use std::fmt;

/// The lnd `feeurl` body could not be filled.
#[derive(Debug)]
pub struct LndFeesUnavailable;

impl fmt::Display for LndFeesUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("fee estimates unavailable")
    }
}

/// Get fee estimate.
///
/// `mode` is `conservative` or `economical`. Absent uses the RPC default.
pub async fn get_fee_estimate(
    mining: &MiningRpc,
    blocks: Option<u64>,
    mode: Option<&str>,
) -> Result<Value> {
    let blocks = blocks.unwrap_or(6);
    let params = match mode {
        Some(mode) => json!([blocks, mode]),
        None => json!([blocks]),
    };
    let estimate = mining.estimate_smart_fee(&params).await?;
    Ok(estimate)
}

/// lnd `SparseConfFeeSource` body.
///
/// Values are the conservative combined quote in sat/kvB. That quote is already
/// an integer ceiling. Targets with no quote are omitted; lnd
/// `WebAPIEstimator.getCachedFee` walks down to `minBlockTarget` when a key is
/// missing. An empty map is unavailable.
pub async fn lnd_fee_estimate(mining: &MiningRpc) -> Result<Value, LndFeesUnavailable> {
    let Some(estimator) = mining.fee_estimator() else {
        return Err(LndFeesUnavailable);
    };
    if estimator
        .select_estimator(std::time::Instant::now())
        .is_err()
    {
        return Err(LndFeesUnavailable);
    }
    let table = estimator.table().await.map_err(|_| LndFeesUnavailable)?;
    let mut fee_by_block_target = Map::new();
    for target in estimator.lnd_targets() {
        let Ok(quote) = table.quote(target, FeeMode::Conservative, SourceView::Combined) else {
            continue;
        };
        fee_by_block_target.insert(target.to_string(), json!(quote.sat_per_kvb));
    }
    if fee_by_block_target.is_empty() {
        return Err(LndFeesUnavailable);
    }
    Ok(json!({
        "current_block_height": table.tip_height,
        "current_block_hash": crate::storage::hashing::hash_to_rpc_hex(&table.tip_hash),
        "fee_by_block_target": fee_by_block_target,
        "min_relay_feerate": estimator.relay_floor_sat_per_kvb(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_rest_helper_returns_the_rpc_json() {
        let mining = MiningRpc::new();
        let omitted = get_fee_estimate(&mining, Some(6), None).await.unwrap();
        let rpc_default = mining.estimate_smart_fee(&json!([6])).await.unwrap();
        assert_eq!(omitted, rpc_default);

        let economical = get_fee_estimate(&mining, Some(6), Some("economical"))
            .await
            .unwrap();
        let rpc_economical = mining
            .estimate_smart_fee(&json!([6, "economical"]))
            .await
            .unwrap();
        assert_eq!(economical, rpc_economical);
        assert_eq!(economical["blocks"].as_u64(), Some(6));
    }

    #[tokio::test]
    async fn an_empty_estimator_is_unavailable_and_a_quote_fills_every_configured_target() {
        use crate::config::FeeEstimatorConfig;
        use crate::node::fee_estimator::FeeEstimator;
        use crate::node::mempool::MempoolManager;
        use crate::storage::Storage;
        use blvm_protocol::opcodes::OP_1;
        use blvm_protocol::segwit::transaction_weight_from_stacks;
        use blvm_protocol::{
            OutPoint, Transaction, TransactionInput, TransactionOutput, UTXO, UtxoSet,
        };
        use std::sync::Arc;

        let mining = MiningRpc::new();
        assert!(lnd_fee_estimate(&mining).await.is_err());

        let dir = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(dir.path()).unwrap());
        let mempool = Arc::new(MempoolManager::new());
        let mut fees = FeeEstimatorConfig::default();
        fees.health_required = Some(false);
        fees.projection_full_threshold = 0.0;
        let estimator = Arc::new(FeeEstimator::new(fees, true));
        mempool.set_fee_estimator(Some(Arc::clone(&estimator)));
        let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
        policy.require_standard = false;
        policy.min_tx_fee = 0;
        policy.min_relay_fee_rate = 1_000;
        mempool.set_policy_config(Some(policy));

        let funding = OutPoint {
            hash: [6u8; 32],
            index: 0,
        };
        let mut tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: funding,
                script_sig: vec![OP_1],
                sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 50_000,
                script_pubkey: vec![0x00, 0x14].into_iter().chain([0u8; 20]).collect(),
            }]
            .into(),
            lock_time: 0,
        };
        let vsize = transaction_weight_from_stacks(&tx, None)
            .unwrap()
            .div_ceil(4)
            .max(1) as i64;
        tx.outputs[0].value = 50_000 - 10 * vsize;
        let utxo = UTXO {
            value: 50_000,
            script_pubkey: vec![OP_1].into(),
            height: 0,
            is_coinbase: false,
        };
        let mut set = UtxoSet::default();
        set.insert(funding, Arc::new(utxo));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));
        assert!(mempool.add_transaction(tx).unwrap());
        estimator.mark_table_stale_for_test();

        let mining = MiningRpc::with_dependencies(storage, mempool);
        let body = lnd_fee_estimate(&mining).await.unwrap();
        assert_eq!(body["current_block_height"].as_u64(), Some(0));
        assert_eq!(
            body["current_block_hash"].as_str(),
            Some("0000000000000000000000000000000000000000000000000000000000000000")
        );
        assert_eq!(body["min_relay_feerate"].as_u64(), Some(1_000));
        let targets = FeeEstimatorConfig::default().lnd_targets;
        assert!(targets.contains(&1));
        assert!(targets.contains(&1008));
        let map = body["fee_by_block_target"].as_object().unwrap();
        assert_eq!(map.len(), targets.len());
        for target in targets {
            assert_eq!(map[&target.to_string()].as_u64(), Some(10_000), "{map:?}");
        }
    }
}
