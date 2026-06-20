//! Generic block pipeline hooks via registered ModuleAPI methods.

use blvm_protocol::Block;
use blvm_protocol::segwit::Witness;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::{debug, warn};

use crate::module::inter_module::router::ModuleRouter;
use crate::module::traits::ModuleError;

pub const FILTER_BLOCK_BEFORE_STORE: &str = "filter_block_before_store";
const NODE_CALLER_ID: &str = "blvm-node";
const FILTER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FilterBlockRequest {
    height: u64,
    block: Block,
    witnesses: Vec<Vec<Witness>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FilterBlockResponse {
    block: Block,
    witnesses: Vec<Vec<Witness>>,
    stripped_txids: Vec<String>,
    filtered: bool,
}

static PIPELINE_ROUTER: RwLock<Option<Arc<ModuleRouter>>> = RwLock::new(None);
static RUNTIME_HANDLE: RwLock<Option<tokio::runtime::Handle>> = RwLock::new(None);

/// Install the block pipeline using the node's module router and current tokio runtime.
pub fn install_block_pipeline(router: Arc<ModuleRouter>) {
    *RUNTIME_HANDLE
        .write()
        .expect("pipeline runtime lock") = Some(tokio::runtime::Handle::current());
    *PIPELINE_ROUTER.write().expect("pipeline router lock") = Some(router);
    debug!("Block pipeline installed");
}

/// Test-only helper: clear installed pipeline state between integration tests.
#[doc(hidden)]
pub fn reset_block_pipeline_for_tests() {
    *PIPELINE_ROUTER.write().expect("pipeline router lock") = None;
    *RUNTIME_HANDLE.write().expect("pipeline runtime lock") = None;
}

/// Apply `filter_block_before_store` when a module registers it. Fail-open on errors/timeouts.
pub fn try_filter_block_before_store(
    height: u64,
    block: Block,
    witnesses: Arc<Vec<Vec<Witness>>>,
) -> (Block, Arc<Vec<Vec<Witness>>>) {
    let router = PIPELINE_ROUTER
        .read()
        .expect("pipeline router lock")
        .clone();
    let Some(router) = router else {
        return (block, witnesses);
    };
    let Some(runtime_handle) = RUNTIME_HANDLE
        .read()
        .expect("pipeline runtime lock")
        .clone()
    else {
        return (block, witnesses);
    };

    filter_sync(&runtime_handle, &router, height, block, witnesses)
}

fn filter_sync(
    runtime_handle: &tokio::runtime::Handle,
    router: &Arc<ModuleRouter>,
    height: u64,
    block: Block,
    witnesses: Arc<Vec<Vec<Witness>>>,
) -> (Block, Arc<Vec<Vec<Witness>>>) {
    let request = FilterBlockRequest {
        height,
        block: block.clone(),
        witnesses: witnesses.as_ref().clone(),
    };
    let params = match bincode::serialize(&request) {
        Ok(params) => params,
        Err(e) => {
            warn!("filter_block_before_store serialize failed at height {height}: {e}");
            return (block, witnesses);
        }
    };

    let router = Arc::clone(router);
    let fut = async move {
        router
            .route_call(NODE_CALLER_ID, None, FILTER_BLOCK_BEFORE_STORE, &params)
            .await
    };

    // Never `Handle::block_on` from the IBD validation / flush threads: if all Tokio
    // worker threads are busy (P2P, RPC, module IPC), `block_on` deadlocks and the
    // timeout future never runs. Queue on the runtime and wait on a std channel instead.
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    runtime_handle.spawn(async move {
        let result = tokio::time::timeout(FILTER_TIMEOUT, fut).await;
        let _ = tx.send(result);
    });
    let channel_wait = FILTER_TIMEOUT + Duration::from_secs(1);
    let response_bytes = match rx.recv_timeout(channel_wait) {
        Ok(Ok(Ok(bytes))) => bytes,
        Ok(Ok(Err(e))) => {
            if !matches!(
                &e,
                ModuleError::OperationError(msg) if msg.contains("not found")
            ) {
                warn!("filter_block_before_store failed-open at height {height}: {e}");
            }
            return (block, witnesses);
        }
        Ok(Err(_)) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            warn!(
                "filter_block_before_store timed out after {}s at height {height}; storing unfiltered",
                FILTER_TIMEOUT.as_secs()
            );
            return (block, witnesses);
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            warn!(
                "filter_block_before_store channel closed at height {height}; storing unfiltered"
            );
            return (block, witnesses);
        }
    };

    let response: FilterBlockResponse = match bincode::deserialize(&response_bytes) {
        Ok(response) => response,
        Err(e) => {
            warn!("filter_block_before_store bad response at height {height}: {e}");
            return (block, witnesses);
        }
    };

    if response.filtered {
        debug!(
            "filter_block_before_store height={height} stripped {} tx(s)",
            response.stripped_txids.len()
        );
    }

    (response.block, Arc::new(response.witnesses))
}
