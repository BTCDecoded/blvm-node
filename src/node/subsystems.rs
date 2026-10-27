//! Aggregated optional subsystems for Node.
//!
//! Groups related optional components to reduce Node field count and clarify initialization.

use std::sync::Arc;

use crate::module::ModuleManager;
use crate::node::event_publisher::EventPublisher;

/// Module subsystem: registry, manager, and event publisher.
/// All three are initialized together when the module system is enabled.
/// ModuleManager is wrapped in Arc<Mutex<>> for sharing with RPC and other subsystems.
#[derive(Default)]
pub struct ModuleSubsystem {
    pub module_registry: Option<Arc<crate::module::registry::client::ModuleRegistry>>,
    pub module_manager: Option<Arc<tokio::sync::Mutex<ModuleManager>>>,
    pub event_publisher: Option<Arc<EventPublisher>>,
}
