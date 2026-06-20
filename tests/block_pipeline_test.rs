//! Block pipeline: `filter_block_before_store` ModuleAPI routing during IBD flush.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use blvm_node::module::inter_module::api::ModuleAPI;
use blvm_node::module::inter_module::{ModuleApiRegistry, ModuleRouter};
use blvm_node::module::pipeline::{
    install_block_pipeline, reset_block_pipeline_for_tests, try_filter_block_before_store,
};
use blvm_node::module::traits::ModuleError;
use blvm_protocol::{
    Block, BlockHeader, OutPoint, Transaction, TransactionInput, TransactionOutput,
};
use serial_test::serial;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FilterBlockRequest {
    height: u64,
    block: Block,
    witnesses: Vec<Vec<blvm_protocol::segwit::Witness>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FilterBlockResponse {
    block: Block,
    witnesses: Vec<Vec<blvm_protocol::segwit::Witness>>,
    stripped_txids: Vec<String>,
    filtered: bool,
}

/// Test double: strips non-empty witness stacks (mimics selective-sync IBD filter).
struct StripWitnessApi;

#[async_trait]
impl ModuleAPI for StripWitnessApi {
    async fn handle_request(
        &self,
        method: &str,
        params: &[u8],
        _caller: &str,
    ) -> Result<Vec<u8>, ModuleError> {
        if method != "filter_block_before_store" {
            return Err(ModuleError::OperationError(format!(
                "unknown method {method}"
            )));
        }
        let mut req: FilterBlockRequest = bincode::deserialize(params)
            .map_err(|e| ModuleError::OperationError(format!("bad params: {e}")))?;
        let mut stripped = Vec::new();
        for (i, tx_witnesses) in req.witnesses.iter_mut().enumerate() {
            let had_data = tx_witnesses.iter().any(|stack| !stack.is_empty());
            if had_data {
                let tx =
                    req.block.transactions.get(i).ok_or_else(|| {
                        ModuleError::OperationError("tx index out of range".into())
                    })?;
                stripped.push(hex::encode(blvm_protocol::block::calculate_tx_id(tx)));
                *tx_witnesses = tx.inputs.iter().map(|_| Vec::new()).collect();
            }
        }
        let response = FilterBlockResponse {
            block: req.block,
            witnesses: req.witnesses,
            stripped_txids: stripped.clone(),
            filtered: !stripped.is_empty(),
        };
        Ok(bincode::serialize(&response)
            .map_err(|e| ModuleError::SerializationError(format!("serialize response: {e}")))?)
    }

    fn list_methods(&self) -> Vec<String> {
        vec!["filter_block_before_store".to_string()]
    }

    fn api_version(&self) -> u32 {
        1
    }
}

fn sample_block_with_witnesses() -> (Block, Arc<Vec<Vec<blvm_protocol::segwit::Witness>>>) {
    let coinbase = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [0; 32],
                index: 0xffffffff,
            },
            script_sig: vec![0x01, 0x00],
            sequence: 0xffffffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 50_0000_0000,
            script_pubkey: vec![0x51],
        }]
        .into(),
        lock_time: 0,
    };
    let flagged = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [1; 32],
                index: 0,
            },
            script_sig: vec![],
            sequence: 0xffffffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 1_000,
            script_pubkey: vec![0x51],
        }]
        .into(),
        lock_time: 0,
    };
    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0; 32],
            merkle_root: [0; 32],
            timestamp: 1,
            bits: 0x1d00ffff,
            nonce: 0,
        },
        transactions: vec![coinbase, flagged].into(),
    };
    let witnesses = Arc::new(vec![vec![vec![]], vec![vec![vec![0x01, 0x02, 0x03]]]]);
    (block, witnesses)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn block_pipeline_routes_filter_block_before_store() {
    reset_block_pipeline_for_tests();
    let (block, witnesses) = sample_block_with_witnesses();

    // Without pipeline installed: pass-through.
    let witnesses_clone = Arc::clone(&witnesses);
    let block_clone = block.clone();
    let (_, w_pass) = tokio::task::spawn_blocking(move || {
        try_filter_block_before_store(1, block_clone, witnesses_clone)
    })
    .await
    .expect("spawn_blocking");
    assert!(w_pass[1].iter().any(|stack| !stack.is_empty()));

    let registry = Arc::new(ModuleApiRegistry::new());
    let router = Arc::new(ModuleRouter::new(Arc::clone(&registry)));
    registry
        .register_api("selective-sync_test".to_string(), Arc::new(StripWitnessApi))
        .await
        .expect("register_api");
    install_block_pipeline(router);

    let (_, w_filtered) =
        tokio::task::spawn_blocking(move || try_filter_block_before_store(1, block, witnesses))
            .await
            .expect("spawn_blocking");
    assert!(w_filtered[1].iter().all(|stack| stack.is_empty()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn block_pipeline_fail_open_without_registered_module() {
    reset_block_pipeline_for_tests();
    let (block, witnesses) = sample_block_with_witnesses();
    let registry = Arc::new(ModuleApiRegistry::new());
    let router = Arc::new(ModuleRouter::new(Arc::clone(&registry)));
    install_block_pipeline(router);

    let (_, w) =
        tokio::task::spawn_blocking(move || try_filter_block_before_store(1, block, witnesses))
            .await
            .expect("spawn_blocking");
    assert!(
        w[1].iter().any(|stack| !stack.is_empty()),
        "expected fail-open pass-through when no module registers filter_block_before_store"
    );
}

/// Module that never completes — forces the filter timeout / fail-open path.
struct HangFilterApi;

#[async_trait]
impl ModuleAPI for HangFilterApi {
    async fn handle_request(
        &self,
        method: &str,
        _params: &[u8],
        _caller: &str,
    ) -> Result<Vec<u8>, ModuleError> {
        if method != "filter_block_before_store" {
            return Err(ModuleError::OperationError(format!(
                "unknown method {method}"
            )));
        }
        std::future::pending::<()>().await;
        Ok(vec![])
    }

    fn list_methods(&self) -> Vec<String> {
        vec!["filter_block_before_store".to_string()]
    }

    fn api_version(&self) -> u32 {
        1
    }
}

/// Regression: filter must fail-open on timeout instead of blocking IBD flush threads
/// indefinitely when the module handler does not return.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn block_pipeline_filter_times_out_fail_open_when_module_hangs() {
    reset_block_pipeline_for_tests();
    let (block, witnesses) = sample_block_with_witnesses();
    let registry = Arc::new(ModuleApiRegistry::new());
    let router = Arc::new(ModuleRouter::new(Arc::clone(&registry)));
    registry
        .register_api("selective-sync_test".to_string(), Arc::new(HangFilterApi))
        .await
        .expect("register_api");
    install_block_pipeline(router);

    let start = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || try_filter_block_before_store(1, block, witnesses)),
    )
    .await
    .expect("filter must not block forever when module hangs");
    let (_, w) = result.expect("spawn_blocking");

    assert!(
        start.elapsed() >= Duration::from_secs(5),
        "expected to wait for filter timeout, took {:?}",
        start.elapsed()
    );
    assert!(
        start.elapsed() < Duration::from_secs(9),
        "expected fail-open within filter timeout, took {:?}",
        start.elapsed()
    );
    assert!(
        w[1].iter().any(|stack| !stack.is_empty()),
        "expected fail-open pass-through when filter times out"
    );
}
