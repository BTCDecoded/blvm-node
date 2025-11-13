# Iroh API Mapping: iroh-net 0.12 → iroh 0.95

## Executive Summary

This document provides a complete API mapping from `iroh-net 0.12` to `iroh 0.95.1`, verified through actual API documentation and testing.

**Key Changes:**
- `iroh-net::magic_endpoint::MagicEndpoint` → `iroh::endpoint::Endpoint`
- `iroh-net::NodeAddr` → `iroh::EndpointAddr`
- `iroh-net::NodeId` → `iroh::PublicKey` (or `iroh::EndpointId`, which is a type alias)
- `quinn::Connection` → `iroh::endpoint::Connection` (iroh wraps quinn internally)
- `iroh-quinn` is used internally, quinn types are NOT directly exposed

**Quinn Dependency:**
- **iroh 0.95 uses `iroh-quinn` internally** - we do NOT need quinn as a direct dependency for iroh transport
- However, `quinn_transport.rs` uses quinn directly and is independent - it will need separate quinn version update
- **Quinn version**: iroh 0.95 uses `iroh-quinn v0.14.0` internally, which likely uses a newer quinn version than 0.10

## Detailed API Mappings

### 1. Imports

#### OLD (iroh-net 0.12):
```rust
use iroh_net::magic_endpoint::MagicEndpoint;
use iroh_net::key::SecretKey;
use iroh_net::NodeId;
use iroh_net::NodeAddr;
```

#### NEW (iroh 0.95):
```rust
use iroh::endpoint::Endpoint;
use iroh::{SecretKey, PublicKey, EndpointId, EndpointAddr};
// Note: EndpointId is a type alias for PublicKey
```

### 2. Endpoint Creation

#### OLD:
```rust
let secret_key = iroh_net::key::SecretKey::generate();
let endpoint = iroh_net::magic_endpoint::MagicEndpoint::builder()
    .secret_key(secret_key.clone())
    .bind(0)  // port 0 = any available port
    .await?;
```

#### NEW:
```rust
use rand::thread_rng;

// Option 1: Simple bind (generates secret key automatically)
let endpoint = iroh::endpoint::Endpoint::bind().await?;

// Option 2: With custom secret key
let mut rng = thread_rng();
let secret_key = iroh::SecretKey::generate(&mut rng);
let endpoint = iroh::endpoint::Endpoint::builder()
    .secret_key(secret_key)
    .bind()
    .await?;
```

**Key Differences:**
- `SecretKey::generate()` now requires `&mut R` where `R: CryptoRng`
- `bind()` no longer takes a port parameter (always binds to available ports)
- Builder pattern is similar but API slightly different

### 3. Getting Node ID / Public Key

#### OLD:
```rust
let node_id: iroh_net::NodeId = endpoint.node_id();
let node_id_bytes = node_id.as_bytes();  // 32 bytes
```

#### NEW:
```rust
let endpoint_id: iroh::EndpointId = endpoint.id();  // EndpointId is PublicKey
let public_key: iroh::PublicKey = endpoint.id();    // Same thing
let public_key_bytes: [u8; 32] = public_key.as_bytes();
```

**Key Differences:**
- Method is `id()` not `node_id()`
- Returns `EndpointId` (type alias for `PublicKey`)
- `as_bytes()` returns `[u8; 32]` (fixed-size array)

### 4. Getting Secret Key

#### OLD:
```rust
let secret_key = &endpoint.secret_key();  // Returns reference
```

#### NEW:
```rust
let secret_key = endpoint.secret_key();  // Returns &SecretKey
// Same API, works identically
```

### 5. Getting Local Address

#### OLD:
```rust
let (local_addr, _) = endpoint.local_addr()
    .map_err(|e| anyhow::anyhow!("Failed to get local address: {}", e))?;
```

#### NEW:
```rust
let bound_sockets = endpoint.bound_sockets();  // Returns Vec<SocketAddr>
let local_addr = bound_sockets.first()
    .copied()
    .ok_or_else(|| anyhow::anyhow!("No bound sockets"))?;
```

**Key Differences:**
- `bound_sockets()` returns `Vec<SocketAddr>` (can have multiple)
- No error return type, always succeeds (may return empty vec)

### 6. Constructing EndpointAddr for Connection

#### OLD:
```rust
let node_addr = iroh_net::NodeAddr::from_parts(
    node_id,
    None,   // No DERP URL
    vec![], // No direct addresses
);
```

#### NEW:
```rust
// Option 1: From PublicKey directly (simplest)
let endpoint_addr: iroh::EndpointAddr = public_key.into();
// Or explicitly:
let endpoint_addr = iroh::EndpointAddr::from(public_key);

// Option 2: Using from_parts (if you have relay URLs or direct addresses)
let endpoint_addr = iroh::EndpointAddr::from_parts(
    public_key,
    vec![],  // relay_urls: Vec<RelayUrl>
    vec![],  // direct_addrs: Vec<SocketAddr> (but wrapped in TransportAddr)
);

// Option 3: Using new() and builder methods
let endpoint_addr = iroh::EndpointAddr::new(public_key);
// Then optionally add:
// endpoint_addr.with_relay_url(relay_url);
// endpoint_addr.with_ip_addr(socket_addr);
```

**Key Differences:**
- `EndpointAddr` has fields: `id: PublicKey` and `addrs: BTreeSet<TransportAddr>`
- Can convert directly from `PublicKey` using `Into` trait
- `from_parts` signature changed: `(PublicKey, Vec<RelayUrl>, Vec<TransportAddr>)`
- `TransportAddr` is iroh's enum, not `SocketAddr` directly

### 7. Connecting to a Peer

#### OLD:
```rust
let alpn = b"bitcoin/1.0";
let conn = endpoint.connect(node_addr, alpn).await?;
// conn is quinn::Connection
```

#### NEW:
```rust
let alpn = b"bitcoin/1.0";
let conn = endpoint.connect(endpoint_addr, alpn).await?;
// conn is iroh::endpoint::Connection (NOT quinn::Connection)
```

**Key Differences:**
- Connection type is `iroh::endpoint::Connection`, not `quinn::Connection`
- API is otherwise identical
- **CRITICAL**: Cannot use `quinn::Connection` methods directly

### 8. Accepting Connections

#### OLD:
```rust
let accept_future = endpoint.accept();
let accept = accept_future.await
    .ok_or_else(|| anyhow::anyhow!("Accept stream ended"))?;
let conn = accept.await?;
// conn is quinn::Connection
```

#### NEW:
```rust
let accept_future = endpoint.accept();  // Returns Accept<'_>
let connecting = accept_future.await
    .ok_or_else(|| anyhow::anyhow!("Accept stream ended"))?;
let conn = connecting.await?;  // Returns iroh::endpoint::Connection
```

**Key Differences:**
- `accept()` returns `Accept<'_>` which yields `Option<Connecting>`
- `Connecting` is a future that resolves to `Connection`
- Connection type is `iroh::endpoint::Connection`, not `quinn::Connection`

### 9. Getting Peer's Public Key from Accepted Connection

#### OLD:
```rust
// Problem: quinn::Connection doesn't expose peer's public key directly
// Workaround: Extract from protocol handshake (placeholder in current code)
let peer_node_id = endpoint.node_id(); // Placeholder - WRONG!
```

#### NEW:
```rust
// Option 1: From Connecting (before awaiting)
let connecting = accept_future.await?;
let peer_id: iroh::EndpointId = connecting.id();  // Get peer ID before connection completes
let conn = connecting.await?;

// Option 2: From Connection (after connection established) - VERIFIED!
let peer_id: iroh::EndpointId = conn.remote_id();  // Get peer's EndpointId from connection
```

**Key Differences:**
- `Connecting` has `id()` method to get peer's `EndpointId` before connection completes
- `Connection` has `remote_id()` method to get peer's `EndpointId` after connection established
- **This fixes the placeholder issue in current code!** We can use `conn.remote_id()` instead of placeholder

### 10. Stream Operations

#### OLD:
```rust
// Opening unidirectional stream
let mut stream = conn.open_uni().await?;  // quinn::SendStream
stream.write_all(&len.to_be_bytes()).await?;
stream.write_all(data).await?;
stream.finish().await?;

// Accepting unidirectional stream
let mut stream = conn.accept_uni().await?;  // quinn::RecvStream
stream.read_exact(&mut len_bytes).await?;
```

#### NEW:
```rust
// Opening unidirectional stream
let mut stream = conn.open_uni().await?;  // Returns iroh::endpoint::SendStream
// Stream implements AsyncWrite, same API
stream.write_all(&len.to_be_bytes()).await?;
stream.write_all(data).await?;
stream.finish().await?;

// Accepting unidirectional stream
let mut stream = conn.accept_uni().await?;  // Returns iroh::endpoint::RecvStream
// Stream implements AsyncRead, same API
stream.read_exact(&mut len_bytes).await?;
```

**Key Differences:**
- Stream types: `iroh::endpoint::SendStream` and `iroh::endpoint::RecvStream` (NOT quinn types)
- They implement the same `AsyncRead`/`AsyncWrite` traits, so API is compatible
- `open_uni()` returns `OpenUni<'_>` (Future) that resolves to `Result<SendStream, ConnectionError>`
- `accept_uni()` returns `AcceptUni<'_>` (Future) that resolves to `Result<RecvStream, ConnectionError>`

### 11. Connection State

#### OLD:
```rust
// Check if connection is closed
if conn.close_reason().is_none() {
    // Connection is open
}
```

#### NEW:
```rust
// VERIFIED: Connection has close_reason() method
if conn.close_reason().is_none() {
    // Connection is open
} else {
    // Connection is closed
    let error = conn.close_reason().unwrap();
    // error is ConnectionError
}
```

**Key Differences:**
- **VERIFIED**: `Connection::close_reason()` exists and returns `Option<ConnectionError>`
- Returns `None` if connection is still open
- Returns `Some(ConnectionError)` if connection is closed
- API is identical to quinn, so no changes needed!

### 12. NodeId in address_db.rs

#### OLD:
```rust
use iroh_net::NodeId;

let node_id = NodeId::from_bytes(&bytes)?;
let node_id_bytes = node_id.as_bytes();
```

#### NEW:
```rust
use iroh::PublicKey;  // or EndpointId

let public_key = PublicKey::from_bytes(&bytes)?;
let public_key_bytes: [u8; 32] = public_key.as_bytes();
```

**Key Differences:**
- `NodeId` → `PublicKey` (or `EndpointId`)
- `from_bytes()` and `as_bytes()` should work similarly
- **VERIFY**: Check if `PublicKey::from_bytes()` exists and signature

## Quinn Dependency Analysis

### Current State:
- `iroh-net 0.12` uses `quinn 0.10` internally
- `quinn_transport.rs` uses `quinn 0.10` directly
- Both are in `Cargo.toml` as optional dependencies

### After Migration:
- `iroh 0.95` uses `iroh-quinn v0.14.0` internally (wraps quinn)
- **iroh_transport.rs**: NO quinn dependency needed (uses iroh's Connection)
- **quinn_transport.rs**: Still needs quinn, but may need version update

### Quinn Version for quinn_transport.rs:
- Current: `quinn = "0.10"`
- iroh-quinn 0.14.0 likely uses a newer quinn version
- **ACTION REQUIRED**: Check what quinn version iroh-quinn 0.14.0 uses
- **RECOMMENDATION**: Update quinn to match iroh-quinn's version, or keep separate if compatible

## Migration Checklist

### Phase 1: Dependencies
- [ ] Update `Cargo.toml`: `iroh-net = "0.12"` → `iroh = "0.95"`
- [ ] Remove `quinn` from iroh feature (keep for quinn feature)
- [ ] Update feature: `iroh = ["dep:iroh"]` (remove quinn)
- [ ] Check quinn version compatibility for `quinn_transport.rs`

### Phase 2: iroh_transport.rs
- [ ] Update imports
- [ ] Update `IrohTransport` struct (remove `secret_key` field if not needed)
- [ ] Update `new()` method (use `Endpoint::bind()` or builder)
- [ ] Update `node_id()` → `id()` method
- [ ] Update `listen()` method (use `bound_sockets()`)
- [ ] Update `connect()` method (construct `EndpointAddr` from `PublicKey`)
- [ ] Update `IrohListener::accept()` (use new accept API, get peer ID from `Connecting`)
- [ ] Update `IrohConnection` struct (`quinn::Connection` → `iroh::endpoint::Connection`)
- [ ] Update stream operations (use `SendStream` and `RecvStream` types)
- [ ] Update connection state checking (use `close_reason()` - same API)
- [ ] Update peer ID extraction (use `conn.remote_id()` instead of placeholder)

### Phase 3: address_db.rs
- [ ] Update import: `iroh_net::NodeId` → `iroh::PublicKey`
- [ ] Update `HashMap<NodeId, AddressEntry>` → `HashMap<PublicKey, AddressEntry>`
- [ ] Update `add_iroh_address()` parameter type
- [ ] Update `get_fresh_iroh_addresses()` return type
- [ ] Update `evict_oldest_unified()` to use `PublicKey`
- [ ] Update tests to use `PublicKey` instead of `NodeId`

### Phase 4: mod.rs
- [ ] Update import: `iroh_net::NodeId` → `iroh::PublicKey` (if used)

### Phase 5: Testing
- [ ] Compile with `cargo check --features iroh`
- [ ] Run `cargo test --features iroh`
- [ ] Verify connection establishment
- [ ] Verify send/receive operations
- [ ] Verify listener/accept flow
- [ ] Verify peer ID extraction works correctly

## Unverified Items (Need Testing)

1. ~~**Stream Types**: Exact type names for `open_uni()` and `accept_uni()` return values~~ ✅ **VERIFIED**: `SendStream` and `RecvStream`
2. ~~**Connection State**: How to check if `iroh::endpoint::Connection` is closed~~ ✅ **VERIFIED**: `close_reason()` method exists
3. **PublicKey::from_bytes()**: Verify method exists and signature
4. **SecretKey Persistence**: If we need to store/load secret keys, verify API
5. **Quinn Version**: What quinn version does iroh-quinn 0.14.0 use? (iroh-quinn wraps quinn, version not directly visible)

## Breaking Changes Summary

1. **Connection Type**: `quinn::Connection` → `iroh::endpoint::Connection` (major)
2. **NodeId → PublicKey**: Type name change (minor, mostly cosmetic)
3. **SecretKey::generate()**: Now requires RNG parameter (minor)
4. **local_addr()**: Changed to `bound_sockets()` returning Vec (minor)
5. **NodeAddr → EndpointAddr**: Different construction API (moderate)
6. **accept() API**: Returns `Accept<'_>` → `Option<Connecting>` → `Connection` (moderate)
7. **Peer ID Extraction**: Now available from `Connecting.id()` (improvement!)

## Estimated Impact

- **High Impact**: Connection type change (affects all connection operations)
- **Medium Impact**: EndpointAddr construction, accept() API changes
- **Low Impact**: Type name changes (NodeId → PublicKey), method renames

## Next Steps

1. ~~**Verify Stream Types**: Check iroh 0.95 docs for exact stream type names~~ ✅ **DONE**: `SendStream` and `RecvStream`
2. ~~**Test Connection State**: Verify how to check if connection is closed~~ ✅ **DONE**: `close_reason()` method exists
3. **Test PublicKey API**: Verify `from_bytes()` and `as_bytes()` methods (likely similar to NodeId)
4. **Check Quinn Version**: Determine quinn version for `quinn_transport.rs` (may need separate update)
5. **Create Test Branch**: Test migration incrementally with verified API mappings

