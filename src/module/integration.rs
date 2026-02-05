//! Module integration helper
//!
//! Provides a simple, unified API for modules to integrate with the node.
//! Handles connection, handshake, and provides NodeAPI + event receiver.

use crate::module::api::NodeApiIpc;
use crate::module::ipc::client::ModuleIpcClient;
use crate::module::ipc::protocol::{
    MessageType, ModuleMessage, RequestMessage, RequestPayload, ResponsePayload,
};
use crate::module::traits::{EventType, ModuleError, NodeAPI};
use std::path::PathBuf;
use std::sync::Arc;

/// Simple helper for module integration
/// Handles connection, handshake, and provides NodeAPI + event receiver
pub struct ModuleIntegration {
    module_id: String,
    node_api: Arc<dyn NodeAPI>,
    event_receiver: tokio::sync::broadcast::Receiver<ModuleMessage>,
}

impl ModuleIntegration {
    /// Connect to node via IPC (for out-of-process modules)
    pub async fn connect(
        socket_path: PathBuf,
        module_id: String,
        module_name: String,
        version: String,
    ) -> Result<Self, ModuleError> {
        use tracing::{error, info};

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
                return Err(ModuleError::IpcError("Invalid handshake response".to_string()));
            }
        }

        // Use broadcast channel for events (allows multiple receivers)
        let (broadcast_tx, broadcast_rx) = tokio::sync::broadcast::channel(1000);
        let broadcast_tx_arc = Arc::new(broadcast_tx);
        let ipc_client_arc = Arc::new(tokio::sync::Mutex::new(ipc_client));

        // Spawn event receiver task that forwards to broadcast channel
        let ipc_for_events = Arc::clone(&ipc_client_arc);
        let broadcast_tx_for_events = Arc::clone(&broadcast_tx_arc);
        tokio::spawn(async move {
            loop {
                match ipc_for_events.lock().await.receive_event().await {
                    Ok(Some(ModuleMessage::Event(e))) => {
                        let _ = broadcast_tx_for_events.send(ModuleMessage::Event(e));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        error!("Error receiving event: {}", e);
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
        })
    }

    /// Create from existing NodeAPI (for in-process modules)
    pub fn from_node_api(
        module_id: String,
        node_api: Arc<dyn NodeAPI>,
    ) -> Self {
        // In-process: events handled differently (via callbacks or direct subscription)
        // For now, create empty broadcast receiver - can be enhanced later
        let (_tx, rx) = tokio::sync::broadcast::channel(1);
        Self {
            module_id,
            node_api,
            event_receiver: rx,
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
}

