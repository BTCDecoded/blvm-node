//! REST API Server
//!
//! Modern REST API server that runs alongside the JSON-RPC server.
//! Uses the existing hyper infrastructure for consistency.

use crate::node::mempool::MempoolManager;
use crate::rpc::{blockchain, mempool, mining, network, rawtx};
use crate::storage::Storage;
use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{debug, error, info};
use uuid::Uuid;

use super::addresses;
use super::blocks;
use super::chain;
use super::fees;
use super::mempool as rest_mempool;
use super::network as rest_network;
use super::transactions;
use super::types::{ApiError, ApiResponse};

/// REST API Server
#[derive(Clone)]
pub struct RestApiServer {
    addr: SocketAddr,
    blockchain: Arc<blockchain::BlockchainRpc>,
    network: Arc<network::NetworkRpc>,
    mempool: Arc<mempool::MempoolRpc>,
    mining: Arc<mining::MiningRpc>,
    rawtx: Arc<rawtx::RawTxRpc>,
}

impl RestApiServer {
    /// Create a new REST API server
    pub fn new(
        addr: SocketAddr,
        blockchain: Arc<blockchain::BlockchainRpc>,
        network: Arc<network::NetworkRpc>,
        mempool: Arc<mempool::MempoolRpc>,
        mining: Arc<mining::MiningRpc>,
        rawtx: Arc<rawtx::RawTxRpc>,
    ) -> Self {
        Self {
            addr,
            blockchain,
            network,
            mempool,
            mining,
            rawtx,
        }
    }

    /// Start the REST API server
    pub async fn start(&self) -> Result<()> {
        let listener = TcpListener::bind(self.addr).await?;
        info!("REST API server listening on {}", self.addr);

        let server = Arc::new(self.clone());

        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    debug!("New REST API connection from {}", addr);
                    let server_clone = Arc::clone(&server);
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let service = service_fn(move |req| {
                            Self::handle_request(server_clone.clone(), req, addr)
                        });
                        if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                            debug!("REST API connection error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    error!("Failed to accept REST API connection: {}", e);
                }
            }
        }
    }

    /// Handle HTTP request
    async fn handle_request(
        server: Arc<Self>,
        req: Request<Incoming>,
        _addr: SocketAddr,
    ) -> Result<Response<Full<Bytes>>, hyper::Error> {
        let method = req.method().clone();
        let uri = req.uri().clone();
        let path = uri.path();

        // Generate request ID for tracing
        let request_id = Uuid::new_v4().to_string();

        debug!(
            "REST API {} {} (request_id: {})",
            method,
            path,
            &request_id[..8]
        );

        // Only allow GET and POST methods
        if method != Method::GET && method != Method::POST {
            return Ok(Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "METHOD_NOT_ALLOWED",
                "Only GET and POST methods are supported",
                None,
                request_id,
            ));
        }

        // Route requests
        let response = if path.starts_with("/api/v1/chain") {
            Self::handle_chain_request(server, method, path, request_id).await
        } else if path.starts_with("/api/v1/blocks") {
            Self::handle_block_request(server, method, path, request_id).await
        } else if path.starts_with("/api/v1/transactions") {
            Self::handle_transaction_request(server, method, path, req, request_id).await
        } else if path.starts_with("/api/v1/addresses") {
            Self::handle_address_request(server, method, path, request_id).await
        } else if path.starts_with("/api/v1/mempool") {
            Self::handle_mempool_request(server, method, path, request_id).await
        } else if path.starts_with("/api/v1/network") {
            Self::handle_network_request(server, method, path, request_id).await
        } else if path.starts_with("/api/v1/fees") {
            Self::handle_fee_request(server, method, path, request_id).await
        } else {
            Self::error_response(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                &format!("Endpoint not found: {}", path),
                None,
                request_id,
            )
        };

        Ok(response)
    }

    /// Handle chain-related requests
    async fn handle_chain_request(
        server: Arc<Self>,
        method: Method,
        path: &str,
        request_id: String,
    ) -> Response<Full<Bytes>> {
        if method != Method::GET {
            return Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "METHOD_NOT_ALLOWED",
                "Only GET method is supported for chain endpoints",
                None,
                request_id.clone(),
            );
        }

        match path {
            "/api/v1/chain/tip" => match chain::get_chain_tip(&server.blockchain).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get chain tip: {}", e),
                    None,
                    request_id,
                ),
            },
            "/api/v1/chain/height" => match chain::get_chain_height(&server.blockchain).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get chain height: {}", e),
                    None,
                    request_id,
                ),
            },
            "/api/v1/chain/info" => match chain::get_chain_info(&server.blockchain).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get chain info: {}", e),
                    None,
                    request_id,
                ),
            },
            _ => Self::error_response(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                &format!("Chain endpoint not found: {}", path),
                None,
                request_id.clone(),
            ),
        }
    }

    /// Handle block-related requests
    async fn handle_block_request(
        server: Arc<Self>,
        method: Method,
        path: &str,
        request_id: String,
    ) -> Response<Full<Bytes>> {
        if method != Method::GET {
            return Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "METHOD_NOT_ALLOWED",
                "Only GET method is supported for block endpoints",
                None,
                request_id.clone(),
            );
        }

        // Parse path: /api/v1/blocks/{hash} or /api/v1/blocks/{hash}/transactions or /api/v1/blocks/height/{height}
        let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        // Expected: ["api", "v1", "blocks", ...]
        if path_parts.len() < 4
            || path_parts[0] != "api"
            || path_parts[1] != "v1"
            || path_parts[2] != "blocks"
        {
            return Self::error_response(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "Invalid block endpoint path",
                None,
                request_id.clone(),
            );
        }

        match path_parts.get(3) {
            Some(&"height") => {
                // /api/v1/blocks/height/{height}
                if let Some(height_str) = path_parts.get(4) {
                    match height_str.parse::<u64>() {
                        Ok(height) => {
                            match blocks::get_block_by_height(&server.blockchain, height).await {
                                Ok(data) => Self::success_response(data, request_id),
                                Err(e) => Self::error_response(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    "INTERNAL_ERROR",
                                    &format!("Failed to get block by height: {}", e),
                                    None,
                                    request_id,
                                ),
                            }
                        }
                        Err(_) => Self::error_response(
                            StatusCode::BAD_REQUEST,
                            "BAD_REQUEST",
                            "Invalid height parameter",
                            None,
                            request_id,
                        ),
                    }
                } else {
                    Self::error_response(
                        StatusCode::BAD_REQUEST,
                        "BAD_REQUEST",
                        "Height parameter required",
                        None,
                        request_id,
                    )
                }
            }
            Some(hash) => {
                // Check if this is /api/v1/blocks/{hash}/transactions
                if path_parts.get(4) == Some(&"transactions") {
                    match blocks::get_block_transactions(&server.blockchain, hash).await {
                        Ok(data) => Self::success_response(data, request_id),
                        Err(e) => Self::error_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "INTERNAL_ERROR",
                            &format!("Failed to get block transactions: {}", e),
                            None,
                            request_id,
                        ),
                    }
                } else {
                    // /api/v1/blocks/{hash}
                    match blocks::get_block_by_hash(&server.blockchain, hash).await {
                        Ok(data) => Self::success_response(data, request_id),
                        Err(e) => Self::error_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "INTERNAL_ERROR",
                            &format!("Failed to get block: {}", e),
                            None,
                            request_id,
                        ),
                    }
                }
            }
            None => Self::error_response(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "Block hash or height required",
                None,
                request_id,
            ),
        }
    }

    /// Handle transaction-related requests
    async fn handle_transaction_request(
        server: Arc<Self>,
        method: Method,
        path: &str,
        req: Request<Incoming>,
        request_id: String,
    ) -> Response<Full<Bytes>> {
        // Parse path: /api/v1/transactions/{txid} or /api/v1/transactions/{txid}/confirmations
        let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        // Expected: ["api", "v1", "transactions", ...]
        if path_parts.len() < 4
            || path_parts[0] != "api"
            || path_parts[1] != "v1"
            || path_parts[2] != "transactions"
        {
            return Self::error_response(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "Invalid transaction endpoint path",
                None,
                request_id.clone(),
            );
        }

        match method {
            Method::GET => {
                if let Some(txid) = path_parts.get(3) {
                    // Check if this is /api/v1/transactions/{txid}/confirmations
                    if path_parts.get(4) == Some(&"confirmations") {
                        match transactions::get_transaction_confirmations(&server.rawtx, txid).await
                        {
                            Ok(data) => Self::success_response(data, request_id),
                            Err(e) => Self::error_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "INTERNAL_ERROR",
                                &format!("Failed to get transaction confirmations: {}", e),
                                None,
                                request_id,
                            ),
                        }
                    } else {
                        // /api/v1/transactions/{txid}
                        match transactions::get_transaction(&server.rawtx, txid).await {
                            Ok(data) => Self::success_response(data, request_id),
                            Err(e) => Self::error_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "INTERNAL_ERROR",
                                &format!("Failed to get transaction: {}", e),
                                None,
                                request_id,
                            ),
                        }
                    }
                } else {
                    Self::error_response(
                        StatusCode::BAD_REQUEST,
                        "BAD_REQUEST",
                        "Transaction ID required",
                        None,
                        request_id,
                    )
                }
            }
            Method::POST => {
                // POST /api/v1/transactions (submit transaction)
                if path_parts.len() == 3 {
                    // Read request body
                    let body = match req.collect().await {
                        Ok(b) => b.to_bytes(),
                        Err(e) => {
                            return Self::error_response(
                                StatusCode::BAD_REQUEST,
                                "BAD_REQUEST",
                                &format!("Failed to read request body: {}", e),
                                None,
                                request_id,
                            );
                        }
                    };

                    let hex = match std::str::from_utf8(&body) {
                        Ok(s) => s.trim().trim_matches('"'), // Remove quotes if JSON string
                        Err(_) => {
                            // Try as raw hex
                            std::str::from_utf8(&body).unwrap_or("")
                        }
                    };

                    if hex.is_empty() {
                        return Self::error_response(
                            StatusCode::BAD_REQUEST,
                            "BAD_REQUEST",
                            "Transaction hex required in request body",
                            None,
                            request_id,
                        );
                    }

                    match transactions::submit_transaction(&server.rawtx, hex).await {
                        Ok(data) => Self::success_response(data, request_id),
                        Err(e) => Self::error_response(
                            StatusCode::BAD_REQUEST,
                            "TRANSACTION_REJECTED",
                            &format!("Transaction rejected: {}", e),
                            None,
                            request_id,
                        ),
                    }
                } else {
                    Self::error_response(
                        StatusCode::BAD_REQUEST,
                        "BAD_REQUEST",
                        "POST /api/v1/transactions expects transaction hex in body",
                        None,
                        request_id,
                    )
                }
            }
            _ => Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "METHOD_NOT_ALLOWED",
                "Only GET and POST methods are supported for transaction endpoints",
                None,
                request_id,
            ),
        }
    }

    /// Handle address-related requests
    async fn handle_address_request(
        server: Arc<Self>,
        method: Method,
        path: &str,
        request_id: String,
    ) -> Response<Full<Bytes>> {
        if method != Method::GET {
            return Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "METHOD_NOT_ALLOWED",
                "Only GET method is supported for address endpoints",
                None,
                request_id.clone(),
            );
        }

        // Parse path: /api/v1/addresses/{address}/balance|transactions|utxos
        let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        // Expected: ["api", "v1", "addresses", {address}, {action}]
        if path_parts.len() < 5
            || path_parts[0] != "api"
            || path_parts[1] != "v1"
            || path_parts[2] != "addresses"
        {
            return Self::error_response(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "Invalid address endpoint path",
                None,
                request_id.clone(),
            );
        }

        let address = path_parts[3];
        let action = path_parts.get(4).copied().unwrap_or("");

        match action {
            "balance" => match addresses::get_address_balance(&server.blockchain, address).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get address balance: {}", e),
                    None,
                    request_id,
                ),
            },
            "transactions" => {
                match addresses::get_address_transactions(&server.blockchain, address).await {
                    Ok(data) => Self::success_response(data, request_id),
                    Err(e) => Self::error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL_ERROR",
                        &format!("Failed to get address transactions: {}", e),
                        None,
                        request_id,
                    ),
                }
            }
            "utxos" => match addresses::get_address_utxos(&server.blockchain, address).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get address UTXOs: {}", e),
                    None,
                    request_id,
                ),
            },
            _ => Self::error_response(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                &format!(
                    "Address action not found: {}. Supported: balance, transactions, utxos",
                    action
                ),
                None,
                request_id,
            ),
        }
    }

    /// Handle mempool-related requests
    async fn handle_mempool_request(
        server: Arc<Self>,
        method: Method,
        path: &str,
        request_id: String,
    ) -> Response<Full<Bytes>> {
        if method != Method::GET {
            return Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "METHOD_NOT_ALLOWED",
                "Only GET method is supported for mempool endpoints",
                None,
                request_id.clone(),
            );
        }

        // Parse path: /api/v1/mempool or /api/v1/mempool/transactions/{txid} or /api/v1/mempool/stats
        let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        if path_parts.len() < 3
            || path_parts[0] != "api"
            || path_parts[1] != "v1"
            || path_parts[2] != "mempool"
        {
            return Self::error_response(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "Invalid mempool endpoint path",
                None,
                request_id.clone(),
            );
        }

        match path_parts.get(3) {
            None => {
                // /api/v1/mempool - list all transactions
                match rest_mempool::get_mempool(&server.mempool).await {
                    Ok(data) => Self::success_response(data, request_id),
                    Err(e) => Self::error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL_ERROR",
                        &format!("Failed to get mempool: {}", e),
                        None,
                        request_id,
                    ),
                }
            }
            Some(&"transactions") => {
                // /api/v1/mempool/transactions/{txid}
                if let Some(txid) = path_parts.get(4) {
                    match rest_mempool::get_mempool_transaction(&server.mempool, txid).await {
                        Ok(data) => Self::success_response(data, request_id),
                        Err(e) => Self::error_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "INTERNAL_ERROR",
                            &format!("Failed to get mempool transaction: {}", e),
                            None,
                            request_id,
                        ),
                    }
                } else {
                    Self::error_response(
                        StatusCode::BAD_REQUEST,
                        "BAD_REQUEST",
                        "Transaction ID required",
                        None,
                        request_id,
                    )
                }
            }
            Some(&"stats") => {
                // /api/v1/mempool/stats
                match rest_mempool::get_mempool_stats(&server.mempool).await {
                    Ok(data) => Self::success_response(data, request_id),
                    Err(e) => Self::error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL_ERROR",
                        &format!("Failed to get mempool stats: {}", e),
                        None,
                        request_id,
                    ),
                }
            }
            _ => Self::error_response(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                &format!("Mempool endpoint not found: {}", path),
                None,
                request_id,
            ),
        }
    }

    /// Handle network-related requests
    async fn handle_network_request(
        server: Arc<Self>,
        method: Method,
        path: &str,
        request_id: String,
    ) -> Response<Full<Bytes>> {
        if method != Method::GET {
            return Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "METHOD_NOT_ALLOWED",
                "Only GET method is supported for network endpoints",
                None,
                request_id.clone(),
            );
        }

        // Parse path: /api/v1/network/info or /api/v1/network/peers
        let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        if path_parts.len() < 4
            || path_parts[0] != "api"
            || path_parts[1] != "v1"
            || path_parts[2] != "network"
        {
            return Self::error_response(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "Invalid network endpoint path",
                None,
                request_id.clone(),
            );
        }

        match path_parts.get(3) {
            Some(&"info") => match rest_network::get_network_info(&server.network).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get network info: {}", e),
                    None,
                    request_id,
                ),
            },
            Some(&"peers") => match rest_network::get_network_peers(&server.network).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get network peers: {}", e),
                    None,
                    request_id,
                ),
            },
            _ => Self::error_response(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                &format!(
                    "Network endpoint not found: {}. Supported: info, peers",
                    path
                ),
                None,
                request_id,
            ),
        }
    }

    /// Handle fee-related requests
    async fn handle_fee_request(
        server: Arc<Self>,
        method: Method,
        path: &str,
        request_id: String,
    ) -> Response<Full<Bytes>> {
        if method != Method::GET {
            return Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "METHOD_NOT_ALLOWED",
                "Only GET method is supported for fee endpoints",
                None,
                request_id.clone(),
            );
        }

        // Parse path: /api/v1/fees/estimate?blocks=N (optional query param)
        let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        if path_parts.len() < 4
            || path_parts[0] != "api"
            || path_parts[1] != "v1"
            || path_parts[2] != "fees"
        {
            return Self::error_response(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "Invalid fee endpoint path",
                None,
                request_id.clone(),
            );
        }

        match path_parts.get(3) {
            Some(&"estimate") => {
                // TODO: Parse query parameters for blocks (for now use default)
                match fees::get_fee_estimate(&server.mining, None).await {
                    Ok(data) => Self::success_response(data, request_id),
                    Err(e) => Self::error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL_ERROR",
                        &format!("Failed to get fee estimate: {}", e),
                        None,
                        request_id,
                    ),
                }
            }
            _ => Self::error_response(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                &format!("Fee endpoint not found: {}. Supported: estimate", path),
                None,
                request_id,
            ),
        }
    }

    /// Create an error response
    fn error_response(
        status: StatusCode,
        code: &str,
        message: &str,
        details: Option<serde_json::Value>,
        request_id: String,
    ) -> Response<Full<Bytes>> {
        let error = ApiError::new(code, message, details, None, Some(request_id.clone()));
        let body = serde_json::to_string(&error).unwrap_or_else(|_| "{}".to_string());

        Response::builder()
            .status(status)
            .header("Content-Type", "application/json")
            .header("Content-Length", body.len())
            .body(Full::new(Bytes::from(body)))
            .unwrap_or_else(|_| {
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Full::new(Bytes::from(
                        "{\"error\":\"Internal server error\"}",
                    )))
                    .expect("Fallback response should always succeed")
            })
    }

    /// Create a success response
    fn success_response<T: serde::Serialize>(data: T, request_id: String) -> Response<Full<Bytes>> {
        let response = ApiResponse::success(
            serde_json::to_value(data).unwrap_or(json!(null)),
            Some(request_id),
        );
        let body = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());

        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .header("Content-Length", body.len())
            .body(Full::new(Bytes::from(body)))
            .unwrap_or_else(|_| {
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Full::new(Bytes::from(
                        "{\"error\":\"Internal server error\"}",
                    )))
                    .expect("Fallback response should always succeed")
            })
    }
}
