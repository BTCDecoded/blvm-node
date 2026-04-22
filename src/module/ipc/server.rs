//! IPC server for node
//!
//! Server-side IPC implementation that the node uses to communicate with modules.
//! Handles incoming connections from module processes.
//! Unix: Unix domain sockets. Windows: named pipes.

use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::{debug, error, info, warn};

use async_trait::async_trait;

use crate::module::api::events::EventManager;
use crate::module::api::hub::ModuleApiHub;
use crate::module::ipc::module_ipc_length_codec;
use crate::module::ipc::protocol::{
    CliSpec, InvocationMessage, InvocationResultMessage, InvocationResultPayload, InvocationType,
    ModuleMessage, RequestMessage, RequestPayload, ResponseMessage, ResponsePayload,
};
use crate::module::traits::{module_error_msg, EventType, ModuleError, NodeAPI};
use tokio::sync::oneshot;

/// Async invoker for in-process WASM modules. Used when a module has no IPC connection.
#[cfg(feature = "wasm-modules")]
#[async_trait]
pub trait WasmInvoker: Send + Sync {
    async fn invoke_cli(
        &self,
        module_name: &str,
        subcommand: &str,
        args: Vec<String>,
    ) -> Result<InvocationResultPayload, ModuleError>;

    async fn invoke_rpc(
        &self,
        module_name: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ModuleError>;
}

/// IPC server that handles module connections
pub struct ModuleIpcServer {
    /// Socket/pipe path where server listens
    socket_path: PathBuf,
    /// Connection count (for fallback module IDs)
    connection_count: std::sync::atomic::AtomicUsize,
    /// Event manager for publishing events
    event_manager: Option<Arc<crate::module::api::events::EventManager>>,
    /// API hub for request routing
    api_hub: Option<Arc<tokio::sync::Mutex<ModuleApiHub>>>,
    /// RPC request channels (module_id -> channel) for RPC endpoint registration
    /// Channel: (correlation_id, method, params, response_tx)
    rpc_channels: Arc<
        tokio::sync::RwLock<
            HashMap<
                String,
                mpsc::UnboundedSender<(
                    u64,
                    String,
                    serde_json::Value,
                    mpsc::UnboundedSender<Result<serde_json::Value, crate::rpc::errors::RpcError>>,
                )>,
            >,
        >,
    >,
    /// CLI specs (module_id -> spec) registered by modules on connect
    cli_registry: Arc<tokio::sync::RwLock<HashMap<String, CliSpec>>>,
    /// Outgoing channel per module (for invoke_cli; module_id -> tx)
    outgoing_tx_by_module:
        Arc<tokio::sync::RwLock<HashMap<String, mpsc::UnboundedSender<bytes::Bytes>>>>,
    /// Pending CLI/RPC invocations (correlation_id -> response sender)
    pending_invocations:
        Arc<tokio::sync::Mutex<HashMap<u64, oneshot::Sender<InvocationResultMessage>>>>,
    /// Pending RPC responses from IpcRpcHandler (correlation_id -> response sender)
    rpc_pending: Arc<
        tokio::sync::Mutex<
            HashMap<
                u64,
                mpsc::UnboundedSender<Result<serde_json::Value, crate::rpc::errors::RpcError>>,
            >,
        >,
    >,
    /// Next correlation ID for invocations
    next_invocation_id: Arc<AtomicU64>,
    /// In-process WASM invoker (when module has no IPC connection)
    #[cfg(feature = "wasm-modules")]
    wasm_invoker: Option<Arc<dyn WasmInvoker>>,
}

/// Active connection to a module (generic over stream read half)
struct ModuleConnection<R: tokio::io::AsyncRead> {
    /// Module ID
    module_id: String,
    /// Framed reader for receiving messages
    reader: FramedRead<R, LengthDelimitedCodec>,
    /// Channel for sending outgoing messages (responses and events)
    outgoing_tx: Option<mpsc::UnboundedSender<bytes::Bytes>>,
    /// Event subscriptions for this module
    subscriptions: Vec<EventType>,
    /// Event channel sender for this module (used by EventManager)
    event_tx: Option<mpsc::Sender<ModuleMessage>>,
    /// Handle to the unified writer task
    writer_task_handle: Option<tokio::task::JoinHandle<()>>,
    /// Channel for RPC requests to this module (for module RPC endpoints)
    /// (correlation_id, method, params, response_tx)
    rpc_request_tx: Option<
        mpsc::UnboundedSender<(
            u64,
            String,
            serde_json::Value,
            mpsc::UnboundedSender<Result<serde_json::Value, crate::rpc::errors::RpcError>>,
        )>,
    >,
}

impl ModuleIpcServer {
    /// Create a new IPC server
    pub fn new<P: AsRef<Path>>(socket_path: P) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
            connection_count: std::sync::atomic::AtomicUsize::new(0),
            event_manager: None,
            api_hub: None,
            rpc_channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            cli_registry: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            outgoing_tx_by_module: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            pending_invocations: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            rpc_pending: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            next_invocation_id: Arc::new(AtomicU64::new(1)),
            #[cfg(feature = "wasm-modules")]
            wasm_invoker: None,
        }
    }

    /// Set the WASM invoker for in-process module dispatch
    #[cfg(feature = "wasm-modules")]
    pub fn with_wasm_invoker(mut self, invoker: Arc<dyn WasmInvoker>) -> Self {
        self.wasm_invoker = Some(invoker);
        self
    }

    /// Get RPC request channel for a module (for RPC endpoint registration)
    pub async fn get_rpc_channel(
        &self,
        module_id: &str,
    ) -> Option<
        mpsc::UnboundedSender<(
            u64,
            String,
            serde_json::Value,
            mpsc::UnboundedSender<Result<serde_json::Value, crate::rpc::errors::RpcError>>,
        )>,
    > {
        let channels = self.rpc_channels.read().await;
        channels.get(module_id).cloned()
    }

    /// Get all registered CLI specs (module_id -> spec)
    pub async fn get_cli_specs(&self) -> HashMap<String, CliSpec> {
        let registry = self.cli_registry.read().await;
        registry.clone()
    }

    /// Register CLI spec for a module (e.g. WASM modules that don't connect via IPC).
    pub async fn register_cli_spec(&self, module_id: String, spec: CliSpec) {
        let mut registry = self.cli_registry.write().await;
        registry.insert(module_id, spec);
    }

    /// Unregister CLI spec when a module is unloaded.
    pub async fn unregister_cli_spec(&self, module_id: &str) {
        let mut registry = self.cli_registry.write().await;
        registry.remove(module_id);
    }

    /// Unregister CLI spec by module name (for WASM and native; removes any entry with matching spec.name).
    pub async fn unregister_cli_spec_by_name(&self, module_name: &str) {
        let mut registry = self.cli_registry.write().await;
        let to_remove: Vec<String> = registry
            .iter()
            .filter(|(_, spec)| spec.name == module_name)
            .map(|(id, _)| id.clone())
            .collect();
        for id in to_remove {
            registry.remove(&id);
        }
    }

    /// Invoke a module CLI subcommand (node → module)
    ///
    /// Returns the invocation result or an error if the module is not found or does not respond.
    /// Tries IPC first for native modules; falls back to in-process WASM if configured.
    pub async fn invoke_cli(
        &self,
        module_name: &str,
        subcommand: &str,
        args: Vec<String>,
    ) -> Result<InvocationResultPayload, ModuleError> {
        // Try IPC path: find module_id by CLI spec name and get outgoing channel
        let maybe_ipc = {
            let registry = self.cli_registry.read().await;
            let module_id = registry
                .iter()
                .find(|(_, spec)| spec.name == module_name)
                .map(|(id, _)| id.clone());
            let by_module = self.outgoing_tx_by_module.read().await;
            module_id.and_then(|id| by_module.get(&id).cloned())
        };

        if let Some(outgoing_tx) = maybe_ipc {
            let correlation_id = self.next_invocation_id.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = oneshot::channel();

            {
                let mut pending = self.pending_invocations.lock().await;
                pending.insert(correlation_id, tx);
            }

            let invocation = InvocationMessage {
                correlation_id,
                invocation_type: InvocationType::Cli {
                    subcommand: subcommand.to_string(),
                    args,
                },
            };
            let bytes = bincode::serialize(&ModuleMessage::Invocation(invocation))
                .map_err(|e| ModuleError::SerializationError(e.to_string()))?;

            outgoing_tx.send(bytes::Bytes::from(bytes)).map_err(|_| {
                ModuleError::OperationError(module_error_msg::MODULE_CONNECTION_CLOSED.to_string())
            })?;

            let result = tokio::time::timeout(tokio::time::Duration::from_secs(60), rx)
                .await
                .map_err(|_| {
                    ModuleError::OperationError(
                        module_error_msg::MODULE_DID_NOT_RESPOND_CLI_60S.to_string(),
                    )
                })?
                .map_err(|_| {
                    ModuleError::OperationError(
                        module_error_msg::INVOCATION_RESPONSE_CHANNEL_CLOSED.to_string(),
                    )
                })?;

            if !result.success {
                return Err(ModuleError::Cli(
                    result.error.unwrap_or_else(|| "Unknown error".to_string()),
                ));
            }

            return result.payload.ok_or_else(|| {
                ModuleError::OperationError(
                    module_error_msg::MODULE_RETURNED_SUCCESS_BUT_NO_PAYLOAD.to_string(),
                )
            });
        }

        // Fallback: in-process WASM invocation
        #[cfg(feature = "wasm-modules")]
        if let Some(ref invoker) = self.wasm_invoker {
            return invoker.invoke_cli(module_name, subcommand, args).await;
        }

        Err(ModuleError::OperationError(format!(
            "No module with CLI name '{module_name}' is loaded"
        )))
    }

    /// Invoke a module RPC method (node → module).
    ///
    /// Finds module by CLI spec name (same as invoke_cli). Returns the JSON result or an error.
    /// Tries IPC first for native modules; falls back to in-process WASM if configured.
    pub async fn invoke_rpc(
        &self,
        module_name: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ModuleError> {
        // Try IPC path
        let maybe_ipc = {
            let registry = self.cli_registry.read().await;
            let module_id = registry
                .iter()
                .find(|(_, spec)| spec.name == module_name)
                .map(|(id, _)| id.clone());
            let by_module = self.outgoing_tx_by_module.read().await;
            module_id.and_then(|id| by_module.get(&id).cloned())
        };

        if let Some(outgoing_tx) = maybe_ipc {
            let correlation_id = self.next_invocation_id.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = oneshot::channel();

            {
                let mut pending = self.pending_invocations.lock().await;
                pending.insert(correlation_id, tx);
            }

            let invocation = InvocationMessage {
                correlation_id,
                invocation_type: InvocationType::Rpc {
                    method: method.to_string(),
                    params,
                },
            };
            let bytes = bincode::serialize(&ModuleMessage::Invocation(invocation))
                .map_err(|e| ModuleError::SerializationError(e.to_string()))?;

            outgoing_tx.send(bytes::Bytes::from(bytes)).map_err(|_| {
                ModuleError::OperationError(module_error_msg::MODULE_CONNECTION_CLOSED.to_string())
            })?;

            let result = tokio::time::timeout(tokio::time::Duration::from_secs(60), rx)
                .await
                .map_err(|_| {
                    ModuleError::OperationError(
                        module_error_msg::MODULE_DID_NOT_RESPOND_RPC_60S.to_string(),
                    )
                })?
                .map_err(|_| {
                    ModuleError::OperationError(
                        module_error_msg::INVOCATION_RESPONSE_CHANNEL_CLOSED.to_string(),
                    )
                })?;

            if !result.success {
                return Err(ModuleError::OperationError(
                    result.error.unwrap_or_else(|| "Unknown error".to_string()),
                ));
            }

            match result.payload {
                Some(InvocationResultPayload::Rpc(value)) => return Ok(value),
                Some(_) => {
                    return Err(ModuleError::OperationError(
                        module_error_msg::MODULE_RETURNED_WRONG_PAYLOAD_TYPE_RPC.to_string(),
                    ));
                }
                None => {
                    return Err(ModuleError::OperationError(
                        module_error_msg::MODULE_RETURNED_SUCCESS_BUT_NO_PAYLOAD.to_string(),
                    ));
                }
            }
        }

        // Fallback: in-process WASM invocation
        #[cfg(feature = "wasm-modules")]
        if let Some(ref invoker) = self.wasm_invoker {
            return invoker.invoke_rpc(module_name, method, params).await;
        }

        Err(ModuleError::OperationError(format!(
            "No module with CLI name '{module_name}' is loaded"
        )))
    }

    /// Set event manager for publishing events
    pub fn with_event_manager(mut self, event_manager: Arc<EventManager>) -> Self {
        self.event_manager = Some(event_manager);
        self
    }

    /// Set API hub for request routing
    pub fn with_api_hub(mut self, api_hub: Arc<tokio::sync::Mutex<ModuleApiHub>>) -> Self {
        self.api_hub = Some(api_hub);
        self
    }

    /// Start listening for module connections
    pub async fn start<A: NodeAPI + Send + Sync + 'static>(
        &mut self,
        node_api: Arc<A>,
    ) -> Result<(), ModuleError> {
        #[cfg(unix)]
        return self.start_unix(node_api).await;
        #[cfg(windows)]
        return self.start_windows(node_api).await;
    }

    #[cfg(unix)]
    async fn start_unix<A: NodeAPI + Send + Sync + 'static>(
        &mut self,
        node_api: Arc<A>,
    ) -> Result<(), ModuleError> {
        use tokio::net::{UnixListener, UnixStream};

        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)
                .map_err(|e| ModuleError::IpcError(format!("Failed to remove old socket: {e}")))?;
        }
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ModuleError::IpcError(format!("Failed to create socket directory: {e}"))
            })?;
        }
        let listener = UnixListener::bind(&self.socket_path)
            .map_err(|e| ModuleError::IpcError(format!("Failed to bind socket: {e}")))?;
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            if let Err(e) = std::fs::set_permissions(&self.socket_path, perms) {
                warn!("Failed to set IPC socket permissions: {}", e);
            }
        }
        info!("Module IPC server listening on {:?}", self.socket_path);
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    debug!("New module connection");
                    self.handle_connection(stream, Arc::clone(&node_api))
                        .await?;
                }
                Err(e) => error!("Failed to accept module connection: {}", e),
            }
        }
    }

    #[cfg(windows)]
    async fn start_windows<A: NodeAPI + Send + Sync + 'static>(
        &mut self,
        node_api: Arc<A>,
    ) -> Result<(), ModuleError> {
        use tokio::net::windows::named_pipe::ServerOptions;

        let pipe_name = path_to_pipe_name(&self.socket_path);
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ModuleError::IpcError(format!("Failed to create socket directory: {e}"))
            })?;
        }
        info!("Module IPC server listening on pipe {}", pipe_name);
        loop {
            let server = ServerOptions::new()
                .first_pipe_instance(false)
                .create(&pipe_name)
                .map_err(|e| ModuleError::IpcError(format!("Failed to create named pipe: {e}")))?;
            server
                .connect()
                .await
                .map_err(|e| ModuleError::IpcError(format!("Failed to connect pipe: {e}")))?;
            debug!("New module connection (named pipe)");
            self.handle_connection(server, Arc::clone(&node_api))
                .await?;
        }
    }

    /// Handle a new module connection (generic over stream type)
    async fn handle_connection<S, A>(
        &mut self,
        stream: S,
        node_api: Arc<A>,
    ) -> Result<(), ModuleError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        A: NodeAPI + Send + Sync,
    {
        let (read_half, write_half) = tokio::io::split(stream);
        let mut reader = FramedRead::new(read_half, module_ipc_length_codec());
        let mut writer = FramedWrite::new(write_half, module_ipc_length_codec());

        // Wait for handshake message from module
        let module_id = match reader.next().await {
            Some(Ok(bytes)) => {
                let message: ModuleMessage = bincode::deserialize(bytes.as_ref())
                    .map_err(|e| ModuleError::SerializationError(e.to_string()))?;

                match message {
                    ModuleMessage::Request(request) => {
                        if let RequestPayload::Handshake {
                            module_id,
                            module_name,
                            version,
                        } = request.payload
                        {
                            info!(
                                "Module handshake: id={}, name={}, version={}",
                                module_id, module_name, version
                            );

                            // Send handshake acknowledgment
                            let ack = ResponseMessage {
                                correlation_id: request.correlation_id,
                                success: true,
                                payload: Some(ResponsePayload::HandshakeAck {
                                    node_version: env!("CARGO_PKG_VERSION").to_string(),
                                }),
                                error: None,
                            };

                            let ack_bytes = bincode::serialize(&ModuleMessage::Response(ack))
                                .map_err(|e| ModuleError::SerializationError(e.to_string()))?;
                            writer
                                .send(bytes::Bytes::from(ack_bytes))
                                .await
                                .map_err(|e| {
                                    ModuleError::IpcError(format!(
                                        "Failed to send handshake ack: {e}"
                                    ))
                                })?;

                            module_id
                        } else {
                            // No handshake - use fallback ID (backward compatibility)
                            warn!("Module did not send handshake, using fallback ID");
                            let timestamp = crate::utils::current_timestamp_nanos();
                            let count = self.connection_count.fetch_add(1, Ordering::SeqCst);
                            format!("module_{count}_{timestamp}")
                        }
                    }
                    _ => {
                        return Err(ModuleError::IpcError(
                            "First message must be a handshake request".to_string(),
                        ));
                    }
                }
            }
            Some(Err(e)) => {
                return Err(ModuleError::IpcError(format!(
                    "Failed to read handshake: {e}"
                )));
            }
            None => {
                return Err(ModuleError::IpcError(
                    "Connection closed before handshake".to_string(),
                ));
            }
        };

        // Create unified outgoing message channel (for both responses and events)
        // This allows us to share the writer between response handler and event handler
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<bytes::Bytes>();

        // Create RPC request channel (for sending RPC requests from node to module)
        // (correlation_id, method, params, response_tx)
        type RpcResponseSender = mpsc::UnboundedSender<
            std::result::Result<serde_json::Value, crate::rpc::errors::RpcError>,
        >;
        let (rpc_request_tx, mut rpc_request_rx) =
            mpsc::unbounded_channel::<(u64, String, serde_json::Value, RpcResponseSender)>();

        // Create event channel for this module (events from EventManager go here)
        let (event_tx, mut event_rx) = mpsc::channel(100);

        // Clone outgoing_tx before moving it into the task
        let outgoing_tx_for_events = outgoing_tx.clone();

        // Spawn unified writer task that handles both responses and events
        let module_id_writer_task = module_id.clone();
        let event_manager_clone = self.event_manager.clone();
        let writer_task_handle = tokio::spawn(async move {
            // Forward events from event_rx to outgoing_tx
            let module_id_event_fwd = module_id_writer_task.clone();
            tokio::spawn(async move {
                while let Some(event_message) = event_rx.recv().await {
                    match bincode::serialize(&event_message) {
                        Ok(bytes) => {
                            if outgoing_tx_for_events
                                .send(bytes::Bytes::from(bytes))
                                .is_err()
                            {
                                break; // Receiver dropped, connection closed
                            }
                        }
                        Err(e) => {
                            warn!(
                                "Failed to serialize event for module {}: {}",
                                module_id_event_fwd, e
                            );
                        }
                    }
                }

                // Clean up: unsubscribe module from events when task exits
                if let Some(event_mgr) = event_manager_clone {
                    if let Err(e) = event_mgr.unsubscribe_module(&module_id_event_fwd).await {
                        warn!(
                            "Failed to unsubscribe module {} from events: {}",
                            module_id_event_fwd, e
                        );
                    }
                }
            });

            // Main writer loop: send all outgoing messages (responses + events) via IPC
            while let Some(bytes) = outgoing_rx.recv().await {
                if let Err(e) = writer.send(bytes).await {
                    warn!(
                        "Failed to send message to module {}: {}",
                        module_id_writer_task, e
                    );
                    break;
                }
            }
        });

        // Initialize filesystem and storage access for this module
        // Extract module name from module_id (format: {module_name}_{uuid})
        let module_name = module_id
            .split('_')
            .next()
            .unwrap_or(&module_id)
            .to_string();

        // Get base data directory - extract from module_id or use default
        // Module ID format: {module_name}_{uuid}
        // We'll derive the base directory from the module name
        // The actual base directory should be passed from ModuleManager, but for now
        // we'll use a reasonable default based on common patterns
        let base_data_dir = std::path::PathBuf::from("data/modules");
        let module_data_dir = base_data_dir.join(&module_name);

        // Ensure module data directory exists
        if let Err(e) = std::fs::create_dir_all(&module_data_dir) {
            warn!(
                "Failed to create module data directory {:?}: {}",
                module_data_dir, e
            );
        }

        // Initialize module filesystem and storage access
        if let Err(e) = node_api
            .initialize_module(module_id.clone(), module_data_dir, base_data_dir)
            .await
        {
            warn!(
                "Failed to initialize module {} filesystem/storage: {}",
                module_id, e
            );
            // Continue anyway - module can still use other APIs
        }

        // Register for invoke_cli and RPC dispatch (before moving outgoing_tx)
        {
            let mut by_module = self.outgoing_tx_by_module.write().await;
            by_module.insert(module_id.clone(), outgoing_tx.clone());
        }
        {
            let mut channels = self.rpc_channels.write().await;
            channels.insert(module_id.clone(), rpc_request_tx.clone());
        }

        // Spawn task to forward RPC requests from IpcRpcHandler to module via Invocation
        let outgoing_tx_for_rpc = outgoing_tx.clone();
        let rpc_pending = Arc::clone(&self.rpc_pending);
        tokio::spawn(async move {
            while let Some((correlation_id, method, params, response_tx)) =
                rpc_request_rx.recv().await
            {
                rpc_pending.lock().await.insert(correlation_id, response_tx);

                let invocation = InvocationMessage {
                    correlation_id,
                    invocation_type: InvocationType::Rpc { method, params },
                };
                if let Ok(bytes) = bincode::serialize(&ModuleMessage::Invocation(invocation)) {
                    if outgoing_tx_for_rpc.send(bytes::Bytes::from(bytes)).is_err() {
                        break; // Connection closed
                    }
                }
            }
        });

        let mut connection = ModuleConnection {
            module_id: module_id.clone(),
            reader,
            outgoing_tx: Some(outgoing_tx),
            subscriptions: Vec::new(),
            event_tx: Some(event_tx),
            writer_task_handle: Some(writer_task_handle),
            rpc_request_tx: Some(rpc_request_tx),
        };

        // Process messages from this module
        while let Some(result) = connection.reader.next().await {
            match result {
                Ok(bytes) => {
                    let node_api_clone = Arc::clone(&node_api);
                    match self
                        .handle_message(bytes.as_ref(), &mut connection, node_api_clone)
                        .await
                    {
                        Ok(()) => {}
                        Err(e) => {
                            error!("Error handling message: {}", e);
                            break;
                        }
                    }
                }
                Err(e) => {
                    error!("Error reading from module {}: {}", module_id, e);
                    break;
                }
            }
        }

        info!("Module {} disconnected", module_id);

        // Clean up: remove from shared maps
        {
            let mut by_module = self.outgoing_tx_by_module.write().await;
            by_module.remove(&module_id);
        }
        {
            let mut channels = self.rpc_channels.write().await;
            channels.remove(&module_id);
        }
        {
            let mut registry = self.cli_registry.write().await;
            registry.remove(&module_id);
        }

        // Close outgoing channel (will cause writer task to exit)
        drop(connection.outgoing_tx);

        // Abort writer task (which includes event forwarding)
        if let Some(handle) = connection.writer_task_handle.take() {
            handle.abort();
        }

        // Unsubscribe from event manager
        if let Some(event_mgr) = &self.event_manager {
            if let Err(e) = event_mgr.unsubscribe_module(&module_id).await {
                warn!(
                    "Failed to unsubscribe module {} from events: {}",
                    module_id, e
                );
            }
        }

        Ok(())
    }

    /// Handle a message from a module
    async fn handle_message<R: tokio::io::AsyncRead, A: NodeAPI + Send + Sync>(
        &mut self,
        bytes: &[u8],
        connection: &mut ModuleConnection<R>,
        node_api: Arc<A>,
    ) -> Result<(), ModuleError> {
        let message: ModuleMessage = bincode::deserialize(bytes)
            .map_err(|e| ModuleError::SerializationError(e.to_string()))?;

        match message {
            ModuleMessage::Request(request) => {
                // Handle RegisterCliSpec: store in registry, respond, don't pass to hub
                if let RequestPayload::RegisterCliSpec { spec } = &request.payload {
                    let module_id = connection.module_id.clone();
                    let mut registry = self.cli_registry.write().await;
                    registry.insert(module_id.clone(), spec.clone());
                    debug!("Module {} registered CLI spec: {}", module_id, spec.name);
                    let response = ResponseMessage::success(
                        request.correlation_id,
                        ResponsePayload::Bool(true),
                    );
                    let response_message = ModuleMessage::Response(response);
                    let response_bytes = bincode::serialize(&response_message)
                        .map_err(|e| ModuleError::SerializationError(e.to_string()))?;
                    if let Some(tx) = &connection.outgoing_tx {
                        tx.send(bytes::Bytes::from(response_bytes)).map_err(|e| {
                            ModuleError::IpcError(format!("Failed to send response: {e}"))
                        })?;
                    }
                    return Ok(());
                }

                // Handle SubscribeEvents specially to register with event manager
                if let RequestPayload::SubscribeEvents { ref event_types } = request.payload {
                    if let Some(event_mgr) = &self.event_manager {
                        if let Some(event_tx) = &connection.event_tx {
                            // Register module subscriptions
                            let module_id = connection.module_id.clone();
                            let event_tx_clone = event_tx.clone();
                            event_mgr
                                .subscribe_module(
                                    module_id.clone(),
                                    event_types.clone(),
                                    event_tx_clone,
                                )
                                .await?;
                            connection.subscriptions = event_types.clone();
                            debug!(
                                "Module {} subscribed to events: {:?}",
                                module_id, event_types
                            );
                            // Note: subscribe_module() handles:
                            // 1. Sending already-loaded modules to this newly subscribing module
                            // 2. Publishing ModuleLoaded for this module (if loaded) AFTER subscription
                            // This ensures ModuleLoaded only happens after startup is complete
                        }
                    }
                }

                // Use API hub if available, otherwise fall back to direct node_api
                let response = if let Some(hub) = &self.api_hub {
                    let mut hub_guard = hub.lock().await;
                    hub_guard
                        .handle_request(&connection.module_id, request.clone())
                        .await?
                } else {
                    self.process_request(&request, node_api).await?
                };
                let response_message = ModuleMessage::Response(response);

                let response_bytes = bincode::serialize(&response_message)
                    .map_err(|e| ModuleError::SerializationError(e.to_string()))?;

                // Send response through outgoing channel
                if let Some(tx) = &connection.outgoing_tx {
                    tx.send(bytes::Bytes::from(response_bytes)).map_err(|e| {
                        ModuleError::IpcError(format!("Failed to send response: {e}"))
                    })?;
                }
            }
            ModuleMessage::Response(_) => {
                warn!("Received response from module (unexpected)");
            }
            ModuleMessage::Event(_) => {
                warn!("Received event from module (unexpected)");
            }
            ModuleMessage::InvocationResult(result) => {
                // Check rpc_pending first (IpcRpcHandler path)
                {
                    let mut rpc_pending = self.rpc_pending.lock().await;
                    if let Some(response_tx) = rpc_pending.remove(&result.correlation_id) {
                        let response = if result.success {
                            result
                                .payload
                                .map(|p| {
                                    if let InvocationResultPayload::Rpc(v) = p {
                                        Ok(v)
                                    } else {
                                        Err(crate::rpc::errors::RpcError::internal_error(
                                            "Wrong payload type".to_string(),
                                        ))
                                    }
                                })
                                .unwrap_or_else(|| {
                                    Err(crate::rpc::errors::RpcError::internal_error(
                                        "No RPC payload".to_string(),
                                    ))
                                })
                        } else {
                            Err(crate::rpc::errors::RpcError::internal_error(
                                result.error.unwrap_or_else(|| "Unknown error".to_string()),
                            ))
                        };
                        let _ = response_tx.send(response);
                        return Ok(());
                    }
                }
                // Fallback: pending_invocations (invoke_cli/invoke_rpc path)
                let mut pending = self.pending_invocations.lock().await;
                if let Some(tx) = pending.remove(&result.correlation_id) {
                    let _ = tx.send(result);
                }
                return Ok(());
            }
            ModuleMessage::Invocation(_) => {
                warn!("Received Invocation from module (unexpected; node sends to module)");
                return Ok(());
            }
            ModuleMessage::Log(log_msg) => {
                // Forward log message to node's logging system
                use crate::module::ipc::protocol::LogLevel;
                let module_id_str = log_msg.module_id.clone();
                let message_str = log_msg.message.clone();
                // Use tracing macros without target parameter (tracing will use default target)
                // The module_id is included in the log message for identification
                match log_msg.level {
                    LogLevel::Trace => {
                        tracing::trace!(
                            module_id = %module_id_str,
                            "{}",
                            message_str
                        );
                    }
                    LogLevel::Debug => {
                        tracing::debug!(
                            module_id = %module_id_str,
                            "{}",
                            message_str
                        );
                    }
                    LogLevel::Info => {
                        tracing::info!(
                            module_id = %module_id_str,
                            "{}",
                            message_str
                        );
                    }
                    LogLevel::Warn => {
                        tracing::warn!(
                            module_id = %module_id_str,
                            "{}",
                            message_str
                        );
                    }
                    LogLevel::Error => {
                        tracing::error!(
                            module_id = %module_id_str,
                            "{}",
                            message_str
                        );
                    }
                }
                // Log messages don't require a response
                return Ok(());
            }
        }

        Ok(())
    }

    /// Process a request from a module
    async fn process_request<A: NodeAPI + Send + Sync>(
        &self,
        request: &RequestMessage,
        node_api: Arc<A>,
    ) -> Result<ResponseMessage, ModuleError> {
        use crate::module::ipc::protocol::{RequestPayload, ResponsePayload};

        match &request.payload {
            RequestPayload::Handshake { .. } => {
                // Handshake is handled at connection level
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::HandshakeAck {
                        node_version: env!("CARGO_PKG_VERSION").to_string(),
                    },
                ))
            }
            RequestPayload::GetBlock { hash } => {
                let block = node_api.get_block(hash).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::Block(block),
                ))
            }
            RequestPayload::GetBlockHeader { hash } => {
                let header = node_api.get_block_header(hash).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::BlockHeader(header),
                ))
            }
            RequestPayload::GetTransaction { hash } => {
                let tx = node_api.get_transaction(hash).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::Transaction(tx),
                ))
            }
            RequestPayload::HasTransaction { hash } => {
                let exists = node_api.has_transaction(hash).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::Bool(exists),
                ))
            }
            RequestPayload::GetChainTip => {
                let tip = node_api.get_chain_tip().await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::Hash(tip),
                ))
            }
            RequestPayload::GetBlockHeight => {
                let height = node_api.get_block_height().await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::U64(height),
                ))
            }
            RequestPayload::GetUtxo { outpoint } => {
                let utxo = node_api.get_utxo(outpoint).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::Utxo(utxo),
                ))
            }
            RequestPayload::SubscribeEvents { event_types } => {
                // Register module subscriptions with event manager
                if let Some(_event_mgr) = &self.event_manager {
                    // Get module ID from connection (would need to pass it through)
                    // For now, we'll handle this in handle_message where we have connection
                    // This will be implemented properly when we integrate event manager
                    debug!("Module subscribing to events: {:?}", event_types);
                }
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::SubscribeAck,
                ))
            }
            // Mempool API
            RequestPayload::GetMempoolTransactions => {
                let txs = node_api.get_mempool_transactions().await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::MempoolTransactions(txs),
                ))
            }
            RequestPayload::GetMempoolTransaction { tx_hash } => {
                let tx = node_api.get_mempool_transaction(tx_hash).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::MempoolTransaction(tx),
                ))
            }
            RequestPayload::GetMempoolSize => {
                let size = node_api.get_mempool_size().await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::MempoolSize(size),
                ))
            }
            // Network API
            RequestPayload::GetNetworkStats => {
                let stats = node_api.get_network_stats().await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::NetworkStats(stats),
                ))
            }
            RequestPayload::GetNetworkPeers => {
                let peers = node_api.get_network_peers().await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::NetworkPeers(peers),
                ))
            }
            // Chain API
            RequestPayload::GetChainInfo => {
                let info = node_api.get_chain_info().await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::ChainInfo(info),
                ))
            }
            RequestPayload::GetBlockByHeight { height } => {
                let block = node_api.get_block_by_height(*height).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::BlockByHeight(block),
                ))
            }
            // Lightning API
            RequestPayload::GetLightningNodeUrl => {
                let url = node_api.get_lightning_node_url().await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::LightningNodeUrl(url),
                ))
            }
            RequestPayload::GetLightningInfo => {
                let info = node_api.get_lightning_info().await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::LightningInfo(info),
                ))
            }
            // Payment API
            RequestPayload::GetPaymentState { payment_id } => {
                let state = node_api.get_payment_state(payment_id).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::PaymentState(state),
                ))
            }
            // Additional Mempool API
            RequestPayload::CheckTransactionInMempool { tx_hash } => {
                let exists = node_api.check_transaction_in_mempool(tx_hash).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::CheckTransactionInMempool(exists),
                ))
            }
            RequestPayload::GetFeeEstimate { target_blocks } => {
                let fee_rate = node_api.get_fee_estimate(*target_blocks).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::FeeEstimate(fee_rate),
                ))
            }
            // Filesystem API
            RequestPayload::ReadFile { path } => {
                let data = node_api.read_file(path.clone()).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::FileData(data),
                ))
            }
            RequestPayload::WriteFile { path, data } => {
                node_api.write_file(path.clone(), data.clone()).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::Bool(true),
                ))
            }
            RequestPayload::DeleteFile { path } => {
                node_api.delete_file(path.clone()).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::Bool(true),
                ))
            }
            RequestPayload::ListDirectory { path } => {
                let entries = node_api.list_directory(path.clone()).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::DirectoryListing(entries),
                ))
            }
            RequestPayload::CreateDirectory { path } => {
                node_api.create_directory(path.clone()).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::Bool(true),
                ))
            }
            RequestPayload::GetFileMetadata { path } => {
                let metadata = node_api.get_file_metadata(path.clone()).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::FileMetadata(metadata),
                ))
            }
            // Module RPC Endpoint Registration
            RequestPayload::RegisterRpcEndpoint {
                method,
                description,
            } => {
                node_api
                    .register_rpc_endpoint(method.clone(), description.clone())
                    .await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::RpcEndpointRegistered,
                ))
            }
            RequestPayload::UnregisterRpcEndpoint { method } => {
                node_api.unregister_rpc_endpoint(method).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::RpcEndpointUnregistered,
                ))
            }
            // Timers and Scheduled Tasks
            RequestPayload::RegisterTimer {
                interval_seconds: _,
            } => {
                // Note: Timer callbacks cannot be serialized over IPC
                // For IPC-based timers, we need a different approach:
                // The module would need to send a "timer_fire" request when the timer should fire
                // For now, return an error indicating this needs a callback mechanism
                Err(ModuleError::OperationError(
                    module_error_msg::TIMER_REGISTRATION_REQUIRES_CALLBACK_IPC.to_string(),
                ))
            }
            RequestPayload::CancelTimer { timer_id: _ } => {
                // Note: Timers registered via IPC would need to be tracked differently
                // For now, return an error
                Err(ModuleError::OperationError(
                    module_error_msg::TIMER_CANCELLATION_NOT_SUPPORTED_IPC.to_string(),
                ))
            }
            RequestPayload::ScheduleTask { delay_seconds: _ } => {
                // Note: Task callbacks cannot be serialized over IPC
                // Similar to timers, this needs a different approach
                Err(ModuleError::OperationError(
                    module_error_msg::TASK_SCHEDULING_REQUIRES_CALLBACK_IPC.to_string(),
                ))
            }
            // Metrics and Telemetry
            RequestPayload::ReportMetric { metric } => {
                node_api.report_metric(metric.clone()).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::MetricReported,
                ))
            }
            RequestPayload::GetModuleMetrics { module_id } => {
                let metrics = node_api.get_module_metrics(module_id).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::ModuleMetrics(metrics),
                ))
            }
            // Network Integration
            RequestPayload::SendMeshPacketToPeer {
                peer_addr,
                packet_data,
            } => {
                node_api
                    .send_mesh_packet_to_peer(peer_addr.clone(), packet_data.clone())
                    .await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::Bool(true),
                ))
            }
            RequestPayload::SendStratumV2MessageToPeer {
                peer_addr,
                message_data,
            } => {
                node_api
                    .send_stratum_v2_message_to_peer(peer_addr.clone(), message_data.clone())
                    .await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::Bool(true),
                ))
            }
            // Mining API
            RequestPayload::GetBlockTemplate {
                rules,
                coinbase_script,
                coinbase_address,
            } => {
                let template = node_api
                    .get_block_template(
                        rules.clone(),
                        coinbase_script.clone(),
                        coinbase_address.clone(),
                    )
                    .await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::BlockTemplate(template),
                ))
            }
            RequestPayload::SubmitBlock { block } => {
                let result = node_api.submit_block(block.clone()).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::SubmitBlockResult(result),
                ))
            }
            RequestPayload::MergeBlockServeDenylist { block_hashes } => {
                node_api
                    .merge_block_serve_denylist(block_hashes.as_slice())
                    .await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::BlockServeDenylistMerged,
                ))
            }
            RequestPayload::GetBlockServeDenylistSnapshot => {
                let s = node_api.get_block_serve_denylist_snapshot().await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::BlockServeDenylistSnapshot(s),
                ))
            }
            RequestPayload::ClearBlockServeDenylist => {
                node_api.clear_block_serve_denylist().await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::Bool(true),
                ))
            }
            RequestPayload::ReplaceBlockServeDenylist { block_hashes } => {
                node_api
                    .replace_block_serve_denylist(block_hashes.as_slice())
                    .await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::Bool(true),
                ))
            }
            RequestPayload::MergeTxServeDenylist { tx_hashes } => {
                node_api
                    .merge_tx_serve_denylist(tx_hashes.as_slice())
                    .await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::TxServeDenylistMerged,
                ))
            }
            RequestPayload::GetTxServeDenylistSnapshot => {
                let s = node_api.get_tx_serve_denylist_snapshot().await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::TxServeDenylistSnapshot(s),
                ))
            }
            RequestPayload::ClearTxServeDenylist => {
                node_api.clear_tx_serve_denylist().await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::Bool(true),
                ))
            }
            RequestPayload::ReplaceTxServeDenylist { tx_hashes } => {
                node_api
                    .replace_tx_serve_denylist(tx_hashes.as_slice())
                    .await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::Bool(true),
                ))
            }
            RequestPayload::GetSyncStatus => {
                let s = node_api.get_sync_status().await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::NodeSyncStatus(s),
                ))
            }
            RequestPayload::BanPeer {
                peer_addr,
                ban_duration_seconds,
            } => {
                node_api
                    .ban_peer(peer_addr.as_str(), *ban_duration_seconds)
                    .await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::Bool(true),
                ))
            }
            RequestPayload::SetBlockServeMaintenanceMode { enabled } => {
                node_api.set_block_serve_maintenance_mode(*enabled).await?;
                Ok(ResponseMessage::success(
                    request.correlation_id,
                    ResponsePayload::Bool(true),
                ))
            }
            _ => Ok(ResponseMessage::error(
                request.correlation_id,
                format!("Unimplemented request payload: {:?}", request.payload),
            )),
        }
    }
}

/// Derive Windows named pipe name from socket path (e.g. "hello.sock" -> "\\.\pipe\blvm-hello")
#[cfg(windows)]
fn path_to_pipe_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("blvm-module");
    let safe = stem.replace(|c: char| !c.is_alphanumeric(), "-");
    format!(r"\\.\pipe\blvm-{}", safe)
}
