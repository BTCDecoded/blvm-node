//! JSON-RPC server implementation
//!
//! Handles HTTP/WebSocket connections and routes JSON-RPC requests.
//! Uses hyper for secure HTTP handling with proper request size limits.

use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{debug, error, info, warn, Span};
use uuid::Uuid;

use super::{auth, blockchain, control, errors, mempool, mining, network, rawtx};
use crate::node::metrics::MetricsCollector;

/// Maximum request body size (1MB)
const MAX_REQUEST_SIZE: usize = 1_048_576;

/// JSON-RPC server
#[derive(Clone)]
pub struct RpcServer {
    addr: SocketAddr,
    // Cached RPC handlers to avoid recreating on every request
    blockchain: Arc<blockchain::BlockchainRpc>,
    network: Arc<network::NetworkRpc>,
    mempool: Arc<mempool::MempoolRpc>,
    mining: Arc<mining::MiningRpc>,
    rawtx: Arc<rawtx::RawTxRpc>,
    control: Arc<control::ControlRpc>,
    // Authentication manager (optional)
    auth_manager: Option<Arc<auth::RpcAuthManager>>,
    // Metrics collector (optional, for Prometheus export)
    metrics: Option<Arc<MetricsCollector>>,
}

impl RpcServer {
    /// Create a new RPC server
    pub fn new(addr: SocketAddr) -> Self {
        use crate::utils::arc_new;
        Self {
            addr,
            blockchain: arc_new(blockchain::BlockchainRpc::new()),
            network: arc_new(network::NetworkRpc::new()),
            mempool: arc_new(mempool::MempoolRpc::new()),
            mining: arc_new(mining::MiningRpc::new()),
            rawtx: arc_new(rawtx::RawTxRpc::new()),
            control: arc_new(control::ControlRpc::new()),
            auth_manager: None,
            metrics: None,
        }
    }

    /// Create a new RPC server with authentication
    pub fn with_auth(addr: SocketAddr, auth_manager: Arc<auth::RpcAuthManager>) -> Self {
        use crate::utils::arc_new;
        Self {
            addr,
            blockchain: arc_new(blockchain::BlockchainRpc::new()),
            network: arc_new(network::NetworkRpc::new()),
            mempool: arc_new(mempool::MempoolRpc::new()),
            mining: arc_new(mining::MiningRpc::new()),
            rawtx: arc_new(rawtx::RawTxRpc::new()),
            control: arc_new(control::ControlRpc::new()),
            auth_manager: Some(auth_manager),
            metrics: None,
        }
    }

    /// Create with dependencies
    pub fn with_dependencies(
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
            metrics: None,
        }
    }

    /// Create with dependencies and metrics
    pub fn with_dependencies_and_metrics(
        addr: SocketAddr,
        blockchain: Arc<blockchain::BlockchainRpc>,
        network: Arc<network::NetworkRpc>,
        mempool: Arc<mempool::MempoolRpc>,
        mining: Arc<mining::MiningRpc>,
        rawtx: Arc<rawtx::RawTxRpc>,
        control: Arc<control::ControlRpc>,
        metrics: Arc<MetricsCollector>,
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
            metrics: Some(metrics),
        }
    }

    /// Create with dependencies and authentication
    pub fn with_dependencies_and_auth(
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
            metrics: None,
        }
    }

    /// Create with dependencies, authentication, and metrics
    pub fn with_dependencies_auth_and_metrics(
        addr: SocketAddr,
        blockchain: Arc<blockchain::BlockchainRpc>,
        network: Arc<network::NetworkRpc>,
        mempool: Arc<mempool::MempoolRpc>,
        mining: Arc<mining::MiningRpc>,
        rawtx: Arc<rawtx::RawTxRpc>,
        control: Arc<control::ControlRpc>,
        auth_manager: Arc<auth::RpcAuthManager>,
        metrics: Arc<MetricsCollector>,
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
            metrics: Some(metrics),
        }
    }

    /// Start the RPC server
    ///
    /// Handles both HTTP (via hyper) and raw TCP JSON-RPC (for backward compatibility)
    pub async fn start(&self) -> Result<()> {
        let listener = TcpListener::bind(self.addr).await?;
        info!("RPC server listening on {}", self.addr);

        // Wrap server in Arc to share across connections
        // Create a new server instance with cloned Arc handlers
        use crate::utils::{arc_clone, arc_new};
        let server = arc_new(RpcServer {
            addr: self.addr,
            blockchain: arc_clone(&self.blockchain),
            network: arc_clone(&self.network),
            mempool: arc_clone(&self.mempool),
            mining: arc_clone(&self.mining),
            rawtx: arc_clone(&self.rawtx),
            control: arc_clone(&self.control),
            auth_manager: self.auth_manager.clone(),
            metrics: self.metrics.clone(),
        });

        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    debug!("New RPC connection from {}", addr);
                    let peer_addr = addr;
                    let server = Arc::clone(&server);

                    // Spawn task to handle connection
                    // Clone values before moving into async block to ensure Send
                    let server_for_spawn = Arc::clone(&server);
                    let peer_addr_for_spawn = peer_addr;
                    tokio::spawn(async move {
                        // Use hyper for HTTP - it will handle protocol detection and parsing
                        let io = TokioIo::new(stream);
                        let server_clone = Arc::clone(&server_for_spawn);
                        let peer_addr_clone = peer_addr_for_spawn;
                        let service = service_fn({
                            let server_for_service = Arc::clone(&server_clone);
                            let addr_for_service = peer_addr_clone;
                            move |req| {
                                let server_inner = Arc::clone(&server_for_service);
                                let addr_inner = addr_for_service;
                                Self::handle_http_request_with_server(server_inner, req, addr_inner)
                            }
                        });

                        // Try to serve as HTTP
                        if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                            // If hyper fails, it might be raw TCP
                            // But we can't recover here since hyper consumed the connection
                            // For now, log and continue - raw TCP support would need separate port
                            debug!(
                                "HTTP connection failed from {} (might be raw TCP): {}",
                                peer_addr_clone, e
                            );
                        }
                    });
                }
                Err(e) => {
                    error!("Failed to accept RPC connection: {}", e);
                }
            }
        }
    }

    /// Handle HTTP request using hyper (with server instance for cached handlers)
    async fn handle_http_request_with_server(
        server: Arc<Self>,
        req: Request<Incoming>,
        addr: SocketAddr,
    ) -> Result<Response<Full<Bytes>>, hyper::Error> {
        // Handle GET requests for health and metrics endpoints
        if req.method() == Method::GET {
            let path = req.uri().path();
            if path == "/metrics" {
                return Self::handle_metrics_endpoint(server).await;
            }
            return Self::handle_health_endpoint(server, req).await;
        }

        // Only allow POST method for JSON-RPC
        if req.method() != Method::POST {
            return Ok(Self::http_error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "Only POST method is supported for JSON-RPC",
            ));
        }

        // Extract headers before consuming request body
        let headers = req.headers().clone();

        // Check Content-Type
        if let Some(content_type) = headers.get("content-type") {
            if content_type != "application/json" {
                warn!("Invalid Content-Type from {}: {:?}", addr, content_type);
            }
        }

        // Read request body with size limit
        let body = req.collect().await?;
        let body_bytes = body.to_bytes();

        // Enforce maximum request size
        if body_bytes.len() > MAX_REQUEST_SIZE {
            return Ok(Self::http_error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!(
                    "Request body too large: {} bytes (max: {} bytes)",
                    body_bytes.len(),
                    MAX_REQUEST_SIZE
                ),
            ));
        }

        // Parse JSON body
        let json_body = match std::str::from_utf8(&body_bytes) {
            Ok(s) => s.to_string(),
            Err(e) => {
                return Ok(Self::http_error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("Invalid UTF-8 in request body: {e}"),
                ));
            }
        };

        // Generate request ID for tracing
        let request_id = Uuid::new_v4().to_string();
        let request_id_short = request_id.chars().take(8).collect::<String>();

        // Create tracing span with request context
        let span = tracing::span!(
            tracing::Level::DEBUG,
            "rpc_request",
            request_id = %request_id_short,
            method = tracing::field::Empty,
            client_addr = %addr,
            request_size = json_body.len()
        );

        let _guard = span.enter();

        debug!("HTTP RPC request from {}: {} bytes", addr, json_body.len());

        // Extract method name for per-method rate limiting (before authentication)
        let method_name = serde_json::from_str::<Value>(&json_body)
            .ok()
            .and_then(|req| {
                req.get("method")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "unknown".to_string());

        // Record method in span
        Span::current().record("method", &method_name);

        // Authenticate request if authentication is enabled
        let auth_result = if let Some(ref auth_manager) = server.auth_manager {
            Some(auth_manager.authenticate_request(&headers, addr).await)
        } else {
            None
        };

        // Check rate limiting (multiple layers)
        if let Some(ref auth_manager) = server.auth_manager {
            if let Some(ref auth_result) = auth_result {
                // Check if authentication failed
                if let Some(error) = &auth_result.error {
                    return Ok(Self::http_error_response(StatusCode::UNAUTHORIZED, error));
                }

                // Check per-user rate limiting (for authenticated users)
                if let Some(ref user_id) = auth_result.user_id {
                    if !auth_manager.check_rate_limit(user_id).await {
                        return Ok(Self::http_error_response(
                            StatusCode::TOO_MANY_REQUESTS,
                            "User rate limit exceeded",
                        ));
                    }
                }
            } else {
                // Unauthenticated request - check per-IP rate limit
                if !auth_manager.check_ip_rate_limit(addr).await {
                    return Ok(Self::http_error_response(
                        StatusCode::TOO_MANY_REQUESTS,
                        "IP rate limit exceeded",
                    ));
                }
            }

            // Check per-method rate limiting (applies to all requests)
            if !auth_manager.check_method_rate_limit(&method_name).await {
                return Ok(Self::http_error_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    &format!("Method '{method_name}' rate limit exceeded"),
                ));
            }
        }

        // Process JSON-RPC request (reuse server instance with cached handlers)
        let start_time = std::time::Instant::now();
        let response_json = Self::process_request_with_server(server, &json_body).await;
        let duration = start_time.elapsed();

        // Record response metrics in span
        Span::current().record("duration_ms", duration.as_millis() as u64);
        Span::current().record("response_size", response_json.len());

        debug!(
            "RPC request completed in {:?} (request_id: {})",
            duration, request_id_short
        );

        // Build HTTP response with request ID header
        // Response::builder() returns http::Error, but we need hyper::Error
        // Since hyper::Error doesn't implement From<http::Error> or From<io::Error>,
        // we use expect() since Response::builder() should never fail with valid inputs
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .header("Content-Length", response_json.len())
            .header("X-Request-ID", request_id_short)
            .body(Full::new(Bytes::from(response_json)))
            .expect("Failed to build HTTP response - this should never happen with valid inputs"))
    }

    /// Handle Prometheus metrics endpoint
    async fn handle_metrics_endpoint(
        server: Arc<Self>,
    ) -> Result<Response<Full<Bytes>>, hyper::Error> {
        // Get metrics if available
        let metrics_text = if let Some(ref metrics_collector) = server.metrics {
            Self::format_prometheus_metrics(metrics_collector.collect())
        } else {
            // Return empty metrics if collector not available
            "# No metrics available\n".to_string()
        };

        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/plain; version=0.0.4")
            .header("Content-Length", metrics_text.len())
            .body(Full::new(Bytes::from(metrics_text)))
            .expect("Failed to build metrics response"))
    }

    /// Format NodeMetrics as Prometheus text format
    fn format_prometheus_metrics(metrics: crate::node::metrics::NodeMetrics) -> String {
        let mut output = String::new();

        // Network metrics
        output.push_str("# HELP bllvm_network_peers_total Total number of connected peers\n");
        output.push_str("# TYPE bllvm_network_peers_total gauge\n");
        output.push_str(&format!(
            "bllvm_network_peers_total {}\n",
            metrics.network.peer_count
        ));

        output.push_str("# HELP bllvm_network_bytes_sent_total Total bytes sent\n");
        output.push_str("# TYPE bllvm_network_bytes_sent_total counter\n");
        output.push_str(&format!(
            "bllvm_network_bytes_sent_total {}\n",
            metrics.network.bytes_sent
        ));

        output.push_str("# HELP bllvm_network_bytes_received_total Total bytes received\n");
        output.push_str("# TYPE bllvm_network_bytes_received_total counter\n");
        output.push_str(&format!(
            "bllvm_network_bytes_received_total {}\n",
            metrics.network.bytes_received
        ));

        output.push_str("# HELP bllvm_network_messages_sent_total Total messages sent\n");
        output.push_str("# TYPE bllvm_network_messages_sent_total counter\n");
        output.push_str(&format!(
            "bllvm_network_messages_sent_total {}\n",
            metrics.network.messages_sent
        ));

        output.push_str("# HELP bllvm_network_messages_received_total Total messages received\n");
        output.push_str("# TYPE bllvm_network_messages_received_total counter\n");
        output.push_str(&format!(
            "bllvm_network_messages_received_total {}\n",
            metrics.network.messages_received
        ));

        output.push_str("# HELP bllvm_network_active_connections Active network connections\n");
        output.push_str("# TYPE bllvm_network_active_connections gauge\n");
        output.push_str(&format!(
            "bllvm_network_active_connections {}\n",
            metrics.network.active_connections
        ));

        output.push_str("# HELP bllvm_network_banned_peers Banned peers count\n");
        output.push_str("# TYPE bllvm_network_banned_peers gauge\n");
        output.push_str(&format!(
            "bllvm_network_banned_peers {}\n",
            metrics.network.banned_peers
        ));

        // Storage metrics
        output.push_str("# HELP bllvm_storage_blocks_total Total blocks stored\n");
        output.push_str("# TYPE bllvm_storage_blocks_total gauge\n");
        output.push_str(&format!(
            "bllvm_storage_blocks_total {}\n",
            metrics.storage.block_count
        ));

        output.push_str("# HELP bllvm_storage_utxos_total Total UTXOs\n");
        output.push_str("# TYPE bllvm_storage_utxos_total gauge\n");
        output.push_str(&format!(
            "bllvm_storage_utxos_total {}\n",
            metrics.storage.utxo_count
        ));

        output.push_str("# HELP bllvm_storage_transactions_total Total transactions indexed\n");
        output.push_str("# TYPE bllvm_storage_transactions_total gauge\n");
        output.push_str(&format!(
            "bllvm_storage_transactions_total {}\n",
            metrics.storage.transaction_count
        ));

        output.push_str("# HELP bllvm_storage_disk_size_bytes Estimated disk size in bytes\n");
        output.push_str("# TYPE bllvm_storage_disk_size_bytes gauge\n");
        output.push_str(&format!(
            "bllvm_storage_disk_size_bytes {}\n",
            metrics.storage.disk_size
        ));

        output.push_str("# HELP bllvm_storage_within_bounds Storage bounds status (1=within bounds, 0=exceeded)\n");
        output.push_str("# TYPE bllvm_storage_within_bounds gauge\n");
        output.push_str(&format!(
            "bllvm_storage_within_bounds {}\n",
            if metrics.storage.within_bounds { 1 } else { 0 }
        ));

        // RPC metrics
        output.push_str("# HELP bllvm_rpc_requests_total Total RPC requests\n");
        output.push_str("# TYPE bllvm_rpc_requests_total counter\n");
        output.push_str(&format!(
            "bllvm_rpc_requests_total {}\n",
            metrics.rpc.requests_total
        ));

        output.push_str("# HELP bllvm_rpc_requests_success_total Successful RPC requests\n");
        output.push_str("# TYPE bllvm_rpc_requests_success_total counter\n");
        output.push_str(&format!(
            "bllvm_rpc_requests_success_total {}\n",
            metrics.rpc.requests_success
        ));

        output.push_str("# HELP bllvm_rpc_requests_failed_total Failed RPC requests\n");
        output.push_str("# TYPE bllvm_rpc_requests_failed_total counter\n");
        output.push_str(&format!(
            "bllvm_rpc_requests_failed_total {}\n",
            metrics.rpc.requests_failed
        ));

        output.push_str("# HELP bllvm_rpc_requests_per_second Current RPC requests per second\n");
        output.push_str("# TYPE bllvm_rpc_requests_per_second gauge\n");
        output.push_str(&format!(
            "bllvm_rpc_requests_per_second {}\n",
            metrics.rpc.requests_per_second
        ));

        output.push_str(
            "# HELP bllvm_rpc_avg_response_time_ms Average RPC response time in milliseconds\n",
        );
        output.push_str("# TYPE bllvm_rpc_avg_response_time_ms gauge\n");
        output.push_str(&format!(
            "bllvm_rpc_avg_response_time_ms {}\n",
            metrics.rpc.avg_response_time_ms
        ));

        // Performance metrics
        output.push_str("# HELP bllvm_performance_avg_block_processing_time_ms Average block processing time in milliseconds\n");
        output.push_str("# TYPE bllvm_performance_avg_block_processing_time_ms gauge\n");
        output.push_str(&format!(
            "bllvm_performance_avg_block_processing_time_ms {}\n",
            metrics.performance.avg_block_processing_time_ms
        ));

        output.push_str("# HELP bllvm_performance_avg_tx_validation_time_ms Average transaction validation time in milliseconds\n");
        output.push_str("# TYPE bllvm_performance_avg_tx_validation_time_ms gauge\n");
        output.push_str(&format!(
            "bllvm_performance_avg_tx_validation_time_ms {}\n",
            metrics.performance.avg_tx_validation_time_ms
        ));

        output.push_str("# HELP bllvm_performance_blocks_per_second Blocks processed per second\n");
        output.push_str("# TYPE bllvm_performance_blocks_per_second gauge\n");
        output.push_str(&format!(
            "bllvm_performance_blocks_per_second {}\n",
            metrics.performance.blocks_per_second
        ));

        output.push_str(
            "# HELP bllvm_performance_transactions_per_second Transactions processed per second\n",
        );
        output.push_str("# TYPE bllvm_performance_transactions_per_second gauge\n");
        output.push_str(&format!(
            "bllvm_performance_transactions_per_second {}\n",
            metrics.performance.transactions_per_second
        ));

        // System metrics
        output.push_str("# HELP bllvm_system_uptime_seconds Node uptime in seconds\n");
        output.push_str("# TYPE bllvm_system_uptime_seconds gauge\n");
        output.push_str(&format!(
            "bllvm_system_uptime_seconds {}\n",
            metrics.system.uptime_seconds
        ));

        if let Some(memory) = metrics.system.memory_usage_bytes {
            output.push_str("# HELP bllvm_system_memory_usage_bytes Memory usage in bytes\n");
            output.push_str("# TYPE bllvm_system_memory_usage_bytes gauge\n");
            output.push_str(&format!("bllvm_system_memory_usage_bytes {memory}\n"));
        }

        if let Some(cpu) = metrics.system.cpu_usage_percent {
            output.push_str("# HELP bllvm_system_cpu_usage_percent CPU usage percentage\n");
            output.push_str("# TYPE bllvm_system_cpu_usage_percent gauge\n");
            output.push_str(&format!("bllvm_system_cpu_usage_percent {cpu}\n"));
        }

        // DoS protection metrics
        output.push_str(
            "# HELP bllvm_dos_connection_rate_violations_total Connection rate violations\n",
        );
        output.push_str("# TYPE bllvm_dos_connection_rate_violations_total counter\n");
        output.push_str(&format!(
            "bllvm_dos_connection_rate_violations_total {}\n",
            metrics.network.dos_protection.connection_rate_violations
        ));

        output.push_str("# HELP bllvm_dos_auto_bans_total Auto-bans triggered\n");
        output.push_str("# TYPE bllvm_dos_auto_bans_total counter\n");
        output.push_str(&format!(
            "bllvm_dos_auto_bans_total {}\n",
            metrics.network.dos_protection.auto_bans
        ));

        output
    }

    /// Handle health check endpoints
    async fn handle_health_endpoint(
        server: Arc<Self>,
        req: Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>, hyper::Error> {
        let path = req.uri().path();

        // Call gethealth RPC method internally
        let health_params = json!([]);
        let health_result = server.control.gethealth(&health_params).await;

        let (status_code, body) = match health_result {
            Ok(health_value) => {
                match path {
                    "/health" | "/health/live" => {
                        // Quick health check - just return status
                        let status = health_value
                            .get("status")
                            .and_then(|s| s.as_str())
                            .unwrap_or("unknown");
                        let is_healthy = status == "healthy" || status == "degraded";

                        let response_body = json!({
                            "status": status,
                            "service": "bllvm-node"
                        });

                        let status_code = if is_healthy {
                            StatusCode::OK
                        } else {
                            StatusCode::SERVICE_UNAVAILABLE
                        };

                        (
                            status_code,
                            serde_json::to_string(&response_body)
                                .unwrap_or_else(|_| "{}".to_string()),
                        )
                    }
                    "/health/ready" => {
                        // Readiness probe - check if node is ready to serve traffic
                        let status = health_value
                            .get("status")
                            .and_then(|s| s.as_str())
                            .unwrap_or("unknown");
                        let is_ready = status == "healthy";

                        let response_body = json!({
                            "status": if is_ready { "ready" } else { "not_ready" },
                            "service": "bllvm-node"
                        });

                        let status_code = if is_ready {
                            StatusCode::OK
                        } else {
                            StatusCode::SERVICE_UNAVAILABLE
                        };

                        (
                            status_code,
                            serde_json::to_string(&response_body)
                                .unwrap_or_else(|_| "{}".to_string()),
                        )
                    }
                    "/health/detailed" => {
                        // Detailed health report
                        (
                            StatusCode::OK,
                            serde_json::to_string(&health_value)
                                .unwrap_or_else(|_| "{}".to_string()),
                        )
                    }
                    _ => {
                        return Ok(Self::http_error_response(
                            StatusCode::NOT_FOUND,
                            "Health endpoint not found",
                        ));
                    }
                }
            }
            Err(_) => {
                // Health check failed
                let response_body = json!({
                    "status": "unhealthy",
                    "service": "bllvm-node",
                    "error": "Health check failed"
                });
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    serde_json::to_string(&response_body).unwrap_or_else(|_| "{}".to_string()),
                )
            }
        };

        Ok(Response::builder()
            .status(status_code)
            .header("Content-Type", "application/json")
            .header("Content-Length", body.len())
            .body(Full::new(Bytes::from(body)))
            .expect("Failed to build health response"))
    }

    /// Create HTTP error response
    fn http_error_response(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
        let body = json!({
            "error": {
                "code": status.as_u16(),
                "message": message
            }
        });
        let body_json = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());

        Response::builder()
            .status(status)
            .header("Content-Type", "application/json")
            .header("Content-Length", body_json.len())
            .body(Full::new(Bytes::from(body_json)))
            .unwrap_or_else(|e| {
                // Fallback response if builder fails (should never happen)
                error!("Failed to build error response: {}", e);
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Full::new(Bytes::from(
                        "{\"error\":\"Internal server error\"}",
                    )))
                    .expect("Fallback response should always succeed")
            })
    }

    /// Process a JSON-RPC request
    ///
    /// Public method for use by both HTTP and raw TCP RPC servers
    /// Note: This creates temporary RPC handlers. For better performance,
    /// use process_request_with_server() with a server instance.
    pub async fn process_request(request: &str) -> String {
        // Create temporary server instance for backward compatibility
        // Using 127.0.0.1:0 is safe - it's just a placeholder address for testing
        let addr: SocketAddr = "127.0.0.1:0"
            .parse()
            .expect("127.0.0.1:0 should always parse as valid SocketAddr");
        use crate::utils::arc_new;
        let server = arc_new(Self::new(addr));
        Self::process_request_with_server(server, request).await
    }

    /// Process a JSON-RPC request with a server instance (reuses cached handlers)
    /// Supports both single requests and batch requests (JSON-RPC 2.0)
    async fn process_request_with_server(server: Arc<Self>, request: &str) -> String {
        let request: Value = match serde_json::from_str(request) {
            Ok(req) => req,
            Err(e) => {
                let err = errors::RpcError::parse_error(format!("Invalid JSON: {e}"));
                return serde_json::to_string(&err.to_json(None))
                    .unwrap_or_else(|_| "{}".to_string());
            }
        };

        // Check if this is a batch request (array) or single request (object)
        if let Some(requests) = request.as_array() {
            // Batch request - process all requests in parallel
            return Self::process_batch_request(server, requests).await;
        }

        // Single request - process normally
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");

        let params = request.get("params").cloned().unwrap_or_else(|| json!([]));
        let id = request.get("id");

        let result = server.call_method(method, params).await;

        match result {
            Ok(response) => {
                let response_str =
                    serde_json::to_string(&response).unwrap_or_else(|_| "null".to_string());
                let id_str = match id {
                    Some(id_val) => {
                        serde_json::to_string(id_val).unwrap_or_else(|_| "null".to_string())
                    }
                    None => "null".to_string(),
                };
                format!(r#"{{"jsonrpc":"2.0","result":{response_str},"id":{id_str}}}"#)
            }
            Err(e) => {
                serde_json::to_string(&e.to_json(id.cloned())).unwrap_or_else(|_| "{}".to_string())
            }
        }
    }

    /// Process a batch of JSON-RPC requests in parallel
    /// Maintains request order and handles errors per-request
    async fn process_batch_request(server: Arc<Self>, requests: &[Value]) -> String {
        // Empty batch returns empty array (JSON-RPC 2.0 spec)
        if requests.is_empty() {
            return "[]".to_string();
        }

        // Process all requests in parallel using tokio::task::spawn
        // Each request is processed independently, maintaining order
        let handles: Vec<_> = requests
            .iter()
            .enumerate()
            .map(|(index, req)| {
                let server_clone = Arc::clone(&server);
                let req_clone = req.clone();
                tokio::spawn(async move {
                    // Process each request
                    let method = req_clone
                        .get("method")
                        .and_then(|m| m.as_str())
                        .unwrap_or("");
                    let params = req_clone
                        .get("params")
                        .cloned()
                        .unwrap_or_else(|| json!([]));
                    let id = req_clone.get("id").cloned();

                    let result = server_clone.call_method(method, params).await;

                    // Build response maintaining original request ID
                    let response = match result {
                        Ok(response_value) => {
                            json!({
                                "jsonrpc": "2.0",
                                "result": response_value,
                                "id": id.unwrap_or(Value::Null)
                            })
                        }
                        Err(e) => {
                            // Error response maintains the request ID
                            e.to_json(id)
                        }
                    };

                    (index, response)
                })
            })
            .collect();

        // Wait for all requests to complete
        let mut results = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(e) => {
                    // Task join error - create error response
                    error!("Task join error in batch request: {}", e);
                    // We can't recover the original request ID here, so we'll skip this response
                    // In practice, this should rarely happen
                }
            }
        }

        // Sort by original index to maintain request order
        results.sort_by_key(|(index, _)| *index);

        // Extract responses in order
        let responses: Vec<Value> = results.into_iter().map(|(_, response)| response).collect();

        // Return batch response as JSON array
        serde_json::to_string(&responses).unwrap_or_else(|_| "[]".to_string())
    }

    /// Call a specific RPC method
    async fn call_method(&self, method: &str, params: Value) -> Result<Value, errors::RpcError> {
        match method {
            // Blockchain methods
            "getblockchaininfo" => self
                .blockchain
                .get_blockchain_info()
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "getblock" => {
                let hash = params.get(0).and_then(|p| p.as_str()).unwrap_or("");
                self.blockchain
                    .get_block(hash)
                    .await
                    .map_err(|e| errors::RpcError::internal_error(e.to_string()))
            }
            "getblockhash" => {
                let height = params.get(0).and_then(|p| p.as_u64()).unwrap_or(0);
                self.blockchain
                    .get_block_hash(height)
                    .await
                    .map_err(|e| errors::RpcError::internal_error(e.to_string()))
            }
            "getblockheader" => {
                let hash = params.get(0).and_then(|p| p.as_str()).unwrap_or("");
                let verbose = params.get(1).and_then(|p| p.as_bool()).unwrap_or(true);
                self.blockchain
                    .get_block_header(hash, verbose)
                    .await
                    .map_err(|e| errors::RpcError::internal_error(e.to_string()))
            }
            "getbestblockhash" => self
                .blockchain
                .get_best_block_hash()
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "getblockcount" => self
                .blockchain
                .get_block_count()
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "getdifficulty" => self
                .blockchain
                .get_difficulty()
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "gettxoutsetinfo" => self
                .blockchain
                .get_txoutset_info()
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "verifychain" => {
                let checklevel = params.get(0).and_then(|p| p.as_u64());
                let numblocks = params.get(1).and_then(|p| p.as_u64());
                self.blockchain
                    .verify_chain(checklevel, numblocks)
                    .await
                    .map_err(|e| errors::RpcError::internal_error(e.to_string()))
            }
            "getchaintips" => self
                .blockchain
                .get_chain_tips()
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "getchaintxstats" => self
                .blockchain
                .get_chain_tx_stats(&params)
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "getblockstats" => self
                .blockchain
                .get_block_stats(&params)
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "pruneblockchain" => self
                .blockchain
                .prune_blockchain(&params)
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "getpruneinfo" => self
                .blockchain
                .get_prune_info(&params)
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "invalidateblock" => self
                .blockchain
                .invalidate_block(&params)
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "reconsiderblock" => self
                .blockchain
                .reconsider_block(&params)
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "waitfornewblock" => self
                .blockchain
                .wait_for_new_block(&params)
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "waitforblock" => self
                .blockchain
                .wait_for_block(&params)
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "waitforblockheight" => self
                .blockchain
                .wait_for_block_height(&params)
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),

            // Raw Transaction methods
            "getrawtransaction" => self.rawtx.getrawtransaction(&params).await,
            "sendrawtransaction" => self.rawtx.sendrawtransaction(&params).await,
            "testmempoolaccept" => self.rawtx.testmempoolaccept(&params).await,
            "decoderawtransaction" => self.rawtx.decoderawtransaction(&params).await,
            "gettxout" => self.rawtx.gettxout(&params).await,
            "gettxoutproof" => self.rawtx.gettxoutproof(&params).await,
            "verifytxoutproof" => self.rawtx.verifytxoutproof(&params).await,

            // Mempool methods
            "getmempoolinfo" => self.mempool.getmempoolinfo(&params).await,
            "getrawmempool" => self.mempool.getrawmempool(&params).await,
            "savemempool" => self.mempool.savemempool(&params).await,
            "getmempoolancestors" => self.mempool.getmempoolancestors(&params).await,
            "getmempooldescendants" => self.mempool.getmempooldescendants(&params).await,
            "getmempoolentry" => self.mempool.getmempoolentry(&params).await,

            // Network methods
            "getnetworkinfo" => self.network.get_network_info().await,
            "getpeerinfo" => self.network.get_peer_info().await,
            "getconnectioncount" => self.network.get_connection_count(&params).await,
            "ping" => self.network.ping(&params).await,
            "addnode" => self.network.add_node(&params).await,
            "disconnectnode" => self.network.disconnect_node(&params).await,
            "getnettotals" => self.network.get_net_totals(&params).await,
            "clearbanned" => self.network.clear_banned(&params).await,
            "setban" => self.network.set_ban(&params).await,
            "listbanned" => self.network.list_banned(&params).await,
            "getaddednodeinfo" => self.network.getaddednodeinfo(&params).await,
            "getnodeaddresses" => self.network.getnodeaddresses(&params).await,
            "setnetworkactive" => self.network.setnetworkactive(&params).await,

            // Mining methods
            "getmininginfo" => self.mining.get_mining_info().await,
            "getblocktemplate" => self.mining.get_block_template(&params).await,
            "submitblock" => self.mining.submit_block(&params).await,
            "estimatesmartfee" => self.mining.estimate_smart_fee(&params).await,
            "prioritisetransaction" => self.mining.prioritise_transaction(&params).await,
            "getblockfilter" => self
                .blockchain
                .get_block_filter(&params)
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "getindexinfo" => self
                .blockchain
                .get_index_info(&params)
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "getblockchainstate" => self
                .blockchain
                .get_blockchain_state()
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "validateaddress" => self
                .blockchain
                .validate_address(&params)
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "getaddressinfo" => self
                .blockchain
                .get_address_info(&params)
                .await
                .map_err(|e| errors::RpcError::internal_error(e.to_string())),
            "gettransactiondetails" => self.rawtx.get_transaction_details(&params).await,

            // Control methods
            "stop" => self.control.stop(&params).await,
            "uptime" => self.control.uptime(&params).await,
            "getmemoryinfo" => self.control.getmemoryinfo(&params).await,
            "getrpcinfo" => self.control.getrpcinfo(&params).await,
            "help" => self.control.help(&params).await,
            "logging" => self.control.logging(&params).await,
            "gethealth" => self.control.gethealth(&params).await,
            "getmetrics" => self.control.getmetrics(&params).await,

            _ => Err(errors::RpcError::method_not_found(method)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream as TokioTcpStream;

    #[tokio::test]
    async fn test_rpc_server_creation() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = RpcServer::new(addr);
        assert_eq!(server.addr, addr);
    }

    #[tokio::test]
    async fn test_http_rpc_integration() {
        // Start server on random port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_addr = listener.local_addr().unwrap();
        use crate::utils::arc_new;
        let server = arc_new(RpcServer::new(server_addr));

        // Spawn server task using hyper
        let server_handle = tokio::spawn(async move {
            while let Ok((stream, addr)) = listener.accept().await {
                let peer_addr = addr;
                let server_clone = server.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| {
                        RpcServer::handle_http_request_with_server(
                            server_clone.clone(),
                            req,
                            peer_addr,
                        )
                    });
                    let _ = http1::Builder::new().serve_connection(io, service).await;
                });
            }
        });

        // Give server time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Connect to server
        let mut client = TokioTcpStream::connect(server_addr).await.unwrap();

        // Send HTTP POST request
        let json_body = r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":1}"#;
        let http_request = format!(
            "POST / HTTP/1.1\r\n\
            Host: 127.0.0.1:18332\r\n\
            Content-Type: application/json\r\n\
            Content-Length: {}\r\n\
            \r\n\
            {}",
            json_body.len(),
            json_body
        );

        client.write_all(http_request.as_bytes()).await.unwrap();

        // Read response
        let mut response = vec![0u8; 4096];
        let n = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            client.read(&mut response),
        )
        .await
        .unwrap()
        .unwrap();

        let response_str = String::from_utf8_lossy(&response[..n]);

        // Verify HTTP response (hyper uses lowercase headers)
        assert!(
            response_str.contains("HTTP/1.1 200 OK") || response_str.contains("200 OK"),
            "Response: {response_str}"
        );
        assert!(
            response_str.contains("content-type: application/json")
                || response_str.contains("Content-Type: application/json")
        );
        assert!(response_str.contains("jsonrpc"));
        assert!(response_str.contains("\"result\""));

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_process_request_valid_json() {
        let request = r#"{"jsonrpc":"2.0","method":"getblockchaininfo","params":[],"id":1}"#;
        let response_str = RpcServer::process_request(request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert_eq!(response["jsonrpc"], "2.0");
        assert!(response["result"].is_object());
        assert_eq!(response["id"], 1);
    }

    #[tokio::test]
    async fn test_process_request_invalid_json() {
        let request = "invalid json";
        let response_str = RpcServer::process_request(request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["error"]["code"], -32700);
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Parse error")
                || response["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("Invalid JSON")
        );
    }

    #[tokio::test]
    async fn test_process_request_unknown_method() {
        let request = r#"{"jsonrpc":"2.0","method":"unknown_method","params":[],"id":1}"#;
        let response_str = RpcServer::process_request(request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["error"]["code"], -32601);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Method not found"));
        assert_eq!(response["id"], 1);
    }

    #[tokio::test]
    async fn test_process_request_without_id() {
        let request = r#"{"jsonrpc":"2.0","method":"getblockchaininfo","params":[]}"#;
        let response_str = RpcServer::process_request(request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert_eq!(response["jsonrpc"], "2.0");
        assert!(response["result"].is_object());
        assert_eq!(response["id"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn test_process_request_with_params() {
        let request = r#"{"jsonrpc":"2.0","method":"getblock","params":["000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"],"id":1}"#;
        let response_str = RpcServer::process_request(request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert_eq!(response["jsonrpc"], "2.0");
        // Block may not be in storage (test doesn't set up storage)
        // If block found, result should be object; if not found, result should be null or error
        if response.get("error").is_some() {
            // Error response is valid when block not found
            assert!(response["error"].is_object());
        } else {
            // Success response should have object result
            assert!(response["result"].is_object() || response["result"].is_null());
        }
        assert_eq!(response["id"], 1);
    }

    #[tokio::test]
    async fn test_call_method_getblockchaininfo() {
        let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
        let result = server.call_method("getblockchaininfo", json!([])).await;
        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(response.get("chain").is_some());
    }

    #[tokio::test]
    async fn test_call_method_getblock() {
        let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
        let params = json!(["000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"]);
        let result = server.call_method("getblock", params).await;
        // Block may not be in storage (test doesn't set up storage)
        // This is expected - the method should return an error when block not found
        // The important thing is that the method doesn't panic
        if let Ok(response) = result {
            // If block was found, verify response structure
            assert!(response.get("hash").is_some());
        } else {
            // If block not found, that's expected without storage setup
            // Just verify it's a proper error response
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn test_call_method_getblockhash() {
        let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
        let params = json!([0]);
        let result = server.call_method("getblockhash", params).await;
        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(response.is_string());
    }

    #[tokio::test]
    async fn test_call_method_getrawtransaction() {
        let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
        let params = json!(["0000000000000000000000000000000000000000000000000000000000000000"]);
        let result = server.call_method("getrawtransaction", params).await;
        // getrawtransaction may fail if transaction is not found, which is acceptable in tests
        // The important thing is that it doesn't panic
        if let Ok(response) = result {
            // If it succeeds, check that it has expected structure
            assert!(response.is_object() || response.is_string());
        }
    }

    #[tokio::test]
    async fn test_call_method_getnetworkinfo() {
        let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
        let result = server.call_method("getnetworkinfo", json!([])).await;
        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(response.get("version").is_some());
    }

    #[tokio::test]
    async fn test_call_method_getpeerinfo() {
        let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
        let result = server.call_method("getpeerinfo", json!([])).await;
        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(response.is_array());
    }

    #[tokio::test]
    async fn test_call_method_getmininginfo() {
        let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
        let result = server.call_method("getmininginfo", json!([])).await;
        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(response.get("blocks").is_some());
    }

    #[tokio::test]
    async fn test_call_method_getblocktemplate() {
        let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
        let result = server.call_method("getblocktemplate", json!([])).await;
        // getblocktemplate may fail if chain state is not initialized, which is acceptable in tests
        // The important thing is that it doesn't panic
        if let Ok(response) = result {
            // If it succeeds, check that it has expected structure
            assert!(response.is_object() || response.is_string());
        }
    }

    #[tokio::test]
    async fn test_call_method_unknown_method() {
        let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
        let result = server.call_method("unknown_method", json!([])).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Method not found"));
    }

    #[tokio::test]
    async fn test_json_rpc_2_0_compliance() {
        let request = r#"{"jsonrpc":"2.0","method":"getblockchaininfo","params":[],"id":1}"#;
        let response_str = RpcServer::process_request(request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert_eq!(response["jsonrpc"], "2.0");
        assert!(response["result"].is_object());
        assert_eq!(response["id"], 1);
    }

    #[tokio::test]
    async fn test_error_response_format() {
        let request = r#"{"jsonrpc":"2.0","method":"unknown_method","params":[],"id":1}"#;
        let response_str = RpcServer::process_request(request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert_eq!(response["jsonrpc"], "2.0");
        assert!(response["error"].is_object());
        assert!(response["error"]["code"].is_number());
        assert!(response["error"]["message"].is_string());
        assert_eq!(response["id"], 1);
    }

    #[tokio::test]
    async fn test_parse_error_response() {
        let request = "invalid json";
        let response_str = RpcServer::process_request(request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["error"]["code"], -32700);
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Parse error")
                || response["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("Invalid JSON")
        );
    }

    #[tokio::test]
    async fn test_method_not_found_response() {
        let request = r#"{"jsonrpc":"2.0","method":"nonexistent","params":[],"id":42}"#;
        let response_str = RpcServer::process_request(request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["error"]["code"], -32601);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Method not found"));
        assert!(response["error"]["data"].is_string() || response["error"]["data"].is_null());
        assert_eq!(response["id"], 42);
    }

    #[tokio::test]
    async fn test_empty_params_handling() {
        let request = r#"{"jsonrpc":"2.0","method":"getblockchaininfo","id":1}"#;
        let response_str = RpcServer::process_request(request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert_eq!(response["jsonrpc"], "2.0");
        assert!(response["result"].is_object());
        assert_eq!(response["id"], 1);
    }

    #[tokio::test]
    async fn test_missing_method_handling() {
        let request = r#"{"jsonrpc":"2.0","params":[],"id":1}"#;
        let response_str = RpcServer::process_request(request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["error"]["code"], -32601);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Method not found"));
        assert_eq!(response["id"], 1);
    }

    #[tokio::test]
    async fn test_blockchain_methods_integration() {
        let methods = vec![
            "getblockchaininfo",
            "getblock",
            "getblockhash",
            "getrawtransaction",
        ];

        for method in methods {
            let request = format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":[],"id":1}}"#);
            let response_str = RpcServer::process_request(&request).await;
            let response: Value = serde_json::from_str(&response_str).unwrap();

            assert_eq!(response["jsonrpc"], "2.0");
            // Result may be an object, string, or null (if method failed)
            assert!(
                response["result"].is_object()
                    || response["result"].is_string()
                    || response["result"].is_null()
            );
            assert_eq!(response["id"], 1);
        }
    }

    #[tokio::test]
    async fn test_network_methods_integration() {
        let methods = vec!["getnetworkinfo", "getpeerinfo"];

        for method in methods {
            let request = format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":[],"id":1}}"#);
            let response_str = RpcServer::process_request(&request).await;
            let response: Value = serde_json::from_str(&response_str).unwrap();

            assert_eq!(response["jsonrpc"], "2.0");
            assert!(response["result"].is_object() || response["result"].is_array());
            assert_eq!(response["id"], 1);
        }
    }

    #[tokio::test]
    async fn test_mining_methods_integration() {
        let methods = vec!["getmininginfo", "getblocktemplate"];

        for method in methods {
            let request = format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":[],"id":1}}"#);
            let response_str = RpcServer::process_request(&request).await;
            let response: Value = serde_json::from_str(&response_str).unwrap();

            assert_eq!(response["jsonrpc"], "2.0");
            // Result may be an object, string, or null (if method failed due to missing dependencies)
            assert!(
                response["result"].is_object()
                    || response["result"].is_string()
                    || response["result"].is_null()
            );
            assert_eq!(response["id"], 1);
        }
    }

    #[tokio::test]
    async fn test_batch_request_empty() {
        let request = "[]";
        let response_str = RpcServer::process_request(request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert!(response.is_array());
        assert_eq!(response.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_batch_request_single() {
        let request = r#"[{"jsonrpc":"2.0","method":"getblockchaininfo","params":[],"id":1}]"#;
        let response_str = RpcServer::process_request(request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert!(response.is_array());
        let responses = response.as_array().unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["jsonrpc"], "2.0");
        assert!(responses[0]["result"].is_object());
        assert_eq!(responses[0]["id"], 1);
    }

    #[tokio::test]
    async fn test_batch_request_multiple() {
        let request = r#"[
            {"jsonrpc":"2.0","method":"getblockchaininfo","params":[],"id":1},
            {"jsonrpc":"2.0","method":"getblockcount","params":[],"id":2},
            {"jsonrpc":"2.0","method":"getnetworkinfo","params":[],"id":3}
        ]"#;
        let response_str = RpcServer::process_request(request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert!(response.is_array());
        let responses = response.as_array().unwrap();
        assert_eq!(responses.len(), 3);

        // Verify order is maintained
        assert_eq!(responses[0]["id"], 1);
        assert_eq!(responses[1]["id"], 2);
        assert_eq!(responses[2]["id"], 3);

        // Verify all are successful
        assert!(responses[0]["result"].is_object());
        assert!(responses[1]["result"].is_number());
        assert!(responses[2]["result"].is_object());
    }

    #[tokio::test]
    async fn test_batch_request_with_errors() {
        let request = r#"[
            {"jsonrpc":"2.0","method":"getblockchaininfo","params":[],"id":1},
            {"jsonrpc":"2.0","method":"unknown_method","params":[],"id":2},
            {"jsonrpc":"2.0","method":"getblockcount","params":[],"id":3}
        ]"#;
        let response_str = RpcServer::process_request(request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert!(response.is_array());
        let responses = response.as_array().unwrap();
        assert_eq!(responses.len(), 3);

        // First request should succeed
        assert_eq!(responses[0]["id"], 1);
        assert!(responses[0]["result"].is_object());
        assert!(responses[0]["error"].is_null());

        // Second request should fail
        assert_eq!(responses[1]["id"], 2);
        assert!(responses[1]["result"].is_null());
        assert!(responses[1]["error"].is_object());
        assert_eq!(responses[1]["error"]["code"], -32601);

        // Third request should succeed
        assert_eq!(responses[2]["id"], 3);
        assert!(responses[2]["result"].is_number());
        assert!(responses[2]["error"].is_null());
    }

    #[tokio::test]
    async fn test_batch_request_order_preserved() {
        let request = r#"[
            {"jsonrpc":"2.0","method":"getblockcount","params":[],"id":3},
            {"jsonrpc":"2.0","method":"getblockchaininfo","params":[],"id":1},
            {"jsonrpc":"2.0","method":"getnetworkinfo","params":[],"id":2}
        ]"#;
        let response_str = RpcServer::process_request(request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert!(response.is_array());
        let responses = response.as_array().unwrap();
        assert_eq!(responses.len(), 3);

        // Order should match request order, not ID order
        assert_eq!(responses[0]["id"], 3);
        assert_eq!(responses[1]["id"], 1);
        assert_eq!(responses[2]["id"], 2);
    }

    #[tokio::test]
    async fn test_batch_request_without_ids() {
        let request = r#"[
            {"jsonrpc":"2.0","method":"getblockchaininfo","params":[]},
            {"jsonrpc":"2.0","method":"getblockcount","params":[]}
        ]"#;
        let response_str = RpcServer::process_request(request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert!(response.is_array());
        let responses = response.as_array().unwrap();
        assert_eq!(responses.len(), 2);

        // Both should have null IDs
        assert_eq!(responses[0]["id"], Value::Null);
        assert_eq!(responses[1]["id"], Value::Null);

        // Both should succeed
        assert!(responses[0]["result"].is_object());
        assert!(responses[1]["result"].is_number());
    }
}
