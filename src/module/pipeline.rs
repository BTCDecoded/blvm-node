//! Generic block pipeline hooks via registered ModuleAPI methods.

use blvm_protocol::Block;
use blvm_protocol::Hash;
use blvm_protocol::segwit::Witness;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::{debug, warn};

use crate::module::inter_module::router::ModuleRouter;
use crate::module::traits::ModuleError;
use crate::network::NetworkManager;

pub const FILTER_BLOCK_BEFORE_STORE: &str = "filter_block_before_store";
pub const REHYDRATE_BLOCK_FOR_CONSENSUS: &str = "rehydrate_block_for_consensus";
pub const GET_CANONICAL_TXIDS: &str = "get_canonical_txids";
pub const LOOKUP_BLOCK_FOR_TXIDS: &str = "lookup_block_for_txids";
pub const FILTER_BLOCK_DOWNLOAD_POLICY: &str = "filter_block_download_policy";
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RehydrateRequest {
    height: u64,
    block_hash: Hash,
    block: Block,
    witnesses: Vec<Vec<Witness>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RehydrateResponse {
    block: Block,
    witnesses: Vec<Vec<Witness>>,
    canonical_txids: Vec<String>,
    found: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CanonicalTxidsRequest {
    height: u64,
    block_hash: Hash,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CanonicalTxidsResponse {
    canonical_txids: Vec<String>,
    found: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LookupBlockRequest {
    txids: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LookupBlockResponse {
    block_hash: Option<Hash>,
    height: Option<u64>,
    found: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DownloadPolicyRequest {
    height: u64,
    block_hash: Hash,
    merkle_root: Hash,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DownloadPolicyResponse {
    skip_witness: bool,
    verified_paths: u32,
    bytes_saved_hint: u64,
}

static PIPELINE_ROUTER: RwLock<Option<Arc<ModuleRouter>>> = RwLock::new(None);
static RUNTIME_HANDLE: RwLock<Option<tokio::runtime::Handle>> = RwLock::new(None);
static NETWORK: RwLock<Option<Arc<NetworkManager>>> = RwLock::new(None);

/// Install the block pipeline using the node's module router and current tokio runtime.
pub fn install_block_pipeline(router: Arc<ModuleRouter>) {
    *RUNTIME_HANDLE.write().expect("pipeline runtime lock") =
        Some(tokio::runtime::Handle::current());
    *PIPELINE_ROUTER.write().expect("pipeline router lock") = Some(router);
    debug!("Block pipeline installed");
}

/// Use the node's existing GetData path for ephemeral full-block reads.
pub fn install_pipeline_network(network: Arc<NetworkManager>) {
    *NETWORK.write().expect("pipeline network lock") = Some(network);
}

/// Test-only helper: clear installed pipeline state between integration tests.
#[doc(hidden)]
pub fn reset_block_pipeline_for_tests() {
    *PIPELINE_ROUTER.write().expect("pipeline router lock") = None;
    *RUNTIME_HANDLE.write().expect("pipeline runtime lock") = None;
    *NETWORK.write().expect("pipeline network lock") = None;
}

fn pipeline_handles() -> Option<(Arc<ModuleRouter>, tokio::runtime::Handle)> {
    let router = PIPELINE_ROUTER
        .read()
        .expect("pipeline router lock")
        .clone()?;
    let runtime_handle = RUNTIME_HANDLE
        .read()
        .expect("pipeline runtime lock")
        .clone()?;
    Some((router, runtime_handle))
}

fn route_bytes(method: &str, params: Vec<u8>) -> Option<Vec<u8>> {
    let (router, runtime_handle) = pipeline_handles()?;
    let method = method.to_string();
    let fut = async move {
        router
            .route_call(NODE_CALLER_ID, None, &method, &params)
            .await
    };
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    runtime_handle.spawn(async move {
        let result = tokio::time::timeout(FILTER_TIMEOUT, fut).await;
        let _ = tx.send(result);
    });
    let channel_wait = FILTER_TIMEOUT + Duration::from_secs(1);
    match rx.recv_timeout(channel_wait) {
        Ok(Ok(Ok(bytes))) => Some(bytes),
        Ok(Ok(Err(e))) => {
            if !matches!(
                &e,
                ModuleError::OperationError(msg) if msg.contains("not found")
            ) {
                warn!("block pipeline call failed-open: {e}");
            }
            None
        }
        Ok(Err(_)) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            warn!("block pipeline call timed out; fail-open");
            None
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            warn!("block pipeline channel closed; fail-open");
            None
        }
    }
}

/// Apply `filter_block_before_store` when a module registers it. Fail-open on errors/timeouts.
pub fn try_filter_block_before_store(
    height: u64,
    block: Block,
    witnesses: Arc<Vec<Vec<Witness>>>,
) -> (Block, Arc<Vec<Vec<Witness>>>) {
    if pipeline_handles().is_none() {
        return (block, witnesses);
    }
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
    let Some(response_bytes) = route_bytes(FILTER_BLOCK_BEFORE_STORE, params) else {
        return (block, witnesses);
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

/// Consensus read of a stripped block: module never restores payloads; node GetData if needed.
pub fn try_rehydrate_block_for_consensus(
    height: u64,
    block_hash: Hash,
    block: Block,
    witnesses: Vec<Vec<Witness>>,
) -> (Block, Vec<Vec<Witness>>) {
    if let Some(handles) = pipeline_handles() {
        let request = RehydrateRequest {
            height,
            block_hash,
            block: block.clone(),
            witnesses: witnesses.clone(),
        };
        if let Ok(params) = bincode::serialize(&request) {
            if let Some(bytes) = route_bytes(REHYDRATE_BLOCK_FOR_CONSENSUS, params) {
                if let Ok(response) = bincode::deserialize::<RehydrateResponse>(&bytes) {
                    if response.found {
                        return (response.block, response.witnesses);
                    }
                }
            }
        }
        let _ = handles;
    }
    request_block_from_peers(block_hash).unwrap_or((block, witnesses))
}

fn request_block_from_peers(block_hash: Hash) -> Option<(Block, Vec<Vec<Witness>>)> {
    let network = NETWORK.read().ok()?.clone()?;
    let runtime_handle = RUNTIME_HANDLE.read().ok()?.clone()?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    runtime_handle.spawn(async move {
        let result =
            tokio::time::timeout(Duration::from_secs(30), network.request_block(block_hash))
                .await
                .ok()
                .flatten();
        let _ = tx.send(result);
    });
    rx.recv_timeout(FILTER_TIMEOUT + Duration::from_secs(26))
        .ok()
        .flatten()
}

/// Canonical txid list for merkle/SPV. Module never stores payloads; GetData if needed.
pub fn try_get_canonical_txids(height: u64, block_hash: Hash) -> Option<Vec<Hash>> {
    if pipeline_handles().is_some() {
        let request = CanonicalTxidsRequest { height, block_hash };
        if let Ok(params) = bincode::serialize(&request) {
            if let Some(bytes) = route_bytes(GET_CANONICAL_TXIDS, params) {
                if let Ok(response) = bincode::deserialize::<CanonicalTxidsResponse>(&bytes) {
                    if response.found {
                        let mut out = Vec::with_capacity(response.canonical_txids.len());
                        for hex_id in response.canonical_txids {
                            let raw = hex::decode(hex_id).ok()?;
                            if raw.len() != 32 {
                                return None;
                            }
                            let mut hash = [0u8; 32];
                            hash.copy_from_slice(&raw);
                            out.push(hash);
                        }
                        return Some(out);
                    }
                }
            }
        }
    }
    let (block, _) = request_block_from_peers(block_hash)?;
    Some(
        block
            .transactions
            .iter()
            .map(blvm_protocol::block::calculate_tx_id)
            .collect(),
    )
}

/// Resolve a stripped txid to its block via the module index.
pub fn try_lookup_block_for_txids(txids: &[String]) -> Option<(Hash, u64)> {
    pipeline_handles()?;
    let request = LookupBlockRequest {
        txids: txids.to_vec(),
    };
    let params = bincode::serialize(&request).ok()?;
    let bytes = route_bytes(LOOKUP_BLOCK_FOR_TXIDS, params)?;
    let response: LookupBlockResponse = bincode::deserialize(&bytes).ok()?;
    if response.found {
        Some((response.block_hash?, response.height?))
    } else {
        None
    }
}

/// Download policy: `true` means request `MSG_BLOCK` (skip witness). Fail-open = false (full witness).
pub fn try_filter_block_download_policy(height: u64, block_hash: Hash, merkle_root: Hash) -> bool {
    if pipeline_handles().is_none() {
        return false;
    }
    let request = DownloadPolicyRequest {
        height,
        block_hash,
        merkle_root,
    };
    let Ok(params) = bincode::serialize(&request) else {
        return false;
    };
    let Some(bytes) = route_bytes(FILTER_BLOCK_DOWNLOAD_POLICY, params) else {
        return false;
    };
    bincode::deserialize::<DownloadPolicyResponse>(&bytes)
        .ok()
        .map(|r| r.skip_witness)
        .unwrap_or(false)
}
