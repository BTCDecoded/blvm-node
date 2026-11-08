//! Node-hosted inbound services (design 19.3).
//!
//! Operators bind listeners. A module with `host_service` handles `http`
//! (hyper HTTP/1.1) or `stream` (TCP). Unload and crash drop the listeners.
//!
//! Limits that already exist elsewhere:
//! - invocation and stream idle timeout: 30s, same as `IpcRpcHandler`
//!   (`Duration::from_secs(30)` in `module/rpc/ipc_handler.rs`)
//! - connection cap when `max_connections` is unset: 256, the figure in the
//!   `[services.*]` example
//! - per-peer rate: `ConnectionRateLimiter` with the RPC defaults
//!   `max_connections_per_ip_per_minute` (10) and
//!   `connection_rate_limit_window_seconds` (60)
//! - one stream read is capped at `rpc::server::DEFAULT_MAX_REQUEST_SIZE`
//!
//! `tls_cert`, when set, refuses a plaintext listener. This host does not
//! terminate TLS.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;
use tracing::info;

use crate::network::dos_protection::ConnectionRateLimiter;
use crate::rpc::server::DEFAULT_MAX_REQUEST_SIZE;

/// Same timeout as `IpcRpcHandler`.
pub const SERVICE_INVOCATION_TIMEOUT: Duration = Duration::from_secs(30);

/// `[services.*] max_connections` when the operator leaves it unset.
pub const DEFAULT_MAX_CONNECTIONS: u32 = 256;

/// RPC `max_connections_per_ip_per_minute`.
const PER_PEER_PER_MINUTE: usize = 10;
/// RPC `connection_rate_limit_window_seconds`.
const PER_PEER_WINDOW_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceKind {
    Http,
    Stream,
}

#[derive(Debug, Clone)]
pub struct BindRequest {
    pub name: String,
    pub kind: ServiceKind,
    pub listen: SocketAddr,
    pub max_connections: u32,
    pub tls_cert: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ServiceHttpRequest {
    pub service: String,
    pub method: String,
    pub path_and_query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ServiceHttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Open {
        conn_id: u64,
        peer: String,
    },
    Data {
        conn_id: u64,
        seq: u64,
        bytes: Vec<u8>,
    },
    Close {
        conn_id: u64,
    },
}

#[async_trait]
pub trait HostedModule: Send + Sync {
    async fn http(&self, req: ServiceHttpRequest) -> Result<ServiceHttpResponse, String>;
    async fn stream(&self, event: StreamEvent) -> Result<Vec<u8>, String>;
}

struct LiveConn {
    module: String,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    close: watch::Sender<bool>,
}

struct Bindings {
    shutdown: watch::Sender<bool>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    addrs: Mutex<Vec<SocketAddr>>,
}

struct Inner {
    modules: Mutex<HashMap<String, Arc<Bindings>>>,
    conns: Mutex<HashMap<u64, LiveConn>>,
    next_conn: AtomicU64,
    provides: Mutex<HashMap<String, Vec<String>>>,
}

/// Listeners owned by the node, keyed by module name.
#[derive(Clone)]
pub struct ServiceHost {
    inner: Arc<Inner>,
}

impl Default for ServiceHost {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceHost {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                modules: Mutex::new(HashMap::new()),
                conns: Mutex::new(HashMap::new()),
                next_conn: AtomicU64::new(1),
                provides: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub async fn bind(
        &self,
        module: &str,
        services: Vec<BindRequest>,
        handler: Arc<dyn HostedModule>,
    ) -> Result<Vec<SocketAddr>, String> {
        if services.is_empty() {
            return Ok(Vec::new());
        }
        if services
            .iter()
            .any(|svc| svc.tls_cert.as_deref().is_some_and(|cert| !cert.is_empty()))
        {
            return Err(
                "tls_cert is set; refusing to serve the hosted service in plaintext".to_string(),
            );
        }
        self.unbind(module).await;
        let (shutdown, _) = watch::channel(false);
        let bindings = Arc::new(Bindings {
            shutdown,
            tasks: Mutex::new(Vec::new()),
            addrs: Mutex::new(Vec::new()),
        });
        self.inner
            .modules
            .lock()
            .await
            .insert(module.to_string(), Arc::clone(&bindings));

        for svc in services {
            if let Err(err) = self
                .spawn_listener(module, &bindings, &svc, Arc::clone(&handler))
                .await
            {
                self.unbind(module).await;
                return Err(err);
            }
        }
        let addrs = bindings.addrs.lock().await.clone();
        info!(module = %module, ?addrs, "hosted service listeners bound");
        Ok(addrs)
    }

    async fn spawn_listener(
        &self,
        module: &str,
        bindings: &Arc<Bindings>,
        svc: &BindRequest,
        handler: Arc<dyn HostedModule>,
    ) -> Result<(), String> {
        let listener = TcpListener::bind(svc.listen)
            .await
            .map_err(|err| format!("bind {} ({}): {err}", svc.name, svc.listen))?;
        let addr = listener
            .local_addr()
            .map_err(|err| format!("local_addr {}: {err}", svc.name))?;
        bindings.addrs.lock().await.push(addr);
        let max = if svc.max_connections == 0 {
            DEFAULT_MAX_CONNECTIONS
        } else {
            svc.max_connections
        };
        let host = self.clone();
        let module = module.to_string();
        let name = svc.name.clone();
        let kind = svc.kind;
        let mut shutdown = bindings.shutdown.subscribe();
        let task_slot = Arc::clone(bindings);
        let task = tokio::spawn(async move {
            let inflight = Arc::new(AtomicUsize::new(0));
            let rates = Arc::new(Mutex::new(ConnectionRateLimiter::new(
                PER_PEER_PER_MINUTE,
                PER_PEER_WINDOW_SECS,
            )));
            loop {
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            break;
                        }
                    }
                    accepted = listener.accept() => {
                        let Ok((stream, peer)) = accepted else { break };
                        {
                            let mut rates = rates.lock().await;
                            if !rates.check_connection(peer.ip()) {
                                continue;
                            }
                        }
                        let prev = inflight.fetch_add(1, Ordering::SeqCst);
                        if prev >= max as usize {
                            inflight.fetch_sub(1, Ordering::SeqCst);
                            continue;
                        }
                        let guard = InflightGuard(Arc::clone(&inflight));
                        let host = host.clone();
                        let handler = Arc::clone(&handler);
                        let module = module.clone();
                        let name = name.clone();
                        let shutdown = task_slot.shutdown.subscribe();
                        let conn = tokio::spawn(async move {
                            let _guard = guard;
                            match kind {
                                ServiceKind::Http => {
                                    serve_http(host, module, name, stream, handler).await;
                                }
                                ServiceKind::Stream => {
                                    serve_stream(host, module, peer, stream, handler, shutdown).await;
                                }
                            }
                        });
                        task_slot.tasks.lock().await.push(conn);
                    }
                }
            }
        });
        bindings.tasks.lock().await.push(task);
        Ok(())
    }

    /// Drop listeners and connections for `module`.
    pub async fn unbind(&self, module: &str) {
        let removed = self.inner.modules.lock().await.remove(module);
        if let Some(bindings) = removed {
            let _ = bindings.shutdown.send(true);
            let tasks: Vec<_> = bindings.tasks.lock().await.drain(..).collect();
            for task in tasks {
                task.abort();
            }
            info!(module = %module, "hosted service listeners closed");
        }
        let mut conns = self.inner.conns.lock().await;
        let ids: Vec<u64> = conns
            .iter()
            .filter(|(_, conn)| conn.module == module)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(conn) = conns.remove(&id) {
                let _ = conn.close.send(true);
            }
        }
    }

    pub async fn bound_addrs(&self, module: &str) -> Vec<SocketAddr> {
        let modules = self.inner.modules.lock().await;
        match modules.get(module) {
            Some(bindings) => bindings.addrs.lock().await.clone(),
            None => Vec::new(),
        }
    }

    /// `StreamSend` from the module that owns `conn_id`.
    pub async fn write(&self, module: &str, conn_id: u64, bytes: &[u8]) -> Result<(), String> {
        let conns = self.inner.conns.lock().await;
        let Some(conn) = conns.get(&conn_id) else {
            return Err(format!("unknown connection {conn_id}"));
        };
        if conn.module != module {
            return Err(format!("connection {conn_id} is not owned by {module}"));
        }
        conn.tx
            .send(bytes.to_vec())
            .map_err(|_| format!("connection {conn_id} is closed"))
    }

    /// `StreamClose` from the module that owns `conn_id`.
    pub async fn close(&self, module: &str, conn_id: u64) -> Result<(), String> {
        let conns = self.inner.conns.lock().await;
        let Some(conn) = conns.get(&conn_id) else {
            return Err(format!("unknown connection {conn_id}"));
        };
        if conn.module != module {
            return Err(format!("connection {conn_id} is not owned by {module}"));
        }
        conn.close
            .send(true)
            .map_err(|_| format!("connection {conn_id} is closed"))?;
        Ok(())
    }

    pub async fn record_provides(&self, module: &str, apis: Vec<String>) {
        self.inner
            .provides
            .lock()
            .await
            .insert(module.to_string(), apis);
    }

    pub async fn forget(&self, module: &str) {
        self.inner.provides.lock().await.remove(module);
        self.unbind(module).await;
    }

    pub async fn provides_api(&self, api: &str) -> bool {
        self.inner
            .provides
            .lock()
            .await
            .values()
            .any(|apis| apis.iter().any(|name| name == api))
    }
}

struct InflightGuard(Arc<AtomicUsize>);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn serve_http(
    _host: ServiceHost,
    _module: String,
    service: String,
    stream: TcpStream,
    handler: Arc<dyn HostedModule>,
) {
    let io = TokioIo::new(stream);
    let service_fn = service_fn(move |req: Request<Incoming>| {
        let handler = Arc::clone(&handler);
        let service = service.clone();
        async move { Ok::<_, std::convert::Infallible>(dispatch_http(handler, service, req).await) }
    });
    let _ = http1::Builder::new().serve_connection(io, service_fn).await;
}

async fn dispatch_http(
    handler: Arc<dyn HostedModule>,
    service: String,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let method = req.method().to_string();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());
    let headers = req
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|text| (name.as_str().to_string(), text.to_string()))
        })
        .collect();
    let limited = Limited::new(req.into_body(), DEFAULT_MAX_REQUEST_SIZE);
    let body = match limited.collect().await {
        Ok(collected) => collected.to_bytes().to_vec(),
        Err(_) => {
            return plain(
                StatusCode::PAYLOAD_TOO_LARGE,
                b"request body exceeds size limit",
            );
        }
    };
    let call = handler.http(ServiceHttpRequest {
        service,
        method,
        path_and_query,
        headers,
        body,
    });
    match tokio::time::timeout(SERVICE_INVOCATION_TIMEOUT, call).await {
        Ok(Ok(response)) => http_response(response),
        Ok(Err(err)) => plain(StatusCode::BAD_GATEWAY, err.as_bytes()),
        Err(_) => plain(StatusCode::GATEWAY_TIMEOUT, b"service timed out"),
    }
}

fn http_response(response: ServiceHttpResponse) -> Response<Full<Bytes>> {
    let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut builder = Response::builder().status(status);
    for (name, value) in response.headers {
        builder = builder.header(name, value);
    }
    builder
        .body(Full::new(Bytes::from(response.body)))
        .unwrap_or_else(|_| plain(StatusCode::INTERNAL_SERVER_ERROR, b"bad response"))
}

fn plain(status: StatusCode, body: &[u8]) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::copy_from_slice(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

async fn serve_stream(
    host: ServiceHost,
    module: String,
    peer: SocketAddr,
    stream: TcpStream,
    handler: Arc<dyn HostedModule>,
    mut shutdown: watch::Receiver<bool>,
) {
    let conn_id = host.inner.next_conn.fetch_add(1, Ordering::SeqCst);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (close_tx, mut close_rx) = watch::channel(false);
    host.inner.conns.lock().await.insert(
        conn_id,
        LiveConn {
            module: module.clone(),
            tx,
            close: close_tx,
        },
    );
    if handler
        .stream(StreamEvent::Open {
            conn_id,
            peer: peer.to_string(),
        })
        .await
        .is_err()
    {
        host.inner.conns.lock().await.remove(&conn_id);
        return;
    }
    let (mut read_half, mut write_half) = stream.into_split();
    let mut buf = vec![0u8; DEFAULT_MAX_REQUEST_SIZE];
    let mut seq = 0u64;
    loop {
        if *shutdown.borrow() || *close_rx.borrow() {
            break;
        }
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            changed = close_rx.changed() => {
                if changed.is_err() || *close_rx.borrow() {
                    break;
                }
            }
            outgoing = rx.recv() => {
                match outgoing {
                    Some(bytes) => {
                        if write_half.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            incoming = tokio::time::timeout(
                SERVICE_INVOCATION_TIMEOUT,
                read_half.read(&mut buf),
            ) => {
                match incoming {
                    Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                    Ok(Ok(n)) => {
                        seq = seq.saturating_add(1);
                        match handler
                            .stream(StreamEvent::Data {
                                conn_id,
                                seq,
                                bytes: buf[..n].to_vec(),
                            })
                            .await
                        {
                            Ok(echo) if !echo.is_empty() => {
                                if write_half.write_all(&echo).await.is_err() {
                                    break;
                                }
                            }
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                }
            }
        }
    }
    host.inner.conns.lock().await.remove(&conn_id);
    let _ = handler.stream(StreamEvent::Close { conn_id }).await;
}

/// Node → module adapter. One connection is read by one task, and each
/// `invoke_hosted` send on the unbounded channel happens only after the
/// previous frame's result arrives, so frames of that connection stay ordered.
pub struct IpcHostedModule {
    pub handle: crate::module::ipc::server::ModuleIpcHandle,
    pub module_id: String,
}

#[async_trait]
impl HostedModule for IpcHostedModule {
    async fn http(&self, req: ServiceHttpRequest) -> Result<ServiceHttpResponse, String> {
        use crate::module::ipc::protocol::{InvocationResultPayload, InvocationType};
        let result = self
            .handle
            .invoke_hosted(
                &self.module_id,
                InvocationType::ServiceHttp {
                    service: req.service,
                    method: req.method,
                    path_and_query: req.path_and_query,
                    headers: req.headers,
                    body: req.body,
                },
                SERVICE_INVOCATION_TIMEOUT,
            )
            .await
            .map_err(|err| err.to_string())?;
        if !result.success {
            return Err(result
                .error
                .unwrap_or_else(|| "service http failed".to_string()));
        }
        match result.payload {
            Some(InvocationResultPayload::ServiceHttp {
                status,
                headers,
                body,
            }) => Ok(ServiceHttpResponse {
                status,
                headers,
                body,
            }),
            _ => Err("module returned no service http result".to_string()),
        }
    }

    async fn stream(&self, event: StreamEvent) -> Result<Vec<u8>, String> {
        use crate::module::ipc::protocol::InvocationType;
        let invocation = match event {
            StreamEvent::Open { conn_id, peer } => InvocationType::StreamOpen { conn_id, peer },
            StreamEvent::Data {
                conn_id,
                seq,
                bytes,
            } => InvocationType::StreamData {
                conn_id,
                seq,
                bytes,
            },
            StreamEvent::Close { conn_id } => InvocationType::StreamClose { conn_id },
        };
        let result = self
            .handle
            .invoke_hosted(&self.module_id, invocation, SERVICE_INVOCATION_TIMEOUT)
            .await
            .map_err(|err| err.to_string())?;
        if !result.success {
            return Err(result
                .error
                .unwrap_or_else(|| "stream invocation failed".to_string()));
        }
        match result.payload {
            Some(crate::module::ipc::protocol::InvocationResultPayload::Stream(bytes)) => Ok(bytes),
            _ => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::ipc::protocol::{
        InvocationMessage, InvocationResultMessage, InvocationResultPayload, InvocationType,
        ModuleMessage,
    };
    use crate::module::ipc::server::ModuleIpcHandle;
    use std::sync::atomic::AtomicU64;

    /// `ModuleIpcHandle::outgoing_tx_by_module` is an unbounded channel.
    /// Sends from one task stay in order while another task interleaves.
    #[tokio::test]
    async fn one_task_keeps_stream_frames_in_order() {
        let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
        let tx_noise = tx.clone();
        let noise = tokio::spawn(async move {
            for i in 0..40u64 {
                let msg = ModuleMessage::Invocation(InvocationMessage {
                    correlation_id: 10_000 + i,
                    invocation_type: InvocationType::StreamOpen {
                        conn_id: 2,
                        peer: "noise".to_string(),
                    },
                });
                let _ = tx_noise.send(Bytes::from(bincode::serialize(&msg).unwrap()));
                tokio::task::yield_now().await;
            }
        });
        for seq in 1..=20u64 {
            let msg = ModuleMessage::Invocation(InvocationMessage {
                correlation_id: seq,
                invocation_type: InvocationType::StreamData {
                    conn_id: 1,
                    seq,
                    bytes: vec![1],
                },
            });
            tx.send(Bytes::from(bincode::serialize(&msg).unwrap()))
                .unwrap();
        }
        drop(tx);
        noise.await.unwrap();
        let mut seqs = Vec::new();
        while let Some(bytes) = rx.recv().await {
            let ModuleMessage::Invocation(invocation) = bincode::deserialize(&bytes).unwrap()
            else {
                continue;
            };
            if let InvocationType::StreamData { seq, .. } = invocation.invocation_type {
                seqs.push(seq);
            }
        }
        assert_eq!(seqs, (1..=20).collect::<Vec<_>>());
    }

    /// A subprocess `Stream` payload is what the node writes on the socket.
    #[tokio::test]
    async fn stream_result_bytes_are_written_on_the_socket() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let pending = Arc::new(Mutex::new(HashMap::<
            u64,
            tokio::sync::oneshot::Sender<InvocationResultMessage>,
        >::new()));
        let handle = ModuleIpcHandle {
            outgoing_tx_by_module: Arc::new(tokio::sync::RwLock::new(HashMap::from([(
                "blvm-index".to_string(),
                tx,
            )]))),
            pending_invocations: Arc::clone(&pending),
            next_invocation_id: Arc::new(AtomicU64::new(1)),
            cli_registry: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        };
        let pending_reply = Arc::clone(&pending);
        let reply = tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                let Ok(ModuleMessage::Invocation(invocation)) = bincode::deserialize(&bytes) else {
                    continue;
                };
                let payload = match invocation.invocation_type {
                    InvocationType::StreamData { bytes, .. } => {
                        Some(InvocationResultPayload::Stream(bytes))
                    }
                    _ => None,
                };
                let Some(sender) = pending_reply
                    .lock()
                    .await
                    .remove(&invocation.correlation_id)
                else {
                    continue;
                };
                let _ = sender.send(InvocationResultMessage {
                    correlation_id: invocation.correlation_id,
                    success: true,
                    payload,
                    error: None,
                });
            }
        });

        let host = ServiceHost::new();
        let addrs = host
            .bind(
                "blvm-index",
                vec![BindRequest {
                    name: "electrum".to_string(),
                    kind: ServiceKind::Stream,
                    listen: "127.0.0.1:0".parse().unwrap(),
                    max_connections: DEFAULT_MAX_CONNECTIONS,
                    tls_cert: None,
                }],
                Arc::new(IpcHostedModule {
                    handle,
                    module_id: "blvm-index".to_string(),
                }),
            )
            .await
            .unwrap();
        let mut socket = TcpStream::connect(addrs[0]).await.unwrap();
        let line = b"{\"id\":1,\"method\":\"server.ping\",\"params\":[]}\n";
        socket.write_all(line).await.unwrap();
        let mut buf = vec![0u8; 128];
        let n = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut buf))
            .await
            .expect("stream reply")
            .unwrap();
        assert_eq!(&buf[..n], line);
        host.unbind("blvm-index").await;
        reply.abort();
    }
}
