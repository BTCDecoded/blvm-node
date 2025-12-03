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

// Expose loaded_modules for ModuleManager to track loaded modules
impl EventManager {
    /// Get reference to loaded_modules for tracking
    pub fn loaded_modules(&self) -> &Arc<TokioMutex<HashMap<String, (String, u64)>>> {
        &self.loaded_modules
    }
}

/// Event subscription manager
pub struct EventManager {
    /// Event subscribers by event type
    subscribers: Arc<TokioMutex<HashMap<EventType, Vec<String>>>>,
    /// Event channels for each module (module_id -> sender)
    module_channels: Arc<TokioMutex<HashMap<String, mpsc::Sender<ModuleMessage>>>>,
    /// Track loaded modules for sending to newly subscribing modules
    /// (module_name -> (version, loaded_timestamp))
    loaded_modules: Arc<TokioMutex<HashMap<String, (String, u64)>>>,
    /// Event delivery statistics (for monitoring and debugging)
    /// (module_id -> (successful_deliveries, failed_deliveries, channel_full_count))
    delivery_stats: Arc<TokioMutex<HashMap<String, (u64, u64, u64)>>>,
}

impl EventManager {
    /// Create a new event manager
    pub fn new() -> Self {
        Self {
            subscribers: Arc::new(TokioMutex::new(HashMap::new())),
            module_channels: Arc::new(TokioMutex::new(HashMap::new())),
            loaded_modules: Arc::new(TokioMutex::new(HashMap::new())),
            delivery_stats: Arc::new(TokioMutex::new(HashMap::new())),
        }
    }

    /// Subscribe a module to events
    ///
    /// When a module subscribes, it will receive ModuleLoaded events for all already-loaded modules
    /// to ensure consistency (hotloaded modules know about existing modules).
    ///
    /// ModuleLoaded events are only published AFTER subscription (after startup is complete).
    /// This ensures consistent event ordering: subscription -> ModuleLoaded.
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
        let mut loaded_modules = self.loaded_modules.lock().await;

        // Register module channel
        channels.insert(module_id.clone(), sender.clone());

        // Add module to subscribers for each event type
        for event_type in &event_types {
            subscribers
                .entry(event_type.clone())
                .or_insert_with(Vec::new)
                .push(module_id.clone());
        }

        // Extract module name from module_id (format: {module_name}_{uuid})
        let subscribing_module_name = module_id
            .split('_')
            .next()
            .unwrap_or(&module_id)
            .to_string();

        // If module subscribes to ModuleLoaded:
        // 1. Send it events for all already-loaded modules (hotloaded modules get existing modules)
        // 2. Publish ModuleLoaded for this newly subscribing module (if it's loaded)
        //    This ensures ModuleLoaded only happens AFTER subscription (startup complete)
        let should_publish_loaded = if event_types.contains(&EventType::ModuleLoaded) {
            use crate::module::ipc::protocol::{EventMessage, EventPayload, ModuleMessage};

            // Collect already-loaded modules to send (clone data before dropping lock)
            let already_loaded: Vec<(String, String)> = loaded_modules
                .iter()
                .map(|(name, (version, _))| (name.clone(), version.clone()))
                .collect();

            // Check if this module is loaded (for publishing ModuleLoaded after subscription)
            let module_version = loaded_modules
                .get(&subscribing_module_name)
                .map(|(version, _)| version.clone());

            // Send all already-loaded modules to this newly subscribing module
            for (module_name, version) in &already_loaded {
                let payload = EventPayload::ModuleLoaded {
                    module_name: module_name.clone(),
                    version: version.clone(),
                };
                let event_msg = ModuleMessage::Event(EventMessage {
                    event_type: EventType::ModuleLoaded,
                    payload,
                });
                // Send to module (non-blocking, best-effort)
                if let Err(_) = sender.try_send(event_msg) {
                    warn!("Failed to send ModuleLoaded event for {} to newly subscribing module {} (channel full)", module_name, module_id);
                }
            }

            // Return version if module is loaded (for publishing after dropping locks)
            module_version
        } else {
            None
        };

        // Drop all locks before publishing (to avoid deadlock)
        drop(loaded_modules);
        drop(subscribers);
        drop(channels);

        // Now publish ModuleLoaded for this newly subscribing module (if it's loaded)
        // This ensures ModuleLoaded only happens AFTER subscription (startup complete)
        // Other already-subscribed modules will receive this event
        if let Some(version) = should_publish_loaded {
            use crate::module::ipc::protocol::EventPayload;
            let payload = EventPayload::ModuleLoaded {
                module_name: subscribing_module_name.clone(),
                version,
            };
            // Publish to all subscribers (including the newly subscribing module)
            if let Err(e) = self.publish_event(EventType::ModuleLoaded, payload).await {
                warn!(
                    "Failed to publish ModuleLoaded event for newly subscribing module {}: {}",
                    subscribing_module_name, e
                );
            } else {
                info!(
                    "Published ModuleLoaded event for {} (after subscription)",
                    subscribing_module_name
                );
            }
        }

        Ok(())
    }

    /// Unsubscribe a module (when module unloads)
    pub async fn unsubscribe_module(&self, module_id: &str) -> Result<(), ModuleError> {
        debug!("Module {} unsubscribing from events", module_id);

        let mut subscribers = self.subscribers.lock().await;
        let mut channels = self.module_channels.lock().await;
        let mut stats = self.delivery_stats.lock().await;

        // Remove module channel
        channels.remove(module_id);

        // Remove module from all subscriber lists
        for subscribers_list in subscribers.values_mut() {
            subscribers_list.retain(|id| id != module_id);
        }

        // Clean up delivery statistics
        stats.remove(module_id);

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
        let mut stats_updates = Vec::new();

        for (module_id, sender) in channels_snapshot {
            let event_msg_clone = Arc::clone(&event_message);
            // Use try_send to avoid blocking if channel is full or receiver is dropped
            match sender.try_send((*event_msg_clone).clone()) {
                Ok(()) => {
                    // Track successful delivery
                    stats_updates.push((module_id.clone(), true, false));
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(
                        "Event channel full for module {} (event: {:?}), event dropped",
                        module_id, event_type
                    );
                    // Track channel full (but don't remove subscription - module might catch up)
                    stats_updates.push((module_id.clone(), false, true));
                    // Don't add to failed_modules - module is still alive, just slow
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    warn!(
                        "Event channel closed for module {} (event: {:?}), removing subscription",
                        module_id, event_type
                    );
                    // Track failed delivery and mark for removal
                    stats_updates.push((module_id.clone(), false, false));
                    failed_modules.push(module_id);
                }
            }
        }

        // Update delivery statistics
        {
            let mut stats = self.delivery_stats.lock().await;
            for (module_id, success, channel_full) in stats_updates {
                let entry = stats.entry(module_id).or_insert((0, 0, 0));
                if success {
                    entry.0 += 1;
                } else if channel_full {
                    entry.2 += 1; // Channel full count
                } else {
                    entry.1 += 1; // Failed delivery count
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
