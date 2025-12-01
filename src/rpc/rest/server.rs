//! REST API Server
//!
//! Modern REST API server that runs alongside the JSON-RPC server.
//! Uses the existing hyper infrastructure for consistency.

use crate::node::mempool::MempoolManager;
use crate::rpc::{auth, blockchain, mempool, mining, network, rawtx};
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
use super::validation as rest_validation;

/// REST API Server
#[derive(Clone)]
pub struct RestApiServer {
    addr: SocketAddr,
    blockchain: Arc<blockchain::BlockchainRpc>,
    network: Arc<network::NetworkRpc>,
    mempool: Arc<mempool::MempoolRpc>,
    mining: Arc<mining::MiningRpc>,
    rawtx: Arc<rawtx::RawTxRpc>,
    /// Authentication manager (optional)
    auth_manager: Option<Arc<auth::RpcAuthManager>>,
    /// Whether security headers are enabled
    security_headers_enabled: bool,
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
    ) -> Self {
        Self {
            addr,
            blockchain,
            network,
            mempool,
            mining,
            rawtx,
            auth_manager: None,
            security_headers_enabled: true, // Enable security headers by default
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
        auth_manager: Arc<auth::RpcAuthManager>,
    ) -> Self {
        Self {
            addr,
            blockchain,
            network,
            mempool,
            mining,
            rawtx,
            auth_manager: Some(auth_manager),
            security_headers_enabled: true,
            #[cfg(feature = "bip70-http")]
            payment_processor: None,
            #[cfg(feature = "bip70-http")]
            payment_state_machine: None,
        }
    }

    /// Set authentication manager
    pub fn set_auth_manager(mut self, auth_manager: Arc<auth::RpcAuthManager>) -> Self {
        self.auth_manager = Some(auth_manager);
        self
    }

    /// Enable or disable security headers
    pub fn with_security_headers(mut self, enabled: bool) -> Self {
        self.security_headers_enabled = enabled;
        self
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
            "REST API {} {} from {} (request_id: {})",
            method,
            path,
            addr,
            &request_id[..8]
        );

        // Extract headers before consuming request body
        let headers = req.headers().clone();

        // Authenticate request if authentication is enabled
        if let Some(ref auth_manager) = server.auth_manager {
            let auth_result = auth_manager.authenticate_request(&headers, addr).await;

            // Check if authentication failed
            if let Some(error) = &auth_result.error {
                warn!("REST API authentication failed from {}: {}", addr, error);
                return Ok(Self::error_response_with_headers(
                    server.security_headers_enabled,
                    StatusCode::UNAUTHORIZED,
                    "UNAUTHORIZED",
                    error,
                    None,
                    request_id,
                ));
            }

            // Check per-user rate limiting (for authenticated users)
            if let Some(ref user_id) = auth_result.user_id {
                if !auth_manager.check_rate_limit_with_endpoint(user_id, Some(addr), Some(path)).await {
                    warn!("REST API rate limit exceeded for user from {}", addr);
                    return Ok(Self::error_response_with_headers(
                        server.security_headers_enabled,
                        StatusCode::TOO_MANY_REQUESTS,
                        "TOO_MANY_REQUESTS",
                        "Rate limit exceeded",
                        None,
                        request_id,
                    ));
                }
            } else {
                // Unauthenticated request - check per-IP rate limit
                if !auth_manager.check_ip_rate_limit_with_endpoint(addr, Some(path)).await {
                    return Ok(Self::error_response_with_headers(
                        server.security_headers_enabled,
                        StatusCode::TOO_MANY_REQUESTS,
                        "TOO_MANY_REQUESTS",
                        "IP rate limit exceeded",
                        None,
                        request_id,
                    ));
                }
            }

            // Check per-endpoint rate limiting (stricter for write operations)
            let endpoint = Self::get_endpoint_for_rate_limiting(path);
            if !auth_manager.check_method_rate_limit(&endpoint).await {
                warn!("REST API endpoint rate limit exceeded: {} from {}", endpoint, addr);
                return Ok(Self::error_response_with_headers(
                    server.security_headers_enabled,
                    StatusCode::TOO_MANY_REQUESTS,
                    "TOO_MANY_REQUESTS",
                    &format!("Endpoint '{}' rate limit exceeded", endpoint),
                    None,
                    request_id,
                ));
            }
        }

        // Only allow GET and POST methods
        if method != Method::GET && method != Method::POST {
            return Ok(Self::error_response_with_headers(
                server.security_headers_enabled,
                StatusCode::METHOD_NOT_ALLOWED,
                "METHOD_NOT_ALLOWED",
                "Only GET and POST methods are supported",
                None,
                request_id,
            ));
        }

        // Route requests
        let security_headers = server.security_headers_enabled;
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
                    Self::error_response_with_headers(
                        security_headers,
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
                Self::error_response_with_headers(
                    security_headers,
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
                    let body = crate::rpc::rest::types::read_json_body(req).await?;
                    crate::rpc::rest::vault::handle_vault_request(
                        Arc::clone(state_machine),
                        &method,
                        path,
                        body,
                        request_id,
                    )
                    .await
                } else {
                    Self::error_response_with_headers(
                        security_headers,
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
                Self::error_response_with_headers(
                    security_headers,
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
                    let body = crate::rpc::rest::types::read_json_body(req).await?;
                    crate::rpc::rest::pool::handle_pool_request(
                        Arc::clone(state_machine),
                        &method,
                        path,
                        body,
                        request_id,
                    )
                    .await
                } else {
                    Self::error_response_with_headers(
                        security_headers,
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
                Self::error_response_with_headers(
                    security_headers,
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
                    let body = crate::rpc::rest::types::read_json_body(req).await?;
                    crate::rpc::rest::congestion::handle_congestion_request(
                        Arc::clone(state_machine),
                        &method,
                        path,
                        body,
                        request_id,
                    )
                    .await
                } else {
                    Self::error_response_with_headers(
                        security_headers,
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
                Self::error_response_with_headers(
                    security_headers,
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
                        Err(e) => Self::error_response_with_headers(
                            security_headers,
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "PAYMENT_ERROR",
                            &format!("Payment processing error: {}", e),
                            None,
                            request_id,
                        ),
                    }
                } else {
                    Self::error_response_with_headers(
                        security_headers,
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
                Self::error_response_with_headers(
                    security_headers,
                    StatusCode::NOT_IMPLEMENTED,
                    "NOT_IMPLEMENTED",
                    "HTTP BIP70 not enabled. Compile with --features bip70-http",
                    None,
                    request_id,
                )
            }
        } else {
            Self::error_response_with_headers(
                server.security_headers_enabled,
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                &format!("Endpoint not found: {}", path),
                None,
                request_id,
            )
        };

        // Response already has security headers added in individual handlers
        Ok(response)
    }

    /// Get endpoint identifier for rate limiting
    fn get_endpoint_for_rate_limiting(path: &str) -> String {
        // Extract endpoint category for rate limiting
        if path.starts_with("/api/v1/transactions") {
            "rest_transactions".to_string()
        } else if path.starts_with("/api/v1/payments") || path.starts_with("/api/v1/vaults") || path.starts_with("/api/v1/pools") {
            "rest_payments".to_string() // Write operations - stricter limits
        } else if path.starts_with("/api/v1/addresses") {
            "rest_addresses".to_string()
        } else if path.starts_with("/api/v1/blocks") {
            "rest_blocks".to_string()
        } else if path.starts_with("/api/v1/mempool") {
            "rest_mempool".to_string()
        } else {
            "rest_other".to_string()
        }
    }

    /// Add security headers to response
    fn add_security_headers(
        mut response: Response<Full<Bytes>>,
        enabled: bool,
    ) -> Response<Full<Bytes>> {
        if !enabled {
            return response;
        }

        let headers = response.headers_mut();
        
        // Prevent MIME type sniffing
        headers.insert("X-Content-Type-Options", "nosniff".parse().unwrap());
        
        // Prevent clickjacking
        headers.insert("X-Frame-Options", "DENY".parse().unwrap());
        
        // XSS protection (legacy, but still useful)
        headers.insert("X-XSS-Protection", "1; mode=block".parse().unwrap());
        
        // Referrer policy
        headers.insert("Referrer-Policy", "strict-origin-when-cross-origin".parse().unwrap());
        
        // Content Security Policy (restrictive by default)
        headers.insert(
            "Content-Security-Policy",
            "default-src 'self'".parse().unwrap(),
        );

        response
    }

    /// Handle chain-related requests
    async fn handle_chain_request(
        server: Arc<Self>,
        method: Method,
        path: &str,
        request_id: String,
    ) -> Response<Full<Bytes>> {
        let security_headers = server.security_headers_enabled;
        if method != Method::GET {
            return Self::error_response_with_headers(
                security_headers,
                StatusCode::METHOD_NOT_ALLOWED,
                "METHOD_NOT_ALLOWED",
                "Only GET method is supported for chain endpoints",
                None,
                request_id.clone(),
            );
        }

        match path {
            "/api/v1/chain/tip" => match chain::get_chain_tip(&server.blockchain).await {
                Ok(data) => Self::success_response_with_headers(data, request_id, security_headers),
                Err(e) => Self::error_response_with_headers(
                    security_headers,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get chain tip: {}", e),
                    None,
                    request_id,
                ),
            },
            "/api/v1/chain/height" => match chain::get_chain_height(&server.blockchain).await {
                Ok(data) => Self::success_response_with_headers(data, request_id, security_headers),
                Err(e) => Self::error_response_with_headers(
                    security_headers,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get chain height: {}", e),
                    None,
                    request_id,
                ),
            },
            "/api/v1/chain/info" => match chain::get_chain_info(&server.blockchain).await {
                Ok(data) => Self::success_response_with_headers(data, request_id, security_headers),
                Err(e) => Self::error_response_with_headers(
                    security_headers,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get chain info: {}", e),
                    None,
                    request_id,
                ),
            },
            _ => Self::error_response_with_headers(
                security_headers,
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
        let security_headers = server.security_headers_enabled;
        if method != Method::GET {
            return Self::error_response_with_headers(
                security_headers,
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
            return Self::error_response_with_headers(
                security_headers,
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
                            // Validate block height
                            let validated_height = match rest_validation::validate_block_height(height) {
                                Ok(h) => h,
                                Err(e) => {
                                    return Self::error_response_with_headers(
                                        server.security_headers_enabled,
                                        StatusCode::BAD_REQUEST,
                                        "BAD_REQUEST",
                                        &format!("Invalid block height: {}", e),
                                        None,
                                        request_id,
                                    );
                                }
                            };
                            match blocks::get_block_by_height(&server.blockchain, validated_height).await {
                                Ok(data) => Self::success_response_with_headers(data, request_id, server.security_headers_enabled),
                                Err(e) => Self::error_response_with_headers(
                                    server.security_headers_enabled,
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    "INTERNAL_ERROR",
                                    &format!("Failed to get block by height: {}", e),
                                    None,
                                    request_id,
                                ),
                            }
                        }
                        Err(_) => Self::error_response_with_headers(
                            server.security_headers_enabled,
                            StatusCode::BAD_REQUEST,
                            "BAD_REQUEST",
                            "Invalid height parameter (must be a number)",
                            None,
                            request_id,
                        ),
                    }
                } else {
                    Self::error_response_with_headers(
                        server.security_headers_enabled,
                        StatusCode::BAD_REQUEST,
                        "BAD_REQUEST",
                        "Height parameter required",
                        None,
                        request_id,
                    )
                }
            }
            Some(hash) => {
                // Validate hash format
                let validated_hash = match rest_validation::validate_hash_string(hash) {
                    Ok(h) => h,
                    Err(e) => {
                        return Self::error_response_with_headers(
                            server.security_headers_enabled,
                            StatusCode::BAD_REQUEST,
                            "BAD_REQUEST",
                            &format!("Invalid block hash: {}", e),
                            None,
                            request_id,
                        );
                    }
                };
                
                // Check if this is /api/v1/blocks/{hash}/transactions
                if path_parts.get(4) == Some(&"transactions") {
                    match blocks::get_block_transactions(&server.blockchain, &validated_hash).await {
                        Ok(data) => Self::success_response_with_headers(data, request_id, server.security_headers_enabled),
                        Err(e) => Self::error_response_with_headers(
                            server.security_headers_enabled,
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "INTERNAL_ERROR",
                            &format!("Failed to get block transactions: {}", e),
                            None,
                            request_id,
                        ),
                    }
                } else {
                    // /api/v1/blocks/{hash}
                    match blocks::get_block_by_hash(&server.blockchain, &validated_hash).await {
                        Ok(data) => Self::success_response_with_headers(data, request_id, server.security_headers_enabled),
                        Err(e) => Self::error_response_with_headers(
                            server.security_headers_enabled,
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "INTERNAL_ERROR",
                            &format!("Failed to get block: {}", e),
                            None,
                            request_id,
                        ),
                    }
                }
            }
            None => Self::error_response_with_headers(
                server.security_headers_enabled,
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
        let security_headers = server.security_headers_enabled;
        // Parse path: /api/v1/transactions/{txid} or /api/v1/transactions/{txid}/confirmations
        let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        // Expected: ["api", "v1", "transactions", ...]
        if path_parts.len() < 4
            || path_parts[0] != "api"
            || path_parts[1] != "v1"
            || path_parts[2] != "transactions"
        {
            return Self::error_response_with_headers(
                security_headers,
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
                    // Validate transaction ID (hash format)
                    let validated_txid = match rest_validation::validate_hash_string(txid) {
                        Ok(h) => h,
                        Err(e) => {
                            return Self::error_response_with_headers(
                                server.security_headers_enabled,
                                StatusCode::BAD_REQUEST,
                                "BAD_REQUEST",
                                &format!("Invalid transaction ID: {}", e),
                                None,
                                request_id,
                            );
                        }
                    };
                    
                    // Check if this is /api/v1/transactions/{txid}/confirmations
                    if path_parts.get(4) == Some(&"confirmations") {
                        match transactions::get_transaction_confirmations(&server.rawtx, &validated_txid).await
                        {
                            Ok(data) => Self::success_response_with_headers(data, request_id, server.security_headers_enabled),
                            Err(e) => Self::error_response_with_headers(
                                server.security_headers_enabled,
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "INTERNAL_ERROR",
                                &format!("Failed to get transaction confirmations: {}", e),
                                None,
                                request_id,
                            ),
                        }
                    } else {
                        // /api/v1/transactions/{txid}
                        match transactions::get_transaction(&server.rawtx, &validated_txid).await {
                            Ok(data) => Self::success_response_with_headers(data, request_id, server.security_headers_enabled),
                            Err(e) => Self::error_response_with_headers(
                                server.security_headers_enabled,
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "INTERNAL_ERROR",
                                &format!("Failed to get transaction: {}", e),
                                None,
                                request_id,
                            ),
                        }
                    }
                } else {
                    Self::error_response_with_headers(
                        server.security_headers_enabled,
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
                            return Self::error_response_with_headers(
                                security_headers,
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

                    // Validate transaction hex
                    let validated_hex = match rest_validation::validate_transaction_hex(hex) {
                        Ok(h) => h,
                        Err(e) => {
                            return Self::error_response_with_headers(
                                server.security_headers_enabled,
                                StatusCode::BAD_REQUEST,
                                "BAD_REQUEST",
                                &format!("Invalid transaction hex: {}", e),
                                None,
                                request_id,
                            );
                        }
                    };

                    match transactions::submit_transaction(&server.rawtx, &validated_hex).await {
                        Ok(data) => Self::success_response_with_headers(data, request_id, server.security_headers_enabled),
                        Err(e) => Self::error_response_with_headers(
                            server.security_headers_enabled,
                            StatusCode::BAD_REQUEST,
                            "TRANSACTION_REJECTED",
                            &format!("Transaction rejected: {}", e),
                            None,
                            request_id,
                        ),
                    }
                } else {
                    Self::error_response_with_headers(
                        server.security_headers_enabled,
                        StatusCode::BAD_REQUEST,
                        "BAD_REQUEST",
                        "POST /api/v1/transactions expects transaction hex in body",
                        None,
                        request_id,
                    )
                }
            }
            _ => Self::error_response_with_headers(
                server.security_headers_enabled,
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
        let security_headers = server.security_headers_enabled;
        if method != Method::GET {
            return Self::error_response_with_headers(
                security_headers,
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
        
        // Validate address format
        let validated_address = match rest_validation::validate_address_string(address) {
            Ok(a) => a,
            Err(e) => {
                return Self::error_response_with_headers(
                    server.security_headers_enabled,
                    StatusCode::BAD_REQUEST,
                    "BAD_REQUEST",
                    &format!("Invalid address: {}", e),
                    None,
                    request_id,
                );
            }
        };
        
        let action = path_parts.get(4).copied().unwrap_or("");

        match action {
            "balance" => match addresses::get_address_balance(&server.blockchain, &validated_address).await {
                Ok(data) => Self::success_response_with_headers(data, request_id, server.security_headers_enabled),
                Err(e) => Self::error_response_with_headers(
                    server.security_headers_enabled,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get address balance: {}", e),
                    None,
                    request_id,
                ),
            },
            "transactions" => {
                match addresses::get_address_transactions(&server.blockchain, &validated_address).await {
                    Ok(data) => Self::success_response_with_headers(data, request_id, server.security_headers_enabled),
                    Err(e) => Self::error_response_with_headers(
                        server.security_headers_enabled,
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL_ERROR",
                        &format!("Failed to get address transactions: {}", e),
                        None,
                        request_id,
                    ),
                }
            }
            "utxos" => match addresses::get_address_utxos(&server.blockchain, &validated_address).await {
                Ok(data) => Self::success_response_with_headers(data, request_id, server.security_headers_enabled),
                Err(e) => Self::error_response_with_headers(
                    server.security_headers_enabled,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get address UTXOs: {}", e),
                    None,
                    request_id,
                ),
            },
            _ => Self::error_response_with_headers(
                security_headers,
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
        let security_headers = server.security_headers_enabled;
        if method != Method::GET {
            return Self::error_response_with_headers(
                security_headers,
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
            return Self::error_response_with_headers(
                security_headers,
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
                    Ok(data) => Self::success_response_with_headers(data, request_id, security_headers),
                    Err(e) => Self::error_response_with_headers(
                        security_headers,
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
                    // Validate transaction ID
                    let validated_txid = match rest_validation::validate_hash_string(txid) {
                        Ok(h) => h,
                        Err(e) => {
                            return Self::error_response_with_headers(
                                security_headers,
                                StatusCode::BAD_REQUEST,
                                "BAD_REQUEST",
                                &format!("Invalid transaction ID: {}", e),
                                None,
                                request_id,
                            );
                        }
                    };
                    match rest_mempool::get_mempool_transaction(&server.mempool, &validated_txid).await {
                        Ok(data) => Self::success_response_with_headers(data, request_id, security_headers),
                        Err(e) => Self::error_response_with_headers(
                            security_headers,
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "INTERNAL_ERROR",
                            &format!("Failed to get mempool transaction: {}", e),
                            None,
                            request_id,
                        ),
                    }
                } else {
                    Self::error_response_with_headers(
                        security_headers,
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
                    Ok(data) => Self::success_response_with_headers(data, request_id, security_headers),
                    Err(e) => Self::error_response_with_headers(
                        security_headers,
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL_ERROR",
                        &format!("Failed to get mempool stats: {}", e),
                        None,
                        request_id,
                    ),
                }
            }
            _ => Self::error_response_with_headers(
                security_headers,
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
        let security_headers = server.security_headers_enabled;
        if method != Method::GET {
            return Self::error_response_with_headers(
                security_headers,
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
            return Self::error_response_with_headers(
                security_headers,
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "Invalid network endpoint path",
                None,
                request_id.clone(),
            );
        }

        match path_parts.get(3) {
            Some(&"info") => match rest_network::get_network_info(&server.network).await {
                Ok(data) => Self::success_response_with_headers(data, request_id, security_headers),
                Err(e) => Self::error_response_with_headers(
                    security_headers,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get network info: {}", e),
                    None,
                    request_id,
                ),
            },
            Some(&"peers") => match rest_network::get_network_peers(&server.network).await {
                Ok(data) => Self::success_response_with_headers(data, request_id, security_headers),
                Err(e) => Self::error_response_with_headers(
                    security_headers,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get network peers: {}", e),
                    None,
                    request_id,
                ),
            },
            _ => Self::error_response_with_headers(
                security_headers,
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
        let security_headers = server.security_headers_enabled;
        if method != Method::GET {
            return Self::error_response_with_headers(
                security_headers,
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
            return Self::error_response_with_headers(
                security_headers,
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
                    Ok(data) => Self::success_response_with_headers(data, request_id, security_headers),
                    Err(e) => Self::error_response_with_headers(
                        security_headers,
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL_ERROR",
                        &format!("Failed to get fee estimate: {}", e),
                        None,
                        request_id,
                    ),
                }
            }
            _ => Self::error_response_with_headers(
                security_headers,
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
        Self::error_response_with_headers(true, status, code, message, details, request_id)
    }

    /// Create an error response with security headers
    fn error_response_with_headers(
        security_headers_enabled: bool,
        status: StatusCode,
        code: &str,
        message: &str,
        details: Option<serde_json::Value>,
        request_id: String,
    ) -> Response<Full<Bytes>> {
        let error = ApiError::new(code, message, details, None, Some(request_id.clone()));
        let body = serde_json::to_string(&error).unwrap_or_else(|_| "{}".to_string());

        let mut response = Response::builder()
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
            });

        // Add security headers
        if security_headers_enabled {
            response = Self::add_security_headers(response, true);
        }

        response
    }

    /// Create a success response
    fn success_response<T: serde::Serialize>(data: T, request_id: String) -> Response<Full<Bytes>> {
        Self::success_response_with_headers(data, request_id, true)
    }

    /// Create a success response with security headers
    fn success_response_with_headers<T: serde::Serialize>(
        data: T,
        request_id: String,
        security_headers_enabled: bool,
    ) -> Response<Full<Bytes>> {
        let response = ApiResponse::success(
            serde_json::to_value(data).unwrap_or(json!(null)),
            Some(request_id),
        );
        let body = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());

        let mut http_response = Response::builder()
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
            });

        // Add security headers
        if security_headers_enabled {
            http_response = Self::add_security_headers(http_response, true);
        }

        http_response
    }
}
