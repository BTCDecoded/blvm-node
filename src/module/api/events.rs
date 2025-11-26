//! Event notification system for modules
//!
//! Handles event subscriptions and delivery to modules.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex as TokioMutex;
use tracing::{debug, info, warn};

use crate::module::ipc::protocol::{EventMessage, EventPayload, ModuleMessage};
use crate::module::traits::{EventType, ModuleError};

/// Event subscription manager
pub struct EventManager {
    /// Event subscribers by event type
    subscribers: Arc<TokioMutex<HashMap<EventType, Vec<String>>>>,
    /// Event channels for each module (module_id -> sender)
    module_channels: Arc<TokioMutex<HashMap<String, mpsc::Sender<ModuleMessage>>>>,
}

impl EventManager {
    /// Create a new event manager
    pub fn new() -> Self {
        Self {
            subscribers: Arc::new(TokioMutex::new(HashMap::new())),
            module_channels: Arc::new(TokioMutex::new(HashMap::new())),
        }
    }

    /// Subscribe a module to events
    pub async fn subscribe_module(
        &self,
        module_id: String,
        event_types: Vec<EventType>,
        sender: mpsc::Sender<ModuleMessage>,
    ) -> Result<(), ModuleError> {
        info!(
            "Module {} subscribing to events: {:?}",
            module_id, event_types
        );

        let mut subscribers = self.subscribers.lock().await;
        let mut channels = self.module_channels.lock().await;

        // Register module channel
        channels.insert(module_id.clone(), sender);

        // Add module to subscribers for each event type
        for event_type in event_types {
            subscribers
                .entry(event_type)
                .or_insert_with(Vec::new)
                .push(module_id.clone());
        }

        Ok(())
    }

    /// Unsubscribe a module (when module unloads)
    pub async fn unsubscribe_module(&self, module_id: &str) -> Result<(), ModuleError> {
        debug!("Module {} unsubscribing from events", module_id);

        let mut subscribers = self.subscribers.lock().await;
        let mut channels = self.module_channels.lock().await;

        // Remove module channel
        channels.remove(module_id);

        // Remove module from all subscriber lists
        for subscribers_list in subscribers.values_mut() {
            subscribers_list.retain(|id| id != module_id);
        }

        Ok(())
    }

    /// Publish an event to all subscribed modules
    pub async fn publish_event(
        &self,
        event_type: EventType,
        payload: EventPayload,
    ) -> Result<(), ModuleError> {
        debug!("Publishing event: {:?}", event_type);

        let subscribers = self.subscribers.lock().await;
        let channels = self.module_channels.lock().await;

        // Get list of modules subscribed to this event type
        let module_ids = subscribers.get(&event_type).cloned().unwrap_or_default();

        if module_ids.is_empty() {
            return Ok(()); // No subscribers
        }

        // Create event message (shared via Arc to avoid cloning)
        let event_message = Arc::new(ModuleMessage::Event(EventMessage {
            event_type,
            payload,
        }));

        // Build snapshot of channels while holding locks
        let channels_snapshot: Vec<(String, mpsc::Sender<ModuleMessage>)> = {
            module_ids
                .iter()
                .filter_map(|id| channels.get(id).map(|sender| (id.clone(), sender.clone())))
                .collect()
        };
        
        // Explicitly drop locks before sending to avoid deadlock
        drop(subscribers);
        drop(channels);

        // Send to all subscribed modules without holding locks
        let mut failed_modules = Vec::new();
        for (module_id, sender) in channels_snapshot {
            let event_msg_clone = Arc::clone(&event_message);
            // Use try_send to avoid blocking if channel is full or receiver is dropped
            match sender.try_send((*event_msg_clone).clone()) {
                Ok(()) => {}
                Err(e) => {
                    warn!("Failed to send event to module {}: {}", module_id, e);
                    failed_modules.push(module_id);
                }
            }
        }

        // Clean up failed channels and remove from subscribers
        if !failed_modules.is_empty() {
            let mut channels = self.module_channels.lock().await;
            let mut subscribers = self.subscribers.lock().await;
            for module_id in failed_modules {
                channels.remove(&module_id);
                // Remove from all subscriber lists
                for subscribers_list in subscribers.values_mut() {
                    subscribers_list.retain(|id| id != &module_id);
                }
            }
        }

        Ok(())
    }

    /// Get list of subscribed modules for an event type
    pub async fn get_subscribers(&self, event_type: EventType) -> Vec<String> {
        let subscribers = self.subscribers.lock().await;
        subscribers.get(&event_type).cloned().unwrap_or_default()
    }
}

impl Default for EventManager {
    fn default() -> Self {
        Self::new()
    }
}
