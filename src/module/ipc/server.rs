//! IPC server for node
//!
//! Server-side IPC implementation that the node uses to communicate with modules.
//! Handles incoming connections from module processes.

use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::{debug, error, info, warn};

use crate::module::api::events::EventManager;
use crate::module::api::hub::ModuleApiHub;
use crate::module::ipc::protocol::{
    ModuleMessage, RequestMessage, RequestPayload, ResponseMessage, ResponsePayload,
};
use crate::module::traits::{EventType, ModuleError, NodeAPI};

/// IPC server that handles module connections
pub struct ModuleIpcServer {
    /// Socket path where server listens
    socket_path: PathBuf,
    /// Active connections from modules
    connections: HashMap<String, ModuleConnection>,
    /// Event manager for publishing events
    event_manager: Option<Arc<crate::module::api::events::EventManager>>,
    /// API hub for request routing
    api_hub: Option<Arc<tokio::sync::Mutex<ModuleApiHub>>>,
}

/// Active connection to a module
struct ModuleConnection {
    /// Module ID
    module_id: String,
    /// Framed reader for receiving messages
    reader: FramedRead<tokio::io::ReadHalf<UnixStream>, LengthDelimitedCodec>,
    /// Channel for sending outgoing messages (responses and events)
    outgoing_tx: Option<mpsc::UnboundedSender<bytes::Bytes>>,
    /// Event subscriptions for this module
    subscriptions: Vec<EventType>,
    /// Event channel sender for this module (used by EventManager)
    event_tx: Option<mpsc::Sender<ModuleMessage>>,
    /// Handle to the unified writer task
    writer_task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl ModuleIpcServer {
    /// Create a new IPC server
    pub fn new<P: AsRef<Path>>(socket_path: P) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
            connections: HashMap::new(),
            event_manager: None,
            api_hub: None,
        }
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
        // Remove existing socket file if it exists
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path).map_err(|e| {
                ModuleError::IpcError(format!("Failed to remove old socket: {}", e))
            })?;
        }

        // Create parent directory if needed
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ModuleError::IpcError(format!("Failed to create socket directory: {}", e))
            })?;
        }

        let listener = UnixListener::bind(&self.socket_path)
            .map_err(|e| ModuleError::IpcError(format!("Failed to bind socket: {}", e)))?;

        info!("Module IPC server listening on {:?}", self.socket_path);

        // Accept connections
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    debug!("New module connection");
                    let node_api_clone = Arc::clone(&node_api);
                    self.handle_connection(stream, node_api_clone).await?;
                }
                Err(e) => {
                    error!("Failed to accept module connection: {}", e);
                }
            }
        }
    }

    /// Handle a new module connection
    async fn handle_connection<A: NodeAPI + Send + Sync>(
        &mut self,
        stream: UnixStream,
        node_api: Arc<A>,
    ) -> Result<(), ModuleError> {
        let (read_half, write_half) = tokio::io::split(stream);
        let mut reader = FramedRead::new(read_half, LengthDelimitedCodec::new());
        let mut writer = FramedWrite::new(write_half, LengthDelimitedCodec::new());

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
                                        "Failed to send handshake ack: {}",
                                        e
                                    ))
                                })?;

                            module_id
                        } else {
                            // No handshake - use fallback ID (backward compatibility)
                            warn!("Module did not send handshake, using fallback ID");
                            let timestamp = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_nanos();
                            let connection_count = self.connections.len();
                            format!("module_{}_{}", connection_count, timestamp)
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
                    "Failed to read handshake: {}",
                    e
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

        let mut connection = ModuleConnection {
            module_id: module_id.clone(),
            reader,
            outgoing_tx: Some(outgoing_tx),
            subscriptions: Vec::new(),
            event_tx: Some(event_tx),
            writer_task_handle: Some(writer_task_handle),
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

        // Clean up connection: abort tasks and unsubscribe from events
        if let Some(mut conn) = self.connections.remove(&module_id) {
            // Close outgoing channel (will cause writer task to exit)
            drop(conn.outgoing_tx);

            // Abort writer task (which includes event forwarding)
            if let Some(handle) = conn.writer_task_handle.take() {
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
        }

        Ok(())
    }

    /// Handle a message from a module
    async fn handle_message<A: NodeAPI + Send + Sync>(
        &mut self,
        bytes: &[u8],
        connection: &mut ModuleConnection,
        node_api: Arc<A>,
    ) -> Result<(), ModuleError> {
        let message: ModuleMessage = bincode::deserialize(bytes)
            .map_err(|e| ModuleError::SerializationError(e.to_string()))?;

        match message {
            ModuleMessage::Request(request) => {
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
                        ModuleError::IpcError(format!("Failed to send response: {}", e))
                    })?;
                }
            }
            ModuleMessage::Response(_) => {
                warn!("Received response from module (unexpected)");
            }
            ModuleMessage::Event(_) => {
                warn!("Received event from module (unexpected)");
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
        }
    }
}
