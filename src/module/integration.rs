//! Module integration helper
//!
//! Provides a simple, unified API for modules to integrate with the node.
//! Handles connection, handshake, and provides NodeAPI + event receiver.

use crate::module::api::NodeApiIpc;
use crate::module::ipc::protocol::{
    CliSpec, InvocationMessage, InvocationResultMessage, MessageType, ModuleMessage,
    RequestMessage, RequestPayload, ResponsePayload,
};
use crate::module::ipc::ModuleIpcClient;
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

        // Handshake
        let correlation_id = ipc_client.next_correlation_id();
        let request = RequestMessage {
            correlation_id,
            request_type: MessageType::Handshake,
            payload: RequestPayload::Handshake {
                module_id: module_id.clone(),
                module_name,
                version,
            },
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
        let ipc_client_arc = Arc::new(tokio::sync::Mutex::new(ipc_client));

        // Channel for invocations: spawn sends (Invocation, oneshot_tx), module receives and responds
        let (invocation_tx, invocation_rx) = mpsc::channel(32);

        // Spawn message receiver: forwards Events to broadcast, dispatches Invocations to module
        let ipc_for_receive = Arc::clone(&ipc_client_arc);
        let broadcast_tx_for_events = Arc::clone(&broadcast_tx_arc);
        tokio::spawn(async move {
            loop {
                match ipc_for_receive.lock().await.receive_message().await {
                    Ok(Some(ModuleMessage::Event(e))) => {
                        let _ = broadcast_tx_for_events.send(ModuleMessage::Event(e));
                    }
                    Ok(Some(ModuleMessage::Invocation(invocation))) => {
                        let (result_tx, result_rx) = oneshot::channel();
                        if invocation_tx.send((invocation, result_tx)).await.is_err() {
                            warn!("Invocation channel closed, dropping invocation");
                            continue;
                        }
                        match result_rx.await {
                            Ok(result) => {
                                if let Err(e) = ipc_for_receive
                                    .lock()
                                    .await
                                    .send_invocation_result(result)
                                    .await
                                {
                                    error!("Failed to send invocation result: {}", e);
                                }
                            }
                            Err(_) => {
                                warn!("Invocation result channel dropped");
                            }
                        }
                    }
                    Ok(Some(_)) => {
                        // Response, Log, InvocationResult - unexpected here, ignore
                    }
                    Ok(None) => break,
                    Err(e) => {
                        error!("Error receiving message: {}", e);
                        break;
                    }
                }
            }
        });

        // Create a receiver from the broadcast channel for ModuleIntegration
        let event_receiver = broadcast_rx;

        // Create NodeAPI over IPC
        let mut node_api_impl = NodeApiIpc::new(ipc_client_arc, module_id.clone());
        node_api_impl.set_event_broadcast(broadcast_tx_arc);
        let node_api = Arc::new(node_api_impl);

        Ok(Self {
            module_id,
            node_api,
            event_receiver,
            invocation_receiver: Some(invocation_rx),
        })
    }

    /// Create from existing NodeAPI (for in-process modules)
    pub fn from_node_api(module_id: String, node_api: Arc<dyn NodeAPI>) -> Self {
        // In-process: events handled differently (via callbacks or direct subscription)
        // For now, create empty broadcast receiver - can be enhanced later
        let (_tx, rx) = tokio::sync::broadcast::channel(1);
        Self {
            module_id,
            node_api,
            event_receiver: rx,
            invocation_receiver: None,
        }
    }

    /// Subscribe to events
    ///
    /// Note: For out-of-process modules, events are delivered via the event_receiver
    /// set up during connect(). This method sends the subscription request via IPC.
    /// The receiver is already set up and will receive events automatically.
    pub async fn subscribe_events(&self, event_types: Vec<EventType>) -> Result<(), ModuleError> {
        // Call NodeAPI's subscribe_events which handles the IPC request
        // The receiver returned is the same one we already have in event_receiver
        let _receiver = self.node_api.subscribe_events(event_types).await?;
        // We already have the receiver via event_receiver(), so we can ignore the returned one
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
