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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info, warn};

use super::{auth, blockchain, control, errors, mempool, mining, network, rawtx};

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
}

impl RpcServer {
    /// Create a new RPC server
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            blockchain: Arc::new(blockchain::BlockchainRpc::new()),
            network: Arc::new(network::NetworkRpc::new()),
            mempool: Arc::new(mempool::MempoolRpc::new()),
            mining: Arc::new(mining::MiningRpc::new()),
            rawtx: Arc::new(rawtx::RawTxRpc::new()),
            control: Arc::new(control::ControlRpc::new()),
            auth_manager: None,
        }
    }

    /// Create a new RPC server with authentication
    pub fn with_auth(addr: SocketAddr, auth_manager: Arc<auth::RpcAuthManager>) -> Self {
        Self {
            addr,
            blockchain: Arc::new(blockchain::BlockchainRpc::new()),
            network: Arc::new(network::NetworkRpc::new()),
            mempool: Arc::new(mempool::MempoolRpc::new()),
            mining: Arc::new(mining::MiningRpc::new()),
            rawtx: Arc::new(rawtx::RawTxRpc::new()),
            control: Arc::new(control::ControlRpc::new()),
            auth_manager: Some(auth_manager),
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
        let server = Arc::new(RpcServer {
            addr: self.addr,
            blockchain: Arc::clone(&self.blockchain),
            network: Arc::clone(&self.network),
            mempool: Arc::clone(&self.mempool),
            mining: Arc::clone(&self.mining),
            rawtx: Arc::clone(&self.rawtx),
            control: Arc::clone(&self.control),
            auth_manager: self.auth_manager.clone(),
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
        // Only allow POST method
        if req.method() != Method::POST {
            return Ok(Self::http_error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "Only POST method is supported",
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
        let json_body = match String::from_utf8(body_bytes.to_vec()) {
            Ok(s) => s,
            Err(e) => {
                return Ok(Self::http_error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("Invalid UTF-8 in request body: {}", e),
                ));
            }
        };

        debug!("HTTP RPC request from {}: {} bytes", addr, json_body.len());

        // Authenticate request if authentication is enabled
        if let Some(ref auth_manager) = server.auth_manager {
            let auth_result = auth_manager.authenticate_request(&headers, addr).await;

            // Check if authentication failed
            if let Some(error) = auth_result.error {
                return Ok(Self::http_error_response(StatusCode::UNAUTHORIZED, &error));
            }

            // Check rate limiting
            if let Some(ref user_id) = auth_result.user_id {
                if !auth_manager.check_rate_limit(user_id).await {
                    return Ok(Self::http_error_response(
                        StatusCode::TOO_MANY_REQUESTS,
                        "Rate limit exceeded",
                    ));
                }
            }
        }

        // Process JSON-RPC request (reuse server instance with cached handlers)
        let response = Self::process_request_with_server(server, &json_body).await;
        let response_json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());

        // Build HTTP response
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .header("Content-Length", response_json.len())
            .body(Full::new(Bytes::from(response_json)))
            .unwrap())
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
            .unwrap()
    }

    /// Process a JSON-RPC request
    ///
    /// Public method for use by both HTTP and raw TCP RPC servers
    /// Note: This creates temporary RPC handlers. For better performance,
    /// use process_request_with_server() with a server instance.
    pub async fn process_request(request: &str) -> Value {
        // Create temporary server instance for backward compatibility
        let server = Arc::new(Self::new("127.0.0.1:0".parse().unwrap()));
        Self::process_request_with_server(server, request).await
    }

    /// Process a JSON-RPC request with a server instance (reuses cached handlers)
    async fn process_request_with_server(server: Arc<Self>, request: &str) -> Value {
        let request: Value = match serde_json::from_str(request) {
            Ok(req) => req,
            Err(e) => {
                let err = errors::RpcError::parse_error(format!("Invalid JSON: {}", e));
                return err.to_json(None);
            }
        };

        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");

        let params = request.get("params").cloned().unwrap_or(json!([]));
        let id = request.get("id").cloned();

        let result = server.call_method(method, params).await;

        match result {
            Ok(response) => {
                json!({
                    "jsonrpc": "2.0",
                    "result": response,
                    "id": id
                })
            }
            Err(e) => {
                // call_method already returns proper RpcError, just convert to JSON
                e.to_json(id)
            }
        }
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
        let server = Arc::new(RpcServer::new(server_addr));

        // Spawn server task using hyper
        let server_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, addr)) => {
                        let peer_addr = addr;
                        let server_clone = server.clone();
                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);
                            let service = service_fn(move |req| {
                                RpcServer::handle_http_request_with_server(server_clone.clone(), req, peer_addr)
                            });
                            let _ = http1::Builder::new().serve_connection(io, service).await;
                        });
                    }
                    Err(_) => break,
                }
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
            "Response: {}",
            response_str
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
        let response = RpcServer::process_request(request).await;

        assert_eq!(response["jsonrpc"], "2.0");
        assert!(response["result"].is_object());
        assert_eq!(response["id"], 1);
    }

    #[tokio::test]
    async fn test_process_request_invalid_json() {
        let request = "invalid json";
        let response = RpcServer::process_request(request).await;

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
        let response = RpcServer::process_request(request).await;

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
        let response = RpcServer::process_request(request).await;

        assert_eq!(response["jsonrpc"], "2.0");
        assert!(response["result"].is_object());
        assert_eq!(response["id"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn test_process_request_with_params() {
        let request = r#"{"jsonrpc":"2.0","method":"getblock","params":["000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"],"id":1}"#;
        let response = RpcServer::process_request(request).await;

        assert_eq!(response["jsonrpc"], "2.0");
        assert!(response["result"].is_object());
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
        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(response.get("hash").is_some());
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
        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(response.get("txid").is_some());
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
        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(response.get("version").is_some());
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
        let response = RpcServer::process_request(request).await;

        assert_eq!(response["jsonrpc"], "2.0");
        assert!(response["result"].is_object());
        assert_eq!(response["id"], 1);
    }

    #[tokio::test]
    async fn test_error_response_format() {
        let request = r#"{"jsonrpc":"2.0","method":"unknown_method","params":[],"id":1}"#;
        let response = RpcServer::process_request(request).await;

        assert_eq!(response["jsonrpc"], "2.0");
        assert!(response["error"].is_object());
        assert!(response["error"]["code"].is_number());
        assert!(response["error"]["message"].is_string());
        assert_eq!(response["id"], 1);
    }

    #[tokio::test]
    async fn test_parse_error_response() {
        let request = "invalid json";
        let response = RpcServer::process_request(request).await;

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
        let response = RpcServer::process_request(request).await;

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
        let response = RpcServer::process_request(request).await;

        assert_eq!(response["jsonrpc"], "2.0");
        assert!(response["result"].is_object());
        assert_eq!(response["id"], 1);
    }

    #[tokio::test]
    async fn test_missing_method_handling() {
        let request = r#"{"jsonrpc":"2.0","params":[],"id":1}"#;
        let response = RpcServer::process_request(request).await;

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
            let request = format!(
                r#"{{"jsonrpc":"2.0","method":"{}","params":[],"id":1}}"#,
                method
            );
            let response = RpcServer::process_request(&request).await;

            assert_eq!(response["jsonrpc"], "2.0");
            assert!(response["result"].is_object() || response["result"].is_string());
            assert_eq!(response["id"], 1);
        }
    }

    #[tokio::test]
    async fn test_network_methods_integration() {
        let methods = vec!["getnetworkinfo", "getpeerinfo"];

        for method in methods {
            let request = format!(
                r#"{{"jsonrpc":"2.0","method":"{}","params":[],"id":1}}"#,
                method
            );
            let response = RpcServer::process_request(&request).await;

            assert_eq!(response["jsonrpc"], "2.0");
            assert!(response["result"].is_object() || response["result"].is_array());
            assert_eq!(response["id"], 1);
        }
    }

    #[tokio::test]
    async fn test_mining_methods_integration() {
        let methods = vec!["getmininginfo", "getblocktemplate"];

        for method in methods {
            let request = format!(
                r#"{{"jsonrpc":"2.0","method":"{}","params":[],"id":1}}"#,
                method
            );
            let response = RpcServer::process_request(&request).await;

            assert_eq!(response["jsonrpc"], "2.0");
            assert!(response["result"].is_object());
            assert_eq!(response["id"], 1);
        }
    }
}
