# ZMQ Notifications

BLLVM node supports ZeroMQ (ZMQ) notifications for real-time blockchain event notifications, compatible with standard Bitcoin node notification interfaces.

## Overview

ZMQ notifications allow external applications to receive real-time updates about blockchain events without polling. This is useful for:
- Block explorers
- Wallet applications
- Trading systems
- Monitoring tools
- Analytics platforms

## Supported Notification Types

BLLVM node supports all standard Bitcoin node ZMQ notification types:

### 1. `hashblock` - Block Hash Notifications

Publishes the 32-byte block hash when a new block is connected to the chain.

**Message Format:**
- Topic: `"hashblock"` (string)
- Data: Block hash (32 bytes)

**Example:**
```toml
[zmq]
hashblock = "tcp://127.0.0.1:28332"
```

### 2. `hashtx` - Transaction Hash Notifications

Publishes the 32-byte transaction hash when a transaction enters or leaves the mempool.

**Message Format:**
- Topic: `"hashtx"` (string)
- Data: Transaction hash (32 bytes)

**Example:**
```toml
[zmq]
hashtx = "tcp://127.0.0.1:28333"
```

### 3. `rawblock` - Raw Block Data Notifications

Publishes the complete serialized block data when a new block is connected.

**Message Format:**
- Topic: `"rawblock"` (string)
- Data: Serialized block (variable length)

**Example:**
```toml
[zmq]
rawblock = "tcp://127.0.0.1:28334"
```

### 4. `rawtx` - Raw Transaction Data Notifications

Publishes the complete serialized transaction data when a transaction enters or leaves the mempool.

**Message Format:**
- Topic: `"rawtx"` (string)
- Data: Serialized transaction (variable length)

**Example:**
```toml
[zmq]
rawtx = "tcp://127.0.0.1:28335"
```

### 5. `sequence` - Sequence Notifications

Publishes sequence notifications for mempool events with sequence numbers.

**Message Format:**
- Topic: `"sequence"` (string)
- Data: 33 bytes (1 byte type + 32 bytes transaction hash)
  - Type: `0x01` = mempool entry, `0x02` = mempool removal

**Example:**
```toml
[zmq]
sequence = "tcp://127.0.0.1:28336"
```

## Configuration

ZMQ notifications are configured in the node configuration file (TOML format):

```toml
[zmq]
# Block hash notifications
hashblock = "tcp://127.0.0.1:28332"

# Transaction hash notifications
hashtx = "tcp://127.0.0.1:28333"

# Raw block data notifications
rawblock = "tcp://127.0.0.1:28334"

# Raw transaction data notifications
rawtx = "tcp://127.0.0.1:28335"

# Sequence notifications
sequence = "tcp://127.0.0.1:28336"
```

All notification types are optional. Only configure the endpoints you need.

### Endpoint Format

ZMQ endpoints use the format: `transport://address:port`

**Supported Transports:**
- `tcp://` - TCP/IP (most common)
- `ipc://` - Inter-process communication (Unix domain sockets)
- `inproc://` - In-process communication (threads)

**Examples:**
- `tcp://127.0.0.1:28332` - Localhost TCP
- `tcp://0.0.0.0:28332` - All interfaces TCP
- `ipc:///tmp/bitcoin-hashblock` - Unix domain socket

## Enabling ZMQ Support

ZMQ support is **enabled by default** in BLLVM node, differentiating it from other Bitcoin implementations. This makes real-time notifications available out of the box.

To use ZMQ notifications:

1. **Configure endpoints** in your node configuration file (see above)

2. **Start the node** - ZMQ publisher will automatically initialize if configured

**Note:** Even though ZMQ is enabled by default, you can disable it in two ways:

1. **Don't configure endpoints** - If no ZMQ endpoints are configured, the ZMQ publisher won't initialize (no performance impact)

2. **Build without ZMQ feature** - To completely exclude ZMQ from the build:
   ```bash
   cargo build --no-default-features --features sysinfo,redb,nix,libc,utxo-commitments,production,governance
   ```

## Subscribing to Notifications

### Python Example

```python
import zmq

# Create ZMQ context and subscriber
context = zmq.Context()
subscriber = context.socket(zmq.SUB)

# Connect to hashblock endpoint
subscriber.connect("tcp://127.0.0.1:28332")

# Subscribe to hashblock topic
subscriber.setsockopt(zmq.SUBSCRIBE, b"hashblock")

# Receive notifications
while True:
    topic = subscriber.recv_string()
    block_hash = subscriber.recv()
    print(f"New block: {block_hash.hex()}")
```

### Rust Example

```rust
use zmq::{Context, SUB};

let ctx = Context::new();
let subscriber = ctx.socket(SUB)?;
subscriber.connect("tcp://127.0.0.1:28332")?;
subscriber.set_subscribe(b"hashblock")?;

loop {
    let topic = subscriber.recv_msg(0)?;
    let block_hash = subscriber.recv_msg(0)?;
    println!("New block: {:?}", block_hash);
}
```

### Node.js Example

```javascript
const zmq = require('zeromq');

// Create subscriber
const subscriber = zmq.socket('sub');

// Connect and subscribe
subscriber.connect('tcp://127.0.0.1:28332');
subscriber.subscribe('hashblock');

// Receive notifications
subscriber.on('message', (topic, blockHash) => {
    console.log('New block:', blockHash.toString('hex'));
});
```

## Integration with Event System

ZMQ notifications are automatically published when:
- **Blocks are connected**: `hashblock` and `rawblock` notifications are sent
- **Transactions enter mempool**: `hashtx`, `rawtx`, and `sequence` notifications are sent
- **Transactions leave mempool**: `sequence` notification is sent (with removal flag)

The ZMQ publisher is integrated with the node's event system, so notifications are published alongside module events.

## Performance Considerations

- **Zero-copy**: ZMQ uses efficient zero-copy message passing
- **Non-blocking**: ZMQ publishing is non-blocking and won't slow down block processing
- **Multiple subscribers**: Multiple applications can subscribe to the same endpoint
- **Fire-and-forget**: Failed ZMQ publishes are logged but don't affect node operation

## Security Considerations

- **Local access only**: By default, bind to `127.0.0.1` for local access only
- **Network access**: If binding to `0.0.0.0`, ensure proper firewall rules
- **No authentication**: ZMQ PUB/SUB sockets don't provide authentication
- **Use IPC**: For local applications, prefer `ipc://` over `tcp://` for better security

## Troubleshooting

### Notifications not received

1. **Check configuration**: Ensure ZMQ endpoints are configured correctly
2. **Check binding**: Ensure the endpoint address is correct
3. **Check subscription**: Verify subscriber is subscribed to the correct topic
4. **Check timing**: Allow time for ZMQ socket binding (100ms+)

**Note:** ZMQ is enabled by default in BLLVM node, so no special build flags are needed.

### Port conflicts

If you see "Address already in use" errors:
- Change the port number in configuration
- Check if another process is using the port
- Use `ipc://` endpoints instead of `tcp://` for local access

### Missing notifications

- Ensure node is processing blocks/transactions
- Check node logs for ZMQ errors
- Verify event publisher is initialized
- Check that ZMQ publisher was successfully created

## Compatibility

BLLVM node's ZMQ notifications are compatible with standard Bitcoin node ZMQ interfaces. Applications that work with Bitcoin Core's ZMQ notifications should work with BLLVM node without modification.

## Future Enhancements

Potential future improvements:
- Wire format serialization for rawblock/rawtx (currently uses bincode)
- Additional notification types
- Authentication support
- Compression options
- Rate limiting

## See Also

- [Event System Documentation](../docs/MODULE_SYSTEM.md)
- [Configuration Reference](../docs/CONFIGURATION.md)
- [ZeroMQ Documentation](https://zeromq.org/)

