# Module Integration System

## Overview

The module integration system provides a consistent, secure, and performant way for modules to integrate with the node. All modules follow the same integration pattern:

1. **IPC Communication**: Modules communicate via Unix domain sockets
2. **Event Subscription**: Modules subscribe to events they care about
3. **NodeAPI Access**: Modules access node functionality through the NodeAPI interface
4. **Configuration**: Modules receive configuration via ModuleContext and can react to ConfigLoaded events
5. **Process Isolation**: Each module runs in a separate process for security

## Integration Points

### 1. Module Lifecycle

Modules follow a standard lifecycle:
- **Load**: Module binary is discovered and loaded by ModuleManager
- **Initialize**: Module receives ModuleContext with config, data_dir, socket_path
- **Subscribe**: Module subscribes to events via `SubscribeEvents` IPC call
- **Start**: Module begins processing events and handling requests
- **Stop**: Module gracefully shuts down
- **Unload**: Module process is terminated

### 2. Event System

Modules subscribe to events they need:

```rust
// Example: blvm-governance subscribes to governance events
let event_types = vec![
    EventType::GovernanceProposalCreated,
    EventType::EconomicNodeRegistered,
    EventType::NewBlock,
    EventType::ConfigLoaded, // React to config changes
];
client.subscribe_events(event_types).await?;
```

**Available Events:**
- **Blockchain Events**: NewBlock, ChainReorg, BlockValidationStarted, etc.
- **Mempool Events**: MempoolTransactionAdded, FeeRateChanged, etc.
- **Network Events**: PeerConnected, MessageReceived, etc.
- **Module Lifecycle**: ModuleLoaded, ModuleUnloaded, etc.
- **Configuration**: ConfigLoaded (NEW - allows modules to react to config changes)
- **Governance**: EconomicNodeRegistered, EconomicNodeVeto, etc.

### 3. Configuration Integration

Modules can react to node configuration in two ways:

#### A. Initial Configuration (ModuleContext)
When a module is loaded, it receives a `ModuleContext` with:
- `module_id`: Unique identifier
- `config`: HashMap<String, String> from module's config.toml
- `data_dir`: Directory for module data
- `socket_path`: IPC socket path

#### B. ConfigLoaded Event (NEW)
Modules can subscribe to `ConfigLoaded` event to react to node configuration changes:

```rust
// Subscribe to ConfigLoaded event
let event_types = vec![EventType::ConfigLoaded];
client.subscribe_events(event_types).await?;

// In event loop:
match event {
    ModuleMessage::Event(event_msg) => {
        if event_msg.event_type == EventType::ConfigLoaded {
            if let EventPayload::ConfigLoaded { changed_sections, config_json } = &event_msg.payload {
                // React to config changes
                if changed_sections.contains(&"governance".to_string()) {
                    // Reconfigure governance module
                }
            }
        }
    }
}
```

**ConfigLoaded Event Payload:**
- `changed_sections`: Vec<String> - Which config sections changed (e.g., ["network", "governance"])
- `config_json`: Option<String> - Full config as JSON (for modules that need it)

### 4. NodeAPI Integration

Modules access node functionality through NodeAPI:

```rust
let node_api = Arc::new(NodeApiIpc::new(ipc_client));

// Get blockchain data
let block = node_api.get_block(&hash).await?;
let height = node_api.get_block_height().await?;

// Get mempool data
let txs = node_api.get_mempool_transactions().await?;

// Get network stats
let stats = node_api.get_network_stats().await?;
```

**Available APIs:**
- **Blockchain API**: get_block, get_block_header, get_chain_tip, etc.
- **Mempool API**: get_mempool_transactions, get_fee_estimate, etc.
- **Network API**: get_network_stats, get_network_peers, etc.
- **Module API**: publish_event, call_module, etc.

### 5. Module-to-Module Communication

Modules can communicate with each other:

```rust
// Call another module
let response = node_api.call_module(
    Some("blvm-mesh"),
    "route_packet",
    params,
).await?;
```

## Module-Specific Integration

### blvm-governance

**Integration Points:**
- Subscribes to: `EconomicNodeRegistered`, `EconomicNodeVeto`, `NewBlock`, `ConfigLoaded`
- Publishes: Governance webhook notifications
- Uses: NodeAPI for block data, network stats

**Configuration:**
- Reads `governance.webhook_url` and `governance.node_id` from node config
- Reacts to `ConfigLoaded` event to update webhook configuration

### blvm-mesh

**Integration Points:**
- Subscribes to: `PeerConnected`, `MessageReceived`, `NewBlock`, `ConfigLoaded`
- Publishes: Mesh routing events
- Uses: NodeAPI for network stats, peer information

**Configuration:**
- Reads mesh routing configuration from module config.toml
- Reacts to `ConfigLoaded` event for network config changes

### blvm-lightning

**Integration Points:**
- Subscribes to: `NewBlock`, `MempoolTransactionAdded`, `PaymentRequestCreated`, `ConfigLoaded`
- Publishes: Lightning payment events
- Uses: NodeAPI for blockchain data, mempool queries

**Configuration:**
- Reads Lightning provider configuration from module config.toml
- Reacts to `ConfigLoaded` event for payment config changes

### blvm-stratum-v2

**Integration Points:**
- Subscribes to: `NewBlock`, `BlockTemplateUpdated`, `ConfigLoaded`
- Publishes: Mining job events
- Uses: NodeAPI for block templates, mining stats

**Configuration:**
- Reads mining pool configuration from module config.toml
- Reacts to `ConfigLoaded` event for mining config changes

## Best Practices

1. **Event Subscription**: Subscribe only to events you need
2. **Configuration**: Use ModuleContext for initial config, ConfigLoaded for runtime changes
3. **Error Handling**: Always handle errors gracefully, don't crash the module
4. **Resource Management**: Clean up resources on shutdown
5. **Security**: Never trust input from other modules, validate all data

## Security Considerations

- **Process Isolation**: Modules run in separate processes
- **IPC Security**: Unix domain sockets with proper permissions
- **Permission System**: Modules have capabilities (read_blockchain, subscribe_events, etc.)
- **Sandboxing**: Filesystem sandboxing for module data directories
- **Signature Verification**: Modules can be signed and verified

## Performance Considerations

- **Event Batching**: Events are published asynchronously, non-blocking
- **IPC Efficiency**: Unix domain sockets are fast for local communication
- **Resource Limits**: Modules have CPU, memory, and filesystem limits
- **Lazy Loading**: Modules are loaded on-demand

## Future Enhancements

- **Module Registry**: P2P module discovery and installation
- **Module Dependencies**: Modules can depend on other modules
- **Module Versioning**: Support for module versioning and updates
- **Module Metrics**: Built-in metrics collection for modules

