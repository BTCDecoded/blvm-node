# Janitorial and Maintenance Events

## Overview

The module system provides comprehensive janitorial and maintenance events that allow modules to participate in node lifecycle, resource management, and data maintenance operations. This ensures modules can perform their own cleanup, maintenance, and resource management in sync with the node.

## Event Categories

### 1. Node Lifecycle Events

#### NodeShutdown
**When**: Node is shutting down (before components stop)
**Purpose**: Allow modules to clean up gracefully
**Payload**:
- `reason`: String - Shutdown reason ("graceful", "signal", "rpc", "error")
- `timeout_seconds`: u64 - Graceful shutdown timeout

**Module Action**: 
- Save state
- Close connections
- Flush data
- Clean up resources

#### NodeShutdownCompleted
**When**: Node shutdown is complete
**Purpose**: Notify modules that shutdown finished
**Payload**:
- `duration_ms`: u64 - Shutdown duration

#### NodeStartupCompleted
**When**: Node startup is complete (all components initialized)
**Purpose**: Notify modules that node is fully operational
**Payload**:
- `duration_ms`: u64 - Startup duration
- `components`: Vec<String> - Components that were initialized

**Module Action**:
- Initialize connections
- Load state
- Start processing

### 2. Storage Events

#### StorageFlush
**When**: Storage flush is requested (shutdown, periodic, manual)
**Purpose**: Allow modules to flush their data
**Payload**:
- `reason`: String - Flush reason ("shutdown", "periodic", "manual")

**Module Action**:
- Flush pending writes
- Sync data to disk
- Commit transactions

#### StorageFlushCompleted
**When**: Storage flush is complete
**Purpose**: Notify modules that flush finished
**Payload**:
- `success`: bool - Success status
- `duration_ms`: u64 - Flush duration

### 3. Maintenance Events

#### MaintenanceStarted
**When**: Maintenance operation starts (backup, cleanup, prune)
**Purpose**: Allow modules to prepare for maintenance
**Payload**:
- `maintenance_type`: String - Type of maintenance ("backup", "cleanup", "prune")
- `estimated_duration_seconds`: Option<u64> - Estimated duration

**Module Action**:
- Pause non-critical operations
- Prepare for maintenance
- Save state

#### MaintenanceCompleted
**When**: Maintenance operation completes
**Purpose**: Notify modules that maintenance finished
**Payload**:
- `maintenance_type`: String - Type of maintenance
- `success`: bool - Success status
- `duration_ms`: u64 - Duration
- `results`: Option<String> - Results/statistics (JSON)

**Module Action**:
- Resume operations
- Verify state
- Update metrics

#### DataCleanup
**When**: Periodic data cleanup opportunity
**Purpose**: Allow modules to clean up old data
**Payload**:
- `cleanup_type`: String - Type of cleanup ("periodic", "manual", "low_disk")
- `target_age_days`: Option<u64> - Target age for cleanup

**Module Action**:
- Delete old data
- Archive historical data
- Compact databases
- Remove temporary files

### 4. Health and Monitoring Events

#### HealthCheck
**When**: Health check is performed (periodic, manual, startup)
**Purpose**: Allow modules to report their health status
**Payload**:
- `check_type`: String - Type of check ("periodic", "manual", "startup")
- `node_healthy`: bool - Node health status
- `health_report`: Option<String> - Health report (JSON)

**Module Action**:
- Report health metrics
- Check internal state
- Verify dependencies

### 5. Resource Management Events

#### DiskSpaceLow
**When**: Disk space is low (below threshold)
**Purpose**: Allow modules to clean up data to free space
**Payload**:
- `available_bytes`: u64 - Available space in bytes
- `total_bytes`: u64 - Total space in bytes
- `percent_free`: f64 - Percentage free
- `disk_path`: String - Disk path

**Module Action**:
- Delete old data
- Compress data
- Move data to external storage
- Reduce cache sizes

#### ResourceLimitWarning
**When**: Resource limit is approaching (memory, CPU, disk, network)
**Purpose**: Allow modules to reduce resource usage
**Payload**:
- `resource_type`: String - Resource type ("memory", "cpu", "disk", "network")
- `usage_percent`: f64 - Current usage percentage
- `current_usage`: u64 - Current usage value
- `limit`: u64 - Limit value
- `threshold_percent`: f64 - Warning threshold percentage

**Module Action**:
- Reduce memory usage
- Throttle operations
- Free cached data
- Reduce connection count

## Usage Examples

### Example: Module Cleanup on Shutdown

```rust
// Subscribe to shutdown events
let event_types = vec![
    EventType::NodeShutdown,
    EventType::StorageFlush,
];
client.subscribe_events(event_types).await?;

// In event loop:
match event {
    ModuleMessage::Event(event_msg) => {
        match event_msg.event_type {
            EventType::NodeShutdown => {
                if let EventPayload::NodeShutdown { reason, timeout_seconds } = &event_msg.payload {
                    info!("Node shutting down: reason={}, timeout={}s", reason, timeout_seconds);
                    // Clean up gracefully
                    cleanup_resources().await?;
                    flush_data().await?;
                }
            }
            EventType::StorageFlush => {
                if let EventPayload::StorageFlush { reason } = &event_msg.payload {
                    info!("Storage flush requested: reason={}", reason);
                    // Flush module data
                    module_storage.flush().await?;
                }
            }
            _ => {}
        }
    }
}
```

### Example: Disk Space Management

```rust
// Subscribe to disk space events
let event_types = vec![EventType::DiskSpaceLow];
client.subscribe_events(event_types).await?;

// In event loop:
match event {
    ModuleMessage::Event(event_msg) => {
        if event_msg.event_type == EventType::DiskSpaceLow {
            if let EventPayload::DiskSpaceLow { available_bytes, percent_free, .. } = &event_msg.payload {
                warn!("Disk space low: {} bytes available ({:.2}% free)", available_bytes, percent_free);
                // Clean up old data
                cleanup_old_data(percent_free).await?;
            }
        }
    }
}
```

### Example: Periodic Data Cleanup

```rust
// Subscribe to cleanup events
let event_types = vec![EventType::DataCleanup];
client.subscribe_events(event_types).await?;

// In event loop:
match event {
    ModuleMessage::Event(event_msg) => {
        if event_msg.event_type == EventType::DataCleanup {
            if let EventPayload::DataCleanup { cleanup_type, target_age_days } = &event_msg.payload {
                info!("Data cleanup: type={}, target_age={:?} days", cleanup_type, target_age_days);
                // Clean up old data
                let age_days = target_age_days.unwrap_or(30);
                cleanup_data_older_than(age_days).await?;
            }
        }
    }
}
```

## Best Practices

1. **Graceful Shutdown**: Always handle NodeShutdown events to clean up gracefully
2. **Data Flushing**: Flush data on StorageFlush events to ensure data integrity
3. **Resource Management**: Monitor DiskSpaceLow and ResourceLimitWarning events
4. **Periodic Cleanup**: Use DataCleanup events for regular maintenance
5. **Health Reporting**: Report health status on HealthCheck events
6. **Non-Blocking**: Keep event handlers fast and non-blocking
7. **Error Handling**: Always handle errors gracefully, don't crash on event processing

## Event Timing

- **NodeShutdown**: Published before components stop (modules have time to clean up)
- **StorageFlush**: Published before storage flush (modules can flush their data)
- **DiskSpaceLow**: Published when disk space is low (modules can clean up)
- **DataCleanup**: Published periodically (modules can clean up old data)
- **HealthCheck**: Published periodically (modules can report health)

## Integration with Node Operations

These events are automatically published by the node during:
- Shutdown sequence
- Storage operations
- Disk space checks
- Maintenance operations
- Health checks
- Resource monitoring

Modules can subscribe to these events to participate in node lifecycle and maintenance operations, ensuring full integration with the node's janitorial tasks.

