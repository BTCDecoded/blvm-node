# Module Event System Consistency

## Overview

The module event system is designed to be **consistent, minimal, and extensible**. All events follow a clear pattern and timing to ensure modules can integrate seamlessly with the node.

## Event Timing and Consistency

### ModuleLoaded Event

**Key Principle**: `ModuleLoaded` events are **only published AFTER a module has subscribed** (after startup is complete).

**Flow**:
1. Module process is spawned
2. Module connects via IPC and sends Handshake
3. Module sends `SubscribeEvents` request
4. **At subscription time**:
   - Module receives `ModuleLoaded` events for all already-loaded modules (hotloaded modules get existing modules)
   - `ModuleLoaded` is published for the newly subscribing module (if it's loaded)
5. Module is now fully operational

**Why this design?**
- Ensures `ModuleLoaded` only happens after module is fully ready (subscribed)
- Hotloaded modules automatically receive all existing modules
- Consistent event ordering: subscription → ModuleLoaded
- No race conditions: modules can't miss events

### Example Flow

**Startup (Module A loads first)**:
1. Module A process spawned
2. Module A connects and handshakes
3. Module A subscribes to events
4. `ModuleLoaded` published for Module A (no other modules yet)

**Hotload (Module B loads later)**:
1. Module B process spawned
2. Module B connects and handshakes
3. Module B subscribes to events
4. Module B receives `ModuleLoaded` for Module A (already loaded)
5. `ModuleLoaded` published for Module B (all modules get it)

## Unified Events

### DataMaintenance (Unified Cleanup/Flush)

**Replaces**: `StorageFlush` and `DataCleanup` (unified into one extensible event)

**Purpose**: Single event for all data maintenance operations

**Payload**:
- `operation`: "flush", "cleanup", or "both"
- `urgency`: "low", "medium", or "high"
- `reason`: "periodic", "shutdown", "low_disk", "manual"
- `target_age_days`: Optional (for cleanup operations)
- `timeout_seconds`: Optional (for high urgency operations)

**Usage Examples**:
- **Shutdown**: `DataMaintenance { operation: "flush", urgency: "high", reason: "shutdown", timeout_seconds: Some(5) }`
- **Periodic Cleanup**: `DataMaintenance { operation: "cleanup", urgency: "low", reason: "periodic", target_age_days: Some(30) }`
- **Low Disk**: `DataMaintenance { operation: "both", urgency: "high", reason: "low_disk", target_age_days: Some(7), timeout_seconds: Some(10) }`

**Benefits**:
- Single event for all maintenance operations
- Extensible: easy to add new operation types or urgency levels
- Clear semantics: operation + urgency + reason
- Modules can handle all maintenance in one place

## Event Categories

### 1. Node Lifecycle
- `NodeStartupCompleted`: Node is fully operational
- `NodeShutdown`: Node is shutting down (modules should clean up)
- `NodeShutdownCompleted`: Shutdown finished

### 2. Module Lifecycle
- `ModuleLoaded`: Module loaded and subscribed (after startup complete)
- `ModuleUnloaded`: Module unloaded
- `ModuleReloaded`: Module reloaded
- `ModuleCrashed`: Module crashed

### 3. Configuration
- `ConfigLoaded`: Node configuration loaded/changed

### 4. Maintenance
- `DataMaintenance`: Unified cleanup/flush event (replaces StorageFlush + DataCleanup)
- `MaintenanceStarted`: Maintenance operation started
- `MaintenanceCompleted`: Maintenance operation completed
- `HealthCheck`: Health check performed

### 5. Resource Management
- `DiskSpaceLow`: Disk space is low
- `ResourceLimitWarning`: Resource limit approaching

## Best Practices

1. **Subscribe Early**: Modules should subscribe to events as soon as possible after handshake
2. **Handle ModuleLoaded**: Always handle `ModuleLoaded` to know about other modules
3. **DataMaintenance**: Handle all maintenance operations in one place using `DataMaintenance`
4. **Graceful Shutdown**: Always handle `NodeShutdown` and `DataMaintenance` (urgency: "high")
5. **Non-Blocking**: Keep event handlers fast and non-blocking

## Consistency Guarantees

1. **ModuleLoaded Timing**: Always happens after subscription (startup complete)
2. **Hotloaded Modules**: Always receive all already-loaded modules
3. **Event Ordering**: Consistent ordering (subscription → ModuleLoaded)
4. **No Race Conditions**: Events are delivered reliably
5. **Unified Maintenance**: Single event for all maintenance operations

## Extensibility

The event system is designed to be easily extensible:

1. **Add New Events**: Add to `EventType` enum and `EventPayload` enum
2. **Add Event Publishers**: Add methods to `EventPublisher`
3. **Add Event Handlers**: Modules subscribe and handle events
4. **Unified Patterns**: Follow existing patterns (e.g., DataMaintenance)

## Migration from Old Events

**Old**: `StorageFlush` + `DataCleanup`
**New**: `DataMaintenance` with `operation` and `urgency` fields

**Migration**:
```rust
// Old
match event_type {
    EventType::StorageFlush => { flush_data().await?; }
    EventType::DataCleanup => { cleanup_data().await?; }
}

// New
match event_type {
    EventType::DataMaintenance => {
        if let EventPayload::DataMaintenance { operation, urgency, .. } = payload {
            if operation == "flush" || operation == "both" {
                flush_data().await?;
            }
            if operation == "cleanup" || operation == "both" {
                cleanup_data().await?;
            }
        }
    }
}
```

