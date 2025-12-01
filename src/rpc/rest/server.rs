//! REST API Server
//!
//! Modern REST API server that runs alongside the JSON-RPC server.
//! Uses the existing hyper infrastructure for consistency.

use crate::node::mempool::MempoolManager;
use crate::rpc::{auth, blockchain, control, mempool, mining, network, rawtx};
use crate::storage::Storage;
use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::addresses;
use super::blocks;
use super::chain;
use super::fees;
use super::mempool as rest_mempool;
use super::network as rest_network;
use super::transactions;
use super::types::{ApiError, ApiResponse};

/// Maximum request body size (1MB) - matches RPC server
const MAX_REQUEST_SIZE: usize = 1_048_576;

/// REST API Server
#[derive(Clone)]
pub struct RestApiServer {
    addr: SocketAddr,
    blockchain: Arc<blockchain::BlockchainRpc>,
    network: Arc<network::NetworkRpc>,
    mempool: Arc<mempool::MempoolRpc>,
    mining: Arc<mining::MiningRpc>,
    rawtx: Arc<rawtx::RawTxRpc>,
    control: Arc<control::ControlRpc>,
    // Authentication manager (optional, matches RPC server)
    auth_manager: Option<Arc<auth::RpcAuthManager>>,
    #[cfg(feature = "bip70-http")]
    payment_processor: Option<Arc<crate::payment::processor::PaymentProcessor>>,
    #[cfg(feature = "bip70-http")]
    payment_state_machine: Option<Arc<crate::payment::state_machine::PaymentStateMachine>>,
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
        control: Arc<control::ControlRpc>,
    ) -> Self {
        Self {
            addr,
            blockchain,
            network,
            mempool,
            mining,
            rawtx,
            control,
            auth_manager: None,
            #[cfg(feature = "bip70-http")]
            payment_processor: None,
            #[cfg(feature = "bip70-http")]
            payment_state_machine: None,
        }
    }

    /// Create a new REST API server with authentication
    pub fn with_auth(
        addr: SocketAddr,
        blockchain: Arc<blockchain::BlockchainRpc>,
        network: Arc<network::NetworkRpc>,
        mempool: Arc<mempool::MempoolRpc>,
        mining: Arc<mining::MiningRpc>,
        rawtx: Arc<rawtx::RawTxRpc>,
        control: Arc<control::ControlRpc>,
        auth_manager: Arc<auth::RpcAuthManager>,
    ) -> Self {
        Self {
            addr,
            blockchain,
            network,
            mempool,
            mining,
            rawtx,
            control,
            auth_manager: Some(auth_manager),
            #[cfg(feature = "bip70-http")]
            payment_processor: None,
            #[cfg(feature = "bip70-http")]
            payment_state_machine: None,
        }
    }

    /// Set payment processor for BIP70 HTTP endpoints
    #[cfg(feature = "bip70-http")]
    pub fn with_payment_processor(
        mut self,
        processor: Arc<crate::payment::processor::PaymentProcessor>,
    ) -> Self {
        self.payment_processor = Some(processor);
        self
    }

    /// Set payment state machine for CTV payment endpoints
    #[cfg(feature = "bip70-http")]
    pub fn with_payment_state_machine(
        mut self,
        state_machine: Arc<crate::payment::state_machine::PaymentStateMachine>,
    ) -> Self {
        self.payment_state_machine = Some(state_machine);
        self
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
        addr: SocketAddr,
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

        // Extract headers before consuming request body
        let headers = req.headers().clone();

        // Check request body size limit (before parsing)
        if method == Method::POST {
            // For POST requests, we need to check body size
            // We'll do this in read_json_body, but also check Content-Length header if present
            if let Some(content_length) = headers.get("content-length") {
                if let Ok(length_str) = content_length.to_str() {
                    if let Ok(length) = length_str.parse::<usize>() {
                        if length > MAX_REQUEST_SIZE {
                            return Ok(Self::error_response(
                                StatusCode::PAYLOAD_TOO_LARGE,
                                "PAYLOAD_TOO_LARGE",
                                &format!(
                                    "Request body too large: {} bytes (max: {} bytes)",
                                    length, MAX_REQUEST_SIZE
                                ),
                                None,
                                request_id,
                            ));
                        }
                    }
                }
            }
        }

        // Authenticate request if authentication is enabled
        let auth_result = if let Some(ref auth_manager) = server.auth_manager {
            Some(auth_manager.authenticate_request(&headers, addr).await)
        } else {
            None
        };

        // Check if authentication failed
        if let Some(ref auth_result) = auth_result {
            if let Some(error) = &auth_result.error {
                return Ok(Self::error_response(
                    StatusCode::UNAUTHORIZED,
                    "UNAUTHORIZED",
                    error,
                    None,
                    request_id,
                ));
            }
        }

        // Classify endpoint by sensitivity
        let endpoint_type = Self::classify_endpoint(&method, path);

        // Check if endpoint requires authentication
        if let Some(ref auth_result) = auth_result {
            match endpoint_type {
                EndpointType::PublicRead => {
                    // Public read-only endpoints - rate limiting only
                }
                EndpointType::AuthenticatedRead | EndpointType::AuthenticatedWrite | EndpointType::Admin => {
                    // These require authentication
                    if auth_result.user_id.is_none() {
                        return Ok(Self::error_response(
                            StatusCode::UNAUTHORIZED,
                            "UNAUTHORIZED",
                            "Authentication required for this endpoint",
                            None,
                            request_id,
                        ));
                    }
                }
            }
        } else if matches!(endpoint_type, EndpointType::AuthenticatedWrite | EndpointType::Admin) {
            // Auth manager not configured but endpoint requires auth
            return Ok(Self::error_response(
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
                "Authentication required but not configured",
                None,
                request_id,
            ));
        }

        // Check rate limiting (multiple layers)
        if let Some(ref auth_manager) = server.auth_manager {
            if let Some(ref auth_result) = auth_result {
                // Check per-user rate limiting (for authenticated users)
                if let Some(ref user_id) = auth_result.user_id {
                    if !auth_manager.check_rate_limit(user_id).await {
                        return Ok(Self::error_response(
                            StatusCode::TOO_MANY_REQUESTS,
                            "TOO_MANY_REQUESTS",
                            "User rate limit exceeded",
                            None,
                            request_id,
                        ));
                    }
                }
            } else {
                // Unauthenticated request - check per-IP rate limit
                if !auth_manager.check_ip_rate_limit(addr).await {
                    return Ok(Self::error_response(
                        StatusCode::TOO_MANY_REQUESTS,
                        "TOO_MANY_REQUESTS",
                        "IP rate limit exceeded",
                        None,
                        request_id,
                    ));
                }
            }

            // Check per-endpoint rate limiting (stricter for write operations)
            let endpoint_name = Self::get_endpoint_name(path);
            if !auth_manager.check_method_rate_limit(&endpoint_name).await {
                return Ok(Self::error_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    "TOO_MANY_REQUESTS",
                    &format!("Endpoint '{}' rate limit exceeded", endpoint_name),
                    None,
                    request_id,
                ));
            }
        }

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
            Self::handle_chain_request(server, method, path, req, request_id).await
        } else if path.starts_with("/api/v1/blocks") {
            Self::handle_block_request(server, method, path, req, request_id).await
        } else if path.starts_with("/api/v1/transactions") {
            Self::handle_transaction_request(server, method, path, req, request_id).await
        } else if path.starts_with("/api/v1/addresses") {
            Self::handle_address_request(server, method, path, request_id).await
        } else if path.starts_with("/api/v1/mempool") {
            Self::handle_mempool_request(server, method, path, req, request_id).await
        } else if path.starts_with("/api/v1/network") {
            Self::handle_network_request(server, method, path, req, request_id).await
        } else if path.starts_with("/api/v1/mining") {
            Self::handle_mining_request(server, method, path, req, request_id).await
        } else if path.starts_with("/api/v1/node") {
            Self::handle_node_request(server, method, path, req, request_id).await
        } else if path.starts_with("/api/v1/fees") {
            Self::handle_fee_request(server, method, path, request_id).await
        } else if path.starts_with("/api/v1/payments") {
            // CTV payment endpoints (requires bip70-http feature)
            #[cfg(feature = "bip70-http")]
            {
                if let Some(ref state_machine) = server.payment_state_machine {
                    // Parse request body if present
                    let body = if method == Method::POST || method == Method::PUT {
                        let (_, body_stream) = req.into_parts();
                        let body_bytes = body_stream.collect().await?.to_bytes();
                        if body_bytes.is_empty() {
                            None
                        } else {
                            match serde_json::from_slice::<Value>(&body_bytes) {
                                Ok(v) => Some(v),
                                Err(_) => None,
                            }
                        }
                    } else {
                        None
                    };

                    // Handle payment REST endpoints
                    crate::rpc::rest::payment::handle_payment_request(
                        Arc::clone(state_machine),
                        &method,
                        path,
                        body,
                    )
                    .await
                } else {
                    Self::error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "SERVICE_UNAVAILABLE",
                        "Payment state machine not configured",
                        None,
                        request_id,
                    )
                }
            }
            #[cfg(not(feature = "bip70-http"))]
            {
                Self::error_response(
                    StatusCode::NOT_IMPLEMENTED,
                    "NOT_IMPLEMENTED",
                    "Payment endpoints require --features bip70-http",
                    None,
                    request_id,
                )
            }
        } else if path.starts_with("/api/v1/vaults") {
            // Vault endpoints (requires ctv feature)
            #[cfg(feature = "ctv")]
            {
                if let Some(ref state_machine) = server.payment_state_machine {
                    let body = match crate::rpc::rest::types::read_json_body(req).await {
                        Ok(body) => body,
                        Err(e) => {
                            return Ok(Self::error_response(
                                StatusCode::PAYLOAD_TOO_LARGE,
                                "PAYLOAD_TOO_LARGE",
                                &e,
                                None,
                                request_id,
                            ));
                        }
                    };
                    crate::rpc::rest::vault::handle_vault_request(
                        Arc::clone(state_machine),
                        &method,
                        path,
                        body,
                        request_id,
                    )
                    .await
                } else {
                    Self::error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "SERVICE_UNAVAILABLE",
                        "Vault engine not configured",
                        None,
                        request_id,
                    )
                }
            }
            #[cfg(not(feature = "ctv"))]
            {
                Self::error_response(
                    StatusCode::NOT_IMPLEMENTED,
                    "NOT_IMPLEMENTED",
                    "CTV feature not enabled for Vaults",
                    None,
                    request_id,
                )
            }
        } else if path.starts_with("/api/v1/pools") {
            // Pool endpoints (requires ctv feature)
            #[cfg(feature = "ctv")]
            {
                if let Some(ref state_machine) = server.payment_state_machine {
                    let body = match crate::rpc::rest::types::read_json_body(req).await {
                        Ok(body) => body,
                        Err(e) => {
                            return Ok(Self::error_response(
                                StatusCode::PAYLOAD_TOO_LARGE,
                                "PAYLOAD_TOO_LARGE",
                                &e,
                                None,
                                request_id,
                            ));
                        }
                    };
                    crate::rpc::rest::pool::handle_pool_request(
                        Arc::clone(state_machine),
                        &method,
                        path,
                        body,
                        request_id,
                    )
                    .await
                } else {
                    Self::error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "SERVICE_UNAVAILABLE",
                        "Pool engine not configured",
                        None,
                        request_id,
                    )
                }
            }
            #[cfg(not(feature = "ctv"))]
            {
                Self::error_response(
                    StatusCode::NOT_IMPLEMENTED,
                    "NOT_IMPLEMENTED",
                    "CTV feature not enabled for Pools",
                    None,
                    request_id,
                )
            }
        } else if path.starts_with("/api/v1/batches") || path.starts_with("/api/v1/congestion") {
            // Congestion control endpoints (requires ctv feature)
            #[cfg(feature = "ctv")]
            {
                if let Some(ref state_machine) = server.payment_state_machine {
                    let body = match crate::rpc::rest::types::read_json_body(req).await {
                        Ok(body) => body,
                        Err(e) => {
                            return Ok(Self::error_response(
                                StatusCode::PAYLOAD_TOO_LARGE,
                                "PAYLOAD_TOO_LARGE",
                                &e,
                                None,
                                request_id,
                            ));
                        }
                    };
                    crate::rpc::rest::congestion::handle_congestion_request(
                        Arc::clone(state_machine),
                        &method,
                        path,
                        body,
                        request_id,
                    )
                    .await
                } else {
                    Self::error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "SERVICE_UNAVAILABLE",
                        "Congestion manager not configured",
                        None,
                        request_id,
                    )
                }
            }
            #[cfg(not(feature = "ctv"))]
            {
                Self::error_response(
                    StatusCode::NOT_IMPLEMENTED,
                    "NOT_IMPLEMENTED",
                    "CTV feature not enabled for Congestion Control",
                    None,
                    request_id,
                )
            }
        } else if path.starts_with("/api/v1/payment") {
            // Legacy BIP70 payment endpoints (requires bip70-http feature)
            #[cfg(feature = "bip70-http")]
            {
                if let Some(ref processor) = server.payment_processor {
                    match crate::payment::http::handle_payment_routes(Arc::clone(processor), req)
                        .await
                    {
                        Ok(resp) => resp,
                        Err(e) => Self::error_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "PAYMENT_ERROR",
                            &format!("Payment processing error: {}", e),
                            None,
                            request_id,
                        ),
                    }
                } else {
                    Self::error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "SERVICE_UNAVAILABLE",
                        "Payment processor not configured",
                        None,
                        request_id,
                    )
                }
            }
            #[cfg(not(feature = "bip70-http"))]
            {
                Self::error_response(
                    StatusCode::NOT_IMPLEMENTED,
                    "NOT_IMPLEMENTED",
                    "HTTP BIP70 not enabled. Compile with --features bip70-http",
                    None,
                    request_id,
                )
            }
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
        req: Request<Incoming>,
        request_id: String,
    ) -> Response<Full<Bytes>> {
        match (method, path) {
            (Method::GET, "/api/v1/chain/tip") => match chain::get_chain_tip(&server.blockchain).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get chain tip: {}", e),
                    None,
                    request_id,
                ),
            },
            (Method::GET, "/api/v1/chain/height") => match chain::get_chain_height(&server.blockchain).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get chain height: {}", e),
                    None,
                    request_id,
                ),
            },
            (Method::GET, "/api/v1/chain/info") => match chain::get_chain_info(&server.blockchain).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get chain info: {}", e),
                    None,
                    request_id,
                ),
            },
            (Method::GET, "/api/v1/chain/difficulty") => match chain::get_chain_difficulty(&server.blockchain).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get chain difficulty: {}", e),
                    None,
                    request_id,
                ),
            },
            (Method::GET, "/api/v1/chain/utxo-set") => match chain::get_utxo_set_info(&server.blockchain).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get UTXO set info: {}", e),
                    None,
                    request_id,
                ),
            },
            (Method::GET, "/api/v1/chain/tips") => match chain::get_chain_tips(&server.blockchain).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get chain tips: {}", e),
                    None,
                    request_id,
                ),
            },
            (Method::GET, path) if path.starts_with("/api/v1/chain/tx-stats") => {
                // Parse query parameter nblocks
                let nblocks = req.uri().query()
                    .and_then(|q| {
                        q.split('&')
                            .find(|p| p.starts_with("nblocks="))
                            .and_then(|p| p.split('=').nth(1))
                            .and_then(|v| v.parse::<u64>().ok())
                    });
                match chain::get_chain_tx_stats(&server.blockchain, nblocks).await {
                    Ok(data) => Self::success_response(data, request_id),
                    Err(e) => Self::error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL_ERROR",
                        &format!("Failed to get chain tx stats: {}", e),
                        None,
                        request_id,
                    ),
                }
            },
            (Method::GET, "/api/v1/chain/prune-info") => match chain::get_prune_info(&server.blockchain).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get prune info: {}", e),
                    None,
                    request_id,
                ),
            },
            (Method::GET, path) if path.starts_with("/api/v1/indexes") => {
                // Parse index name from query or path
                let index_name = req.uri().path()
                    .strip_prefix("/api/v1/indexes/")
                    .filter(|s| !s.is_empty());
                match chain::get_index_info(&server.blockchain, index_name).await {
                    Ok(data) => Self::success_response(data, request_id),
                    Err(e) => Self::error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL_ERROR",
                        &format!("Failed to get index info: {}", e),
                        None,
                        request_id,
                    ),
                }
            },
            (Method::POST, "/api/v1/chain/verify") => {
                let body = match crate::rpc::rest::types::read_json_body(req).await {
                    Ok(Some(body)) => body,
                    Ok(None) => json!({}),
                    Err(_) => json!({}),
                };
                let checklevel = body.get("checklevel").and_then(|v| v.as_u64());
                let numblocks = body.get("numblocks").and_then(|v| v.as_u64());
                match chain::verify_chain(&server.blockchain, checklevel, numblocks).await {
                    Ok(data) => Self::success_response(data, request_id),
                    Err(e) => Self::error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL_ERROR",
                        &format!("Failed to verify chain: {}", e),
                        None,
                        request_id,
                    ),
                }
            },
            (Method::POST, "/api/v1/chain/prune") => {
                let body = match crate::rpc::rest::types::read_json_body(req).await {
                    Ok(Some(body)) => body,
                    Ok(None) => json!({}),
                    Err(_) => json!({}),
                };
                let height = body.get("height")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| "Missing 'height' field in request body");
                match height {
                    Ok(h) => {
                        match chain::prune_blockchain(&server.blockchain, h).await {
                            Ok(data) => Self::success_response(data, request_id),
                            Err(e) => Self::error_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "INTERNAL_ERROR",
                                &format!("Failed to prune blockchain: {}", e),
                                None,
                                request_id,
                            ),
                        }
                    }
                    Err(e) => Self::error_response(
                        StatusCode::BAD_REQUEST,
                        "BAD_REQUEST",
                        e,
                        None,
                        request_id,
                    ),
                }
            },
            _ => Self::error_response(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                &format!("Chain endpoint not found: {} {}", method, path),
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
        req: Request<Incoming>,
        request_id: String,
    ) -> Response<Full<Bytes>> {
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

        match (method, path_parts.get(3)) {
            (Method::GET, Some(&"height")) => {
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
            (Method::GET, Some(hash)) => {
                // Check sub-path
                match path_parts.get(4) {
                    Some(&"transactions") => {
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
                    }
                    Some(&"header") => {
                        // Parse verbose from query (default: true)
                        let verbose = req.uri().query()
                            .and_then(|q| {
                                q.split('&')
                                    .find(|p| p.starts_with("verbose="))
                                    .and_then(|p| p.split('=').nth(1))
                                    .and_then(|v| v.parse::<bool>().ok())
                            })
                            .unwrap_or(true);
                        match blocks::get_block_header(&server.blockchain, hash, verbose).await {
                            Ok(data) => Self::success_response(data, request_id),
                            Err(e) => Self::error_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "INTERNAL_ERROR",
                                &format!("Failed to get block header: {}", e),
                                None,
                                request_id,
                            ),
                        }
                    }
                    Some(&"stats") => {
                        match blocks::get_block_stats(&server.blockchain, hash).await {
                            Ok(data) => Self::success_response(data, request_id),
                            Err(e) => Self::error_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "INTERNAL_ERROR",
                                &format!("Failed to get block stats: {}", e),
                                None,
                                request_id,
                            ),
                        }
                    }
                    Some(&"filter") => {
                        // Parse filtertype from query
                        let filtertype = req.uri().query()
                            .and_then(|q| {
                                q.split('&')
                                    .find(|p| p.starts_with("filtertype="))
                                    .and_then(|p| p.split('=').nth(1))
                            });
                        match blocks::get_block_filter(&server.blockchain, hash, filtertype).await {
                            Ok(data) => Self::success_response(data, request_id),
                            Err(e) => Self::error_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "INTERNAL_ERROR",
                                &format!("Failed to get block filter: {}", e),
                                None,
                                request_id,
                            ),
                        }
                    }
                    _ => {
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
            }
            (Method::POST, Some(hash)) => {
                match path_parts.get(4) {
                    Some(&"invalidate") => {
                        match blocks::invalidate_block(&server.blockchain, hash).await {
                            Ok(data) => Self::success_response(data, request_id),
                            Err(e) => Self::error_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "INTERNAL_ERROR",
                                &format!("Failed to invalidate block: {}", e),
                                None,
                                request_id,
                            ),
                        }
                    }
                    Some(&"reconsider") => {
                        match blocks::reconsider_block(&server.blockchain, hash).await {
                            Ok(data) => Self::success_response(data, request_id),
                            Err(e) => Self::error_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "INTERNAL_ERROR",
                                &format!("Failed to reconsider block: {}", e),
                                None,
                                request_id,
                            ),
                        }
                    }
                    _ => Self::error_response(
                        StatusCode::NOT_FOUND,
                        "NOT_FOUND",
                        &format!("Block endpoint not found: {} {}", method, path),
                        None,
                        request_id,
                    ),
                }
            }
            _ => Self::error_response(
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
                    } else if path_parts.get(4) == Some(&"outputs") {
                        // /api/v1/transactions/{txid}/outputs/{n}
                        if let Some(n_str) = path_parts.get(5) {
                            match n_str.parse::<u32>() {
                                Ok(n) => {
                                    // TODO: Parse include_mempool from query params
                                    match transactions::get_transaction_output(&server.rawtx, txid, n, true).await {
                                        Ok(data) => Self::success_response(data, request_id),
                                        Err(e) => Self::error_response(
                                            StatusCode::INTERNAL_SERVER_ERROR,
                                            "INTERNAL_ERROR",
                                            &format!("Failed to get transaction output: {}", e),
                                            None,
                                            request_id,
                                        ),
                                    }
                                }
                                Err(_) => Self::error_response(
                                    StatusCode::BAD_REQUEST,
                                    "BAD_REQUEST",
                                    "Invalid output index",
                                    None,
                                    request_id,
                                ),
                            }
                        } else {
                            Self::error_response(
                                StatusCode::BAD_REQUEST,
                                "BAD_REQUEST",
                                "Output index required",
                                None,
                                request_id,
                            )
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
                // Parse request body
                let body = match crate::rpc::rest::types::read_json_body(req).await {
                    Ok(Some(body)) => body,
                    Ok(None) => json!({}),
                    Err(e) => {
                        return Self::error_response(
                            StatusCode::BAD_REQUEST,
                            "BAD_REQUEST",
                            &format!("Invalid JSON body: {}", e),
                            None,
                            request_id,
                        );
                    }
                };

                // Check path for specific endpoints
                if path_parts.get(3) == Some(&"test") {
                    // POST /api/v1/transactions/test
                    let hex = body.get("hex")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| "Missing 'hex' field in request body");
                    match hex {
                        Ok(hex_str) => {
                            match transactions::test_transaction(&server.rawtx, hex_str).await {
                                Ok(data) => Self::success_response(data, request_id),
                                Err(e) => Self::error_response(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    "INTERNAL_ERROR",
                                    &format!("Failed to test transaction: {}", e),
                                    None,
                                    request_id,
                                ),
                            }
                        }
                        Err(e) => Self::error_response(
                            StatusCode::BAD_REQUEST,
                            "BAD_REQUEST",
                            e,
                            None,
                            request_id,
                        ),
                    }
                } else if path_parts.get(3) == Some(&"decode") {
                    // POST /api/v1/transactions/decode
                    let hex = body.get("hex")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| "Missing 'hex' field in request body");
                    match hex {
                        Ok(hex_str) => {
                            match transactions::decode_transaction(&server.rawtx, hex_str).await {
                                Ok(data) => Self::success_response(data, request_id),
                                Err(e) => Self::error_response(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    "INTERNAL_ERROR",
                                    &format!("Failed to decode transaction: {}", e),
                                    None,
                                    request_id,
                                ),
                            }
                        }
                        Err(e) => Self::error_response(
                            StatusCode::BAD_REQUEST,
                            "BAD_REQUEST",
                            e,
                            None,
                            request_id,
                        ),
                    }
                } else if path_parts.get(3) == Some(&"create") {
                    // POST /api/v1/transactions/create
                    let inputs = body.get("inputs")
                        .ok_or_else(|| "Missing 'inputs' field in request body");
                    let outputs = body.get("outputs")
                        .ok_or_else(|| "Missing 'outputs' field in request body");
                    match (inputs, outputs) {
                        (Ok(inputs_val), Ok(outputs_val)) => {
                            let locktime = body.get("locktime").and_then(|v| v.as_u64());
                            let replaceable = body.get("replaceable").and_then(|v| v.as_bool());
                            let version = body.get("version").and_then(|v| v.as_u64()).map(|v| v as u32);
                            match transactions::create_transaction(
                                &server.rawtx,
                                inputs_val.clone(),
                                outputs_val.clone(),
                                locktime,
                                replaceable,
                                version,
                            ).await {
                                Ok(data) => Self::success_response(data, request_id),
                                Err(e) => Self::error_response(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    "INTERNAL_ERROR",
                                    &format!("Failed to create transaction: {}", e),
                                    None,
                                    request_id,
                                ),
                            }
                        }
                        (Err(e), _) | (_, Err(e)) => Self::error_response(
                            StatusCode::BAD_REQUEST,
                            "BAD_REQUEST",
                            e,
                            None,
                            request_id,
                        ),
                    }
                } else {
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
        req: Request<Incoming>,
        request_id: String,
    ) -> Response<Full<Bytes>> {
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

        match (method, path_parts.get(3)) {
            (Method::GET, None) => {
                // /api/v1/mempool - list all transactions
                let verbose = req.uri().query()
                    .and_then(|q| {
                        q.split('&')
                            .find(|p| p.starts_with("verbose="))
                            .and_then(|p| p.split('=').nth(1))
                            .and_then(|v| v.parse::<bool>().ok())
                    })
                    .unwrap_or(false);
                match rest_mempool::get_mempool(&server.mempool, verbose).await {
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
            (Method::GET, Some(&"stats")) => {
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
            (Method::GET, Some(&"transactions")) => {
                if let Some(txid) = path_parts.get(4) {
                    match path_parts.get(5) {
                        Some(&"ancestors") => {
                            let verbose = req.uri().query()
                                .and_then(|q| {
                                    q.split('&')
                                        .find(|p| p.starts_with("verbose="))
                                        .and_then(|p| p.split('=').nth(1))
                                        .and_then(|v| v.parse::<bool>().ok())
                                })
                                .unwrap_or(false);
                            match rest_mempool::get_mempool_ancestors(&server.mempool, txid, verbose).await {
                                Ok(data) => Self::success_response(data, request_id),
                                Err(e) => Self::error_response(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    "INTERNAL_ERROR",
                                    &format!("Failed to get mempool ancestors: {}", e),
                                    None,
                                    request_id,
                                ),
                            }
                        }
                        Some(&"descendants") => {
                            let verbose = req.uri().query()
                                .and_then(|q| {
                                    q.split('&')
                                        .find(|p| p.starts_with("verbose="))
                                        .and_then(|p| p.split('=').nth(1))
                                        .and_then(|v| v.parse::<bool>().ok())
                                })
                                .unwrap_or(false);
                            match rest_mempool::get_mempool_descendants(&server.mempool, txid, verbose).await {
                                Ok(data) => Self::success_response(data, request_id),
                                Err(e) => Self::error_response(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    "INTERNAL_ERROR",
                                    &format!("Failed to get mempool descendants: {}", e),
                                    None,
                                    request_id,
                                ),
                            }
                        }
                        _ => {
                            // /api/v1/mempool/transactions/{txid}
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
            (Method::POST, Some(&"save")) => {
                // POST /api/v1/mempool/save
                match rest_mempool::save_mempool(&server.mempool).await {
                    Ok(data) => Self::success_response(data, request_id),
                    Err(e) => Self::error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL_ERROR",
                        &format!("Failed to save mempool: {}", e),
                        None,
                        request_id,
                    ),
                }
            }
            (Method::POST, Some(&"transactions")) => {
                if let Some(txid) = path_parts.get(4) {
                    if path_parts.get(5) == Some(&"priority") {
                        // POST /api/v1/mempool/transactions/{txid}/priority
                        let body = match crate::rpc::rest::types::read_json_body(req).await {
                            Ok(Some(body)) => body,
                            Ok(None) => json!({}),
                            Err(_) => json!({}),
                        };
                        let fee_delta = body.get("fee_delta")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0);
                        match rest_mempool::prioritize_transaction(&server.mempool, txid, fee_delta).await {
                            Ok(data) => Self::success_response(data, request_id),
                            Err(e) => Self::error_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "INTERNAL_ERROR",
                                &format!("Failed to prioritize transaction: {}", e),
                                None,
                                request_id,
                            ),
                        }
                    } else {
                        Self::error_response(
                            StatusCode::NOT_FOUND,
                            "NOT_FOUND",
                            &format!("Mempool endpoint not found: {}", path),
                            None,
                            request_id,
                        )
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
            _ => Self::error_response(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                &format!("Mempool endpoint not found: {} {}", method, path),
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
        req: Request<Incoming>,
        request_id: String,
    ) -> Response<Full<Bytes>> {
        // Parse path: /api/v1/network/...
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

        match (method, path_parts.get(3)) {
            (Method::GET, Some(&"info")) => match rest_network::get_network_info(&server.network).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get network info: {}", e),
                    None,
                    request_id,
                ),
            },
            (Method::GET, Some(&"peers")) => match rest_network::get_network_peers(&server.network).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get network peers: {}", e),
                    None,
                    request_id,
                ),
            },
            (Method::GET, Some(&"connections")) => {
                if path_parts.get(4) == Some(&"count") {
                    match rest_network::get_connection_count(&server.network).await {
                        Ok(data) => Self::success_response(data, request_id),
                        Err(e) => Self::error_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "INTERNAL_ERROR",
                            &format!("Failed to get connection count: {}", e),
                            None,
                            request_id,
                        ),
                    }
                } else {
                    Self::error_response(
                        StatusCode::NOT_FOUND,
                        "NOT_FOUND",
                        &format!("Network endpoint not found: {}", path),
                        None,
                        request_id,
                    )
                }
            }
            (Method::GET, Some(&"totals")) => match rest_network::get_net_totals(&server.network).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get network totals: {}", e),
                    None,
                    request_id,
                ),
            },
            (Method::GET, Some(&"nodes")) => {
                if path_parts.get(4) == Some(&"addresses") {
                    let count = req.uri().query()
                        .and_then(|q| {
                            q.split('&')
                                .find(|p| p.starts_with("count="))
                                .and_then(|p| p.split('=').nth(1))
                                .and_then(|v| v.parse::<u32>().ok())
                        });
                    match rest_network::get_node_addresses(&server.network, count).await {
                        Ok(data) => Self::success_response(data, request_id),
                        Err(e) => Self::error_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "INTERNAL_ERROR",
                            &format!("Failed to get node addresses: {}", e),
                            None,
                            request_id,
                        ),
                    }
                } else {
                    // GET /api/v1/network/nodes?node={node}&dns={dns}
                    let node = req.uri().query()
                        .and_then(|q| {
                            q.split('&')
                                .find(|p| p.starts_with("node="))
                                .and_then(|p| p.split('=').nth(1))
                        });
                    let dns = req.uri().query()
                        .and_then(|q| {
                            q.split('&')
                                .find(|p| p.starts_with("dns="))
                                .and_then(|p| p.split('=').nth(1))
                                .and_then(|v| v.parse::<bool>().ok())
                        })
                        .unwrap_or(false);
                    match rest_network::get_added_node_info(&server.network, node, dns).await {
                        Ok(data) => Self::success_response(data, request_id),
                        Err(e) => Self::error_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "INTERNAL_ERROR",
                            &format!("Failed to get added node info: {}", e),
                            None,
                            request_id,
                        ),
                    }
                }
            }
            (Method::GET, Some(&"bans")) => match rest_network::list_banned(&server.network).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to list banned nodes: {}", e),
                    None,
                    request_id,
                ),
            },
            (Method::POST, Some(&"ping")) => match rest_network::ping_network(&server.network).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to ping network: {}", e),
                    None,
                    request_id,
                ),
            },
            (Method::POST, Some(&"nodes")) => {
                let body = match crate::rpc::rest::types::read_json_body(req).await {
                    Ok(Some(body)) => body,
                    Ok(None) => json!({}),
                    Err(e) => {
                        return Self::error_response(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "PAYLOAD_TOO_LARGE",
                            &e,
                            None,
                            request_id,
                        );
                    }
                };
                let address = body.get("address")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "Missing 'address' field");
                let command = body.get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("add");
                match address {
                    Ok(addr) => {
                        match rest_network::add_node(&server.network, addr, command).await {
                            Ok(data) => Self::success_response(data, request_id),
                            Err(e) => Self::error_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "INTERNAL_ERROR",
                                &format!("Failed to add node: {}", e),
                                None,
                                request_id,
                            ),
                        }
                    }
                    Err(e) => Self::error_response(
                        StatusCode::BAD_REQUEST,
                        "BAD_REQUEST",
                        e,
                        None,
                        request_id,
                    ),
                }
            }
            (Method::POST, Some(&"active")) => {
                let body = match crate::rpc::rest::types::read_json_body(req).await {
                    Ok(Some(body)) => body,
                    Ok(None) => json!({}),
                    Err(_) => json!({}),
                };
                let state = body.get("state")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                match rest_network::set_network_active(&server.network, state).await {
                    Ok(data) => Self::success_response(data, request_id),
                    Err(e) => Self::error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL_ERROR",
                        &format!("Failed to set network active: {}", e),
                        None,
                        request_id,
                    ),
                }
            }
            (Method::POST, Some(&"bans")) => {
                let body = match crate::rpc::rest::types::read_json_body(req).await {
                    Ok(Some(body)) => body,
                    Ok(None) => json!({}),
                    Err(_) => json!({}),
                };
                let subnet = body.get("subnet")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "Missing 'subnet' field");
                let command = body.get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("add");
                let bantime = body.get("bantime").and_then(|v| v.as_u64());
                let absolute = body.get("absolute").and_then(|v| v.as_bool());
                match subnet {
                    Ok(sub) => {
                        match rest_network::set_ban(&server.network, sub, command, bantime, absolute).await {
                            Ok(data) => Self::success_response(data, request_id),
                            Err(e) => Self::error_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "INTERNAL_ERROR",
                                &format!("Failed to set ban: {}", e),
                                None,
                                request_id,
                            ),
                        }
                    }
                    Err(e) => Self::error_response(
                        StatusCode::BAD_REQUEST,
                        "BAD_REQUEST",
                        e,
                        None,
                        request_id,
                    ),
                }
            }
            (Method::DELETE, Some(&"nodes")) => {
                if let Some(address) = path_parts.get(4) {
                    match rest_network::disconnect_node(&server.network, address).await {
                        Ok(data) => Self::success_response(data, request_id),
                        Err(e) => Self::error_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "INTERNAL_ERROR",
                            &format!("Failed to disconnect node: {}", e),
                            None,
                            request_id,
                        ),
                    }
                } else {
                    Self::error_response(
                        StatusCode::BAD_REQUEST,
                        "BAD_REQUEST",
                        "Node address required",
                        None,
                        request_id,
                    )
                }
            }
            (Method::DELETE, Some(&"bans")) => match rest_network::clear_banned(&server.network).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to clear banned nodes: {}", e),
                    None,
                    request_id,
                ),
            },
            _ => Self::error_response(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                &format!("Network endpoint not found: {} {}", method, path),
                None,
                request_id,
            ),
        }
    }

    /// Handle mining-related requests
    async fn handle_mining_request(
        server: Arc<Self>,
        method: Method,
        path: &str,
        req: Request<Incoming>,
        request_id: String,
    ) -> Response<Full<Bytes>> {
        // Parse path: /api/v1/mining/...
        let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        if path_parts.len() < 4
            || path_parts[0] != "api"
            || path_parts[1] != "v1"
            || path_parts[2] != "mining"
        {
            return Self::error_response(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "Invalid mining endpoint path",
                None,
                request_id.clone(),
            );
        }

        match (method, path_parts.get(3)) {
            (Method::GET, Some(&"info")) => match mining::get_mining_info(&server.mining).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get mining info: {}", e),
                    None,
                    request_id,
                ),
            },
            (Method::GET, Some(&"block-template")) => {
                // Parse query parameters for rules and capabilities (simplified - just pass empty for now)
                match mining::get_block_template(&server.mining, None, None).await {
                    Ok(data) => Self::success_response(data, request_id),
                    Err(e) => Self::error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL_ERROR",
                        &format!("Failed to get block template: {}", e),
                        None,
                        request_id,
                    ),
                }
            }
            (Method::POST, Some(&"blocks")) => {
                // POST /api/v1/mining/blocks (submit block)
                let body = match crate::rpc::rest::types::read_json_body(req).await {
                    Ok(Some(body)) => body,
                    Ok(None) => json!({}),
                    Err(_) => json!({}),
                };
                let hex = body.get("hex")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "Missing 'hex' field in request body");
                match hex {
                    Ok(hex_str) => {
                        match mining::submit_block(&server.mining, hex_str).await {
                            Ok(data) => Self::success_response(data, request_id),
                            Err(e) => Self::error_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "INTERNAL_ERROR",
                                &format!("Failed to submit block: {}", e),
                                None,
                                request_id,
                            ),
                        }
                    }
                    Err(e) => Self::error_response(
                        StatusCode::BAD_REQUEST,
                        "BAD_REQUEST",
                        e,
                        None,
                        request_id,
                    ),
                }
            }
            _ => Self::error_response(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                &format!("Mining endpoint not found: {} {}", method, path),
                None,
                request_id,
            ),
        }
    }

    /// Handle node control requests
    async fn handle_node_request(
        server: Arc<Self>,
        method: Method,
        path: &str,
        req: Request<Incoming>,
        request_id: String,
    ) -> Response<Full<Bytes>> {
        // Parse path: /api/v1/node/...
        let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        if path_parts.len() < 4
            || path_parts[0] != "api"
            || path_parts[1] != "v1"
            || path_parts[2] != "node"
        {
            return Self::error_response(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "Invalid node endpoint path",
                None,
                request_id.clone(),
            );
        }

        match (method, path_parts.get(3)) {
            (Method::GET, Some(&"uptime")) => match node::get_uptime(&server.control).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get uptime: {}", e),
                    None,
                    request_id,
                ),
            },
            (Method::GET, Some(&"memory")) => {
                let mode = req.uri().query()
                    .and_then(|q| {
                        q.split('&')
                            .find(|p| p.starts_with("mode="))
                            .and_then(|p| p.split('=').nth(1))
                    });
                match node::get_memory_info(&server.control, mode).await {
                    Ok(data) => Self::success_response(data, request_id),
                    Err(e) => Self::error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL_ERROR",
                        &format!("Failed to get memory info: {}", e),
                        None,
                        request_id,
                    ),
                }
            }
            (Method::GET, Some(&"rpc-info")) => match node::get_rpc_info(&server.control).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get RPC info: {}", e),
                    None,
                    request_id,
                ),
            },
            (Method::GET, Some(&"help")) => {
                let command = req.uri().query()
                    .and_then(|q| {
                        q.split('&')
                            .find(|p| p.starts_with("command="))
                            .and_then(|p| p.split('=').nth(1))
                    });
                match node::get_help(&server.control, command).await {
                    Ok(data) => Self::success_response(data, request_id),
                    Err(e) => Self::error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL_ERROR",
                        &format!("Failed to get help: {}", e),
                        None,
                        request_id,
                    ),
                }
            }
            (Method::GET, Some(&"logging")) => match node::get_logging(&server.control).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get logging: {}", e),
                    None,
                    request_id,
                ),
            },
            (Method::POST, Some(&"stop")) => match node::stop_node(&server.control).await {
                Ok(data) => Self::success_response(data, request_id),
                Err(e) => Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to stop node: {}", e),
                    None,
                    request_id,
                ),
            },
            (Method::POST, Some(&"logging")) => {
                let body = match crate::rpc::rest::types::read_json_body(req).await {
                    Ok(Some(body)) => body,
                    Ok(None) => json!({}),
                    Err(e) => {
                        return Self::error_response(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "PAYLOAD_TOO_LARGE",
                            &e,
                            None,
                            request_id,
                        );
                    }
                };
                let include = body.get("include")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect());
                let exclude = body.get("exclude")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect());
                match node::set_logging(&server.control, include, exclude).await {
                    Ok(data) => Self::success_response(data, request_id),
                    Err(e) => Self::error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL_ERROR",
                        &format!("Failed to set logging: {}", e),
                        None,
                        request_id,
                    ),
                }
            }
            _ => Self::error_response(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                &format!("Node endpoint not found: {} {}", method, path),
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
    /// Endpoint classification for security
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum EndpointType {
        /// Public read-only endpoints (no auth required)
        PublicRead,
        /// Authenticated read endpoints (auth required)
        AuthenticatedRead,
        /// Authenticated write endpoints (auth required)
        AuthenticatedWrite,
        /// Admin-only endpoints (auth required, stricter limits)
        Admin,
    }

    /// Classify endpoint by sensitivity
    fn classify_endpoint(method: &Method, path: &str) -> EndpointType {
        // Admin endpoints (most sensitive)
        if path.starts_with("/api/v1/node/stop")
            || path.starts_with("/api/v1/node/logging")
            || path.starts_with("/api/v1/blocks/invalidate")
            || path.starts_with("/api/v1/blocks/reconsider")
        {
            return EndpointType::Admin;
        }

        // Write operations (require auth)
        if *method == Method::POST {
            if path.starts_with("/api/v1/transactions/submit")
                || path.starts_with("/api/v1/transactions/create")
                || path.starts_with("/api/v1/blocks/submit")
                || path.starts_with("/api/v1/mining/submit")
                || path.starts_with("/api/v1/network/")
                || path.starts_with("/api/v1/mempool/prioritize")
                || path.starts_with("/api/v1/mempool/save")
            {
                return EndpointType::AuthenticatedWrite;
            }
        }

        // Authenticated read endpoints (sensitive data)
        if path.starts_with("/api/v1/mempool/stats")
            || path.starts_with("/api/v1/mempool/ancestors")
            || path.starts_with("/api/v1/mempool/descendants")
            || path.starts_with("/api/v1/node/")
            || path.starts_with("/api/v1/network/peers")
            || path.starts_with("/api/v1/network/connections")
        {
            return EndpointType::AuthenticatedRead;
        }

        // Default: public read-only
        EndpointType::PublicRead
    }

    /// Get endpoint name for rate limiting
    fn get_endpoint_name(path: &str) -> String {
        // Extract endpoint name from path for rate limiting
        // e.g., "/api/v1/transactions/submit" -> "transactions.submit"
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.len() >= 3 && parts[0] == "api" && parts[1] == "v1" {
            parts[2..].join(".")
        } else {
            "unknown".to_string()
        }
    }

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
