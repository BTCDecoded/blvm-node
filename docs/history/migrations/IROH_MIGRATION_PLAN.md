# Migration Plan: iroh-net 0.12 → iroh 0.95

## Overview

Migrate from deprecated `iroh-net 0.12` to the latest `iroh 0.95.1` crate to:
- Fix security vulnerabilities (idna, ring, rsa)
- Use actively maintained codebase
- Benefit from simpler API and latest features

## Current State

### Dependencies
- `iroh-net = { version = "0.12", optional = true }`
- `quinn = { version = "0.10", optional = true }` (matches iroh-net 0.12)
- Feature flag: `iroh = ["dep:iroh-net", "dep:quinn"]`

### Files to Update
1. `Cargo.toml` - dependency and feature definitions
2. `src/network/iroh_transport.rs` - main implementation (327 lines)
3. `src/network/mod.rs` - imports and usage
4. `src/network/address_db.rs` - NodeId usage
5. Any test files using iroh-net APIs

### Current API Usage
```rust
// Current imports
use iroh_net::magic_endpoint::MagicEndpoint;
use iroh_net::key::SecretKey;
use iroh_net::NodeId;
use iroh_net::NodeAddr;

// Current patterns
let secret_key = iroh_net::key::SecretKey::generate();
let endpoint = iroh_net::magic_endpoint::MagicEndpoint::builder()
    .secret_key(secret_key.clone())
    .bind(0)
    .await?;
let node_id = endpoint.node_id();
let node_addr = iroh_net::NodeAddr::from_parts(node_id, None, vec![]);
let conn = self.endpoint.connect(node_addr, alpn).await?;
```

## Target State

### New Dependencies
- `iroh = { version = "0.95", optional = true }`
- Remove `quinn` dependency (handled by iroh internally)
- Feature flag: `iroh = ["dep:iroh"]`

### New API Structure
```rust
// New imports
use iroh::Endpoint;
use iroh::EndpointAddr;
use iroh::PublicKey;  // or SecretKey if still available
use iroh::NodeId;     // if still exists, or use PublicKey

// New patterns
let ep = Endpoint::bind().await?;
let addr: EndpointAddr = /* construct from public key */;
let conn = ep.connect(addr, alpn).await?;
```

## Step-by-Step Migration

### Phase 1: Dependency Update

1. **Update Cargo.toml**
   ```toml
   # Remove:
   iroh-net = { version = "0.12", optional = true }
   quinn = { version = "0.10", optional = true }
   
   # Add:
   iroh = { version = "0.95", optional = true }
   
   # Update feature:
   iroh = ["dep:iroh"]
   ```

2. **Update Cargo.lock**
   ```bash
   cargo update -p iroh-net
   cargo update -p quinn
   cargo update -p iroh
   ```

### Phase 2: API Mapping & Code Updates

#### 2.1 Import Changes

**File: `src/network/iroh_transport.rs`**

```rust
// OLD:
use iroh_net::magic_endpoint::MagicEndpoint;
use iroh_net::key::SecretKey;
use iroh_net::NodeId;
use iroh_net::NodeAddr;

// NEW:
use iroh::Endpoint;
use iroh::EndpointAddr;
use iroh::PublicKey;  // Check if SecretKey still exists or use PublicKey
```

#### 2.2 Struct Updates

**IrohTransport struct:**
```rust
// OLD:
pub struct IrohTransport {
    endpoint: iroh_net::magic_endpoint::MagicEndpoint,
    secret_key: iroh_net::key::SecretKey,
}

// NEW:
pub struct IrohTransport {
    endpoint: iroh::Endpoint,
    // May need to store public key or node ID separately
    // Check if endpoint.node_id() or similar exists
}
```

#### 2.3 Constructor Updates

**IrohTransport::new():**
```rust
// OLD:
pub async fn new() -> Result<Self> {
    let secret_key = iroh_net::key::SecretKey::generate();
    let endpoint = iroh_net::magic_endpoint::MagicEndpoint::builder()
        .secret_key(secret_key.clone())
        .bind(0)
        .await?;
    Ok(Self { endpoint, secret_key })
}

// NEW:
pub async fn new() -> Result<Self> {
    let endpoint = iroh::Endpoint::bind().await?;
    // Store node_id/public_key if needed
    let node_id = endpoint.node_id(); // Check if this method exists
    Ok(Self { endpoint })
}
```

#### 2.4 Connection Updates

**connect() method:**
```rust
// OLD:
let node_addr = iroh_net::NodeAddr::from_parts(
    node_id,
    None,   // No DERP URL
    vec![], // No direct addresses
);
let conn = self.endpoint.connect(node_addr, alpn).await?;

// NEW:
// Need to construct EndpointAddr from public key bytes
// Check iroh 0.95 docs for EndpointAddr construction
let addr: EndpointAddr = /* construct from 32-byte public key */;
let conn = self.endpoint.connect(addr, alpn).await?;
```

#### 2.5 Listener Updates

**IrohListener struct:**
```rust
// OLD:
pub struct IrohListener {
    endpoint: iroh_net::magic_endpoint::MagicEndpoint,
    local_addr: SocketAddr,
}

// NEW:
pub struct IrohListener {
    endpoint: iroh::Endpoint,
    local_addr: SocketAddr,
}
```

**accept() method:**
```rust
// OLD:
let accept_future = self.endpoint.accept();
let accept = accept_future.await
    .ok_or_else(|| anyhow::anyhow!("Accept stream ended"))?;
let conn = accept.await?;

// NEW:
// Check iroh 0.95 API for accepting connections
// Likely: self.endpoint.accept() or similar
let conn = /* check new API */;
```

#### 2.6 Stream Operations

**send() and recv() methods:**
```rust
// OLD:
let mut stream = self.conn.open_uni().await?;
stream.write_all(&len.to_be_bytes()).await?;
stream.write_all(data).await?;
stream.finish().await?;

// NEW:
// Check if quinn::Connection is still used or if iroh has its own stream types
// May need: conn.open_uni() or conn.send() or similar
```

### Phase 3: Type System Updates

1. **NodeId handling**
   - Check if `NodeId` still exists in `iroh 0.95`
   - May need to use `PublicKey` instead
   - Update `TransportAddr::Iroh` to use correct type

2. **Connection type**
   - Check if `quinn::Connection` is still exposed
   - May need to use `iroh::Connection` or similar

3. **Stream types**
   - Check if `quinn::SendStream` and `quinn::RecvStream` are still used
   - May need to use iroh's stream types

### Phase 4: Address Database Updates

**File: `src/network/address_db.rs`**

```rust
// OLD:
use iroh_net::NodeId;

// NEW:
use iroh::PublicKey;  // or whatever replaces NodeId
// Or: use iroh::NodeId; if it still exists
```

### Phase 5: Testing & Validation

1. **Compilation**
   ```bash
   cargo check --features iroh
   ```

2. **Unit tests**
   ```bash
   cargo test --features iroh
   ```

3. **Integration tests**
   - Test connection establishment
   - Test send/receive operations
   - Test listener/accept flow

4. **Manual testing**
   - Test with two nodes
   - Verify QUIC connections work
   - Verify NAT traversal still functions

## API Reference Investigation Needed

Before starting migration, investigate these in iroh 0.95 docs:

1. **Endpoint creation**
   - How to create endpoint with custom secret key?
   - How to get node_id/public_key from endpoint?

2. **EndpointAddr construction**
   - How to create `EndpointAddr` from 32-byte public key?
   - What fields are required?

3. **Connection API**
   - How to connect to a peer?
   - What does `connect()` return?

4. **Accepting connections**
   - How to accept incoming connections?
   - How to get peer's public key from accepted connection?

5. **Stream operations**
   - How to open unidirectional streams?
   - How to read/write data?
   - Are quinn types still used or replaced?

6. **NodeId/PublicKey**
   - Does `NodeId` type still exist?
   - Should we use `PublicKey` instead?
   - How to convert between bytes and NodeId/PublicKey?

## Rollback Plan

If migration fails:

1. Revert Cargo.toml changes
2. Restore original iroh_transport.rs from git
3. Run `cargo update` to restore old dependencies
4. Keep security note in Cargo.toml about vulnerabilities

## Success Criteria

- [ ] Code compiles with `--features iroh`
- [ ] All tests pass
- [ ] No security vulnerabilities in cargo-audit
- [ ] Connection establishment works
- [ ] Send/receive operations work
- [ ] Listener/accept flow works
- [ ] Documentation updated

## Estimated Effort

- **Phase 1 (Dependencies)**: 15 minutes
- **Phase 2 (API Updates)**: 2-4 hours
- **Phase 3 (Type System)**: 1-2 hours
- **Phase 4 (Address DB)**: 30 minutes
- **Phase 5 (Testing)**: 2-3 hours
- **Total**: 6-10 hours

## Resources

- iroh 0.95 documentation: `cargo doc --package iroh --open`
- iroh changelog: https://iroh.computer/changelog
- Migration guide: Check iroh blog posts about 0.29+ changes

## Notes

- The `iroh` feature is optional and not in default features, so migration can be done incrementally
- Keep old code commented out initially for reference
- Test thoroughly since this is a major API change
- Consider creating a feature branch for this migration

