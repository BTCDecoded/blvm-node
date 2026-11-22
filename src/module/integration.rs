//! Module integration helper
//!
//! Provides a simple, unified API for modules to integrate with the node.
//! Handles connection, handshake, and provides NodeAPI + event receiver.

use crate::module::api::NodeApiIpc;
use crate::module::ipc::ModuleIpcClient;
use crate::module::ipc::protocol::{
    CliSpec, InvocationMessage, InvocationResultMessage, MessageType, ModuleMessage,
    RequestMessage, RequestPayload, ResponsePayload,
};
use crate::module::ipc::session::{SessionRequest, drive_session};
use crate::module::traits::{EventType, ModuleError, NodeAPI};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

/// Simple helper for module integration
/// Handles connection, handshake, and provides NodeAPI + event receiver + invocation handling
pub struct ModuleIntegration {
    module_id: String,
    node_api: Arc<dyn NodeAPI>,
    event_receiver: tokio::sync::broadcast::Receiver<ModuleMessage>,
    /// Broadcast sender for fan-out to `event_receiver()` subscribers.
    event_broadcast: Arc<tokio::sync::broadcast::Sender<ModuleMessage>>,
    /// In-process modules forward NodeAPI subscription mpsc into the broadcast channel.
    forward_events_from_subscribe: bool,
    /// Receiver for CLI/RPC invocations from the node (module handles and sends result)
    invocation_receiver:
        Option<mpsc::Receiver<(InvocationMessage, oneshot::Sender<InvocationResultMessage>)>>,
}

impl ModuleIntegration {
    /// Connect to node via IPC (for out-of-process modules)
    ///
    /// If `cli_spec` is provided, sends it to the node after handshake so the node
    /// can expose the module's CLI commands via getmoduleclispecs/runmodulecli.
    pub async fn connect(
        socket_path: PathBuf,
        module_id: String,
        module_name: String,
        version: String,
        cli_spec: Option<CliSpec>,
    ) -> Result<Self, ModuleError> {
        use tracing::{error, info, warn};

        info!(
            "Connecting to node IPC socket: {:?} (module: {})",
            socket_path, module_name
        );

        // Connect to IPC
        let mut ipc_client = ModuleIpcClient::connect(&socket_path).await?;
        info!("Connected to IPC socket");

        // The node sets MODULE_IPC_PROTOCOL from the manifest. Published binaries
        // ignore the variable and still send Handshake. A rebuilt module sends
        // HandshakeV2 only when the manifest says generation 2.
        let correlation_id = ipc_client.next_correlation_id();
        let strict = std::env::var("MODULE_IPC_PROTOCOL").ok().as_deref() == Some("2");
        let request = if strict {
            RequestMessage {
                correlation_id,
                request_type: MessageType::HandshakeV2,
                payload: RequestPayload::HandshakeV2 {
                    module_id: module_id.clone(),
                    module_name,
                    version,
                    ipc_protocol: 2,
                },
            }
        } else {
            RequestMessage {
                correlation_id,
                request_type: MessageType::Handshake,
                payload: RequestPayload::Handshake {
                    module_id: module_id.clone(),
                    module_name,
                    version,
                },
            }
        };

        let response = ipc_client.request(request).await?;
        match response.payload {
            Some(ResponsePayload::HandshakeAck { node_version }) => {
                info!("Handshake successful! Node version: {}", node_version);
            }
            _ => {
                return Err(ModuleError::IpcError(
                    "Invalid handshake response".to_string(),
                ));
            }
        }

        // Register CLI spec if provided (for dynamic CLI via getmoduleclispecs)
        if let Some(spec) = cli_spec {
            let correlation_id = ipc_client.next_correlation_id();
            let request = RequestMessage {
                correlation_id,
                request_type: MessageType::RegisterCliSpec,
                payload: RequestPayload::RegisterCliSpec { spec },
            };
            let response = ipc_client.request(request).await?;
            if !response.success {
                return Err(ModuleError::IpcError(
                    response
                        .error
                        .unwrap_or_else(|| "Failed to register CLI spec".to_string()),
                ));
            }
            info!("CLI spec registered with node");
        }

        // Use broadcast channel for events (allows multiple receivers)
        let (broadcast_tx, broadcast_rx) = tokio::sync::broadcast::channel(1000);
        let broadcast_tx_arc = Arc::new(broadcast_tx);
        let (request_tx, request_rx) = mpsc::channel::<SessionRequest>(32);

        // Channel for invocations: spawn sends (Invocation, oneshot_tx), module receives and responds
        let (invocation_tx, invocation_rx) = mpsc::channel(32);

        let events = Arc::clone(&broadcast_tx_arc);
        tokio::spawn(async move {
            if let Err(error) = drive_session(ipc_client, request_rx, &events, invocation_tx).await
            {
                tracing::warn!("Module IPC session ended: {error}");
            }
        });

        // Create a receiver from the broadcast channel for ModuleIntegration
        let event_receiver = broadcast_rx;

        // Create NodeAPI over IPC
        let mut node_api_impl = NodeApiIpc::with_session(request_tx, module_id.clone());
        node_api_impl.set_event_broadcast(Arc::clone(&broadcast_tx_arc));
        let node_api = Arc::new(node_api_impl);

        Ok(Self {
            module_id,
            node_api,
            event_receiver,
            event_broadcast: broadcast_tx_arc,
            forward_events_from_subscribe: false,
            invocation_receiver: Some(invocation_rx),
        })
    }

    /// Create from existing NodeAPI (for in-process modules)
    pub fn from_node_api(module_id: String, node_api: Arc<dyn NodeAPI>) -> Self {
        let (broadcast_tx, broadcast_rx) = tokio::sync::broadcast::channel(1000);
        let event_broadcast = Arc::new(broadcast_tx);
        Self {
            module_id,
            node_api,
            event_receiver: broadcast_rx,
            event_broadcast,
            forward_events_from_subscribe: true,
            invocation_receiver: None,
        }
    }

    /// Subscribe to events
    ///
    /// Note: For out-of-process modules, events are delivered via the event_receiver
    /// set up during connect(). This method sends the subscription request via IPC.
    /// The receiver is already set up and will receive events automatically.
    pub async fn subscribe_events(&self, event_types: Vec<EventType>) -> Result<(), ModuleError> {
        let mut event_rx = self.node_api.subscribe_events(event_types).await?;
        if self.forward_events_from_subscribe {
            let broadcast_tx = Arc::clone(&self.event_broadcast);
            tokio::spawn(async move {
                while let Some(msg) = event_rx.recv().await {
                    let _ = broadcast_tx.send(msg);
                }
            });
        }
        Ok(())
    }

    /// Get NodeAPI instance
    pub fn node_api(&self) -> Arc<dyn NodeAPI> {
        Arc::clone(&self.node_api)
    }

    /// Get event receiver (creates a new receiver from the broadcast channel)
    pub fn event_receiver(&self) -> tokio::sync::broadcast::Receiver<ModuleMessage> {
        // Create a new receiver from the broadcast channel
        // Note: This requires access to the broadcast sender, which we don't have here
        // For now, return a clone of the receiver (broadcast receivers can be cloned)
        self.event_receiver.resubscribe()
    }

    /// Get module ID
    pub fn module_id(&self) -> &str {
        &self.module_id
    }

    /// Get receiver for CLI/RPC invocations from the node.
    ///
    /// When the node sends an Invocation (e.g. from `runmodulecli`), it will be delivered here.
    /// The module must receive, run the handler, and send the result via the oneshot.
    /// Returns None for in-process modules.
    pub fn invocation_receiver(
        &mut self,
    ) -> Option<&mut mpsc::Receiver<(InvocationMessage, oneshot::Sender<InvocationResultMessage>)>>
    {
        self.invocation_receiver.as_mut()
    }
}
