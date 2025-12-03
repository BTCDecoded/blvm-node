# Event System Integration Guide

## Overview

The module event system is designed to handle common integration pain points in distributed module architectures. This document covers all integration scenarios, reliability guarantees, and best practices.

## Integration Pain Points Addressed

### 1. Event Delivery Reliability

**Problem**: Events can be lost if modules are slow or channels are full.

**Solution**:
- **Channel Buffering**: 100-event buffer per module (configurable)
- **Non-Blocking Delivery**: Uses `try_send` to avoid blocking the publisher
- **Channel Full Handling**: Events are dropped with warning (module is slow, not dead)
- **Channel Closed Detection**: Automatically removes dead modules from subscriptions
- **Delivery Statistics**: Track success/failure rates per module

**Code**:
```rust
// EventManager tracks delivery statistics
let stats = event_manager.get_delivery_stats("module_id").await;
// Returns: (successful_deliveries, failed_deliveries, channel_full_count)
```

### 2. Event Ordering and Timing

**Problem**: Events might arrive out of order or modules might miss events during startup.

**Solution**:
- **ModuleLoaded Timing**: Only published AFTER module subscribes (startup complete)
- **Hotloaded Modules**: Automatically receive all already-loaded modules when subscribing
- **Consistent Ordering**: Subscription → ModuleLoaded events (guaranteed order)

**Flow**:
1. Module loads → Recorded in `loaded_modules`
2. Module subscribes → Receives all already-loaded modules
3. ModuleLoaded published → After subscription (startup complete)

### 3. Event Channel Backpressure

**Problem**: Fast publishers can overwhelm slow consumers.

**Solution**:
- **Bounded Channels**: 100-event buffer prevents unbounded memory growth
- **Non-Blocking**: Publisher never blocks, events dropped if channel full
- **Statistics Tracking**: Monitor channel full events to identify slow modules
- **Automatic Cleanup**: Dead modules automatically removed

**Monitoring**:
```rust
let stats = event_manager.get_delivery_stats("module_id").await;
if stats.unwrap().2 > 100 {  // channel_full_count
    warn!("Module {} is slow, dropping events", module_id);
}
```

### 4. Missing Events During Startup

**Problem**: Modules that start later miss events from earlier modules.

**Solution**:
- **Hotloaded Module Support**: Newly subscribing modules receive all already-loaded modules
- **Event Replay**: ModuleLoaded events sent to newly subscribing modules
- **Consistent State**: All modules have consistent view of loaded modules

### 5. Event Type Coverage

**Problem**: Not all events have corresponding payloads or are published.

**Solution**:
- **Complete Coverage**: All EventType variants have corresponding EventPayload variants
- **Governance Events**: All governance events (EconomicNodeRegistered, EconomicNodeStatus, EconomicNodeForkDecision, EconomicNodeVeto) are published
- **Network Events**: All network events are published
- **Lifecycle Events**: All lifecycle events are published

## Event Categories

### Core Blockchain Events
- `NewBlock`: Block connected to chain
- `NewTransaction`: Transaction in mempool
- `BlockDisconnected`: Block disconnected (reorg)
- `ChainReorg`: Chain reorganization

### Governance Events
- `EconomicNodeRegistered`: Economic node registered
- `EconomicNodeStatus`: Status query/response
- `EconomicNodeForkDecision`: Fork decision made
- `EconomicNodeVeto`: Veto signal sent
- `GovernanceProposalCreated`: Proposal created
- `GovernanceProposalVoted`: Vote cast
- `GovernanceProposalMerged`: Proposal merged
- `VetoThresholdReached`: Veto threshold reached
- `GovernanceForkDetected`: Fork detected

### Network Events
- `PeerConnected`: Peer connected
- `PeerDisconnected`: Peer disconnected
- `PeerBanned`: Peer banned
- `MessageReceived`: Network message received
- `BroadcastStarted`: Broadcast started
- `BroadcastCompleted`: Broadcast completed

### Module Lifecycle Events
- `ModuleLoaded`: Module loaded (after subscription)
- `ModuleUnloaded`: Module unloaded
- `ModuleCrashed`: Module crashed
- `ModuleHealthChanged`: Health status changed

### Maintenance Events
- `DataMaintenance`: Unified cleanup/flush (replaces StorageFlush + DataCleanup)
- `MaintenanceStarted`: Maintenance started
- `MaintenanceCompleted`: Maintenance completed
- `HealthCheck`: Health check performed

### Resource Management Events
- `DiskSpaceLow`: Disk space low
- `ResourceLimitWarning`: Resource limit warning

## Event Delivery Guarantees

### At-Most-Once Delivery
- Events are delivered at most once per subscriber
- If channel is full, event is dropped (not retried)
- If channel is closed, module is removed from subscriptions

### Best-Effort Delivery
- Events are delivered on a best-effort basis
- No guaranteed delivery (modules can be slow/dead)
- Statistics track delivery success/failure rates

### Ordering Guarantees
- Events are delivered in order per module (single channel)
- No cross-module ordering guarantees
- ModuleLoaded events are ordered: subscription → ModuleLoaded

## Error Handling

### Channel Full
- Event is dropped with warning
- Module subscription is NOT removed (module is slow, not dead)
- Statistics track channel full count

### Channel Closed
- Module subscription is removed
- Statistics track failed delivery count
- Module is automatically cleaned up

### Serialization Errors
- Event is dropped with warning
- Module subscription is NOT removed
- Error is logged for debugging

## Monitoring and Debugging

### Delivery Statistics
```rust
// Get statistics for a module
let stats = event_manager.get_delivery_stats("module_id").await;
// Returns: Option<(successful, failed, channel_full)>

// Get statistics for all modules
let all_stats = event_manager.get_all_delivery_stats().await;
// Returns: HashMap<module_id, (successful, failed, channel_full)>

// Reset statistics (for testing)
event_manager.reset_delivery_stats("module_id").await;
```

### Event Subscribers
```rust
// Get list of subscribers for an event type
let subscribers = event_manager.get_subscribers(EventType::NewBlock).await;
// Returns: Vec<module_id>
```

## Best Practices

### For Module Developers

1. **Subscribe Early**: Subscribe to events as soon as possible after handshake
2. **Handle Events Quickly**: Keep event handlers fast and non-blocking
3. **Monitor Statistics**: Check delivery statistics to ensure events are received
4. **Handle ModuleLoaded**: Always handle ModuleLoaded to know about other modules
5. **Graceful Shutdown**: Handle NodeShutdown and DataMaintenance (urgency: "high")

### For Node Developers

1. **Publish Consistently**: Publish events at consistent points in the code
2. **Use EventPublisher**: Use EventPublisher for all event publishing
3. **Monitor Statistics**: Monitor delivery statistics to identify slow modules
4. **Handle Errors**: Log warnings for failed event deliveries
5. **Test Integration**: Test event delivery in integration tests

## Common Integration Scenarios

### Scenario 1: Module Startup
1. Module process spawned
2. Module connects via IPC
3. Module sends Handshake
4. Module subscribes to events
5. Module receives ModuleLoaded for all already-loaded modules
6. ModuleLoaded published for this module (after subscription)

### Scenario 2: Hotloaded Module
1. Module B loads while Module A is already running
2. Module B subscribes to events
3. Module B receives ModuleLoaded for Module A
4. ModuleLoaded published for Module B
5. Module A receives ModuleLoaded for Module B

### Scenario 3: Slow Module
1. Module receives events slowly
2. Event channel fills up (100 events)
3. New events are dropped with warning
4. Statistics track channel full count
5. Module subscription is NOT removed (module is slow, not dead)

### Scenario 4: Dead Module
1. Module process crashes
2. Event channel is closed
3. Event delivery fails
4. Module subscription is automatically removed
5. Statistics track failed delivery count

### Scenario 5: Governance Event Flow
1. Network receives EconomicNodeRegistration
2. Event published to governance module
3. Governance module processes event
4. Governance module may publish additional events
5. All events delivered via same reliable channel

## Configuration

### Channel Buffer Size
Currently hardcoded to 100 events per module. Can be made configurable in the future.

### Event Statistics
Statistics are kept in memory and reset on node restart. Can be persisted in the future.

## Future Improvements

1. **Configurable Buffer Size**: Make channel buffer size configurable per module
2. **Event Persistence**: Persist events for replay after module restart
3. **Event Filtering**: Allow modules to filter events by criteria
4. **Event Priority**: Add priority queue for critical events
5. **Event Metrics**: Add Prometheus metrics for event delivery
6. **Event Replay**: Allow modules to replay missed events

