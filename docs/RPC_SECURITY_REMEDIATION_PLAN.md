# RPC & External Endpoint Security Remediation Plan

This document outlines the plan to resolve security gaps identified in the RPC and external-facing endpoint audit (March 2025).

## Scope and Context

**Scope:** This plan applies to **blvm-node** only. All modifications are within `blvm-node/src/` (rpc, network, config).

**Post-reorganization context:** A fundamental reorganization of blvm-consensus, blvm-protocol, and blvm-primitives was completed (see `docs/CONSENSUS_IMPLEMENTATION_GROUPINGS.md`). That reorganization:
- Created **blvm-primitives** (types, constants, serialization, crypto, config, etc.)
- Moved **utxo_commitments** and **spam_filter** to blvm-protocol
- Deleted **blvm-consensus::network** (now in blvm-protocol)

**blvm-node** is a consumer of blvm-protocol and blvm-consensus. Its own modules (`rpc`, `network`, `config`) were **not** part of that reorganization. The RPC remediation plan operates entirely within blvm-node's existing structure. No changes to blvm-consensus, blvm-protocol, or blvm-primitives are required for this plan.

---

## Validation Status (March 2025)

Plan validated against codebase (final pass). Corrections and refinements applied below.

| Item | Validation Result |
|------|-------------------|
| P1 Rate limiting | **Verified:** When `auth_required=false`, `authenticate_request` returns `UserId::Ip(addr)`. RPC then calls `check_rate_limit_with_endpoint(user_id)` which uses `default_rate_limit`. Create auth_manager with `with_rate_limits(false, ip_burst, ip_rate)` for rate-limit-only mode. |
| P1 Quinn | **Verified:** `QuinnRpcServer::new(addr)` takes only addr. `connection.remote_address()` available for client addr. No auth/rate-limit params. |
| P2 Batch | **Verified:** `serde_json::from_str` on batch `[{...}]` yields array; `req.get("method")` is `None`; `method_name` = `"unknown"`. |
| P2 Connection | **Verified:** `ConnectionRateLimiter` is `pub` in `crate::network::dos_protection`. RPC can use `crate::network::dos_protection::ConnectionRateLimiter`. Config uses `[rpc]` and `[rpc_auth]` sections. |
| P3 REST vault/pool | **Verified:** Three call sites at lines 346, 382, 418 use `read_json_body(req).await?`. `handle_request` returns `Result<Response, hyper::Error>`. `?` on `Result<_, String>` requires `From<String> for hyper::Error` (does not exist). Payments (lines 294–306) uses correct `match` pattern. |
| Config structure | **Corrected:** `[rpc]` for max_request_size, connection limits; `[rpc_auth]` for rate_limit_when_auth_disabled, ip_rate_limit_*. |
| Test file names | **Verified:** `rpc_auth_security_tests.rs`, `rpc_auth_comprehensive_tests.rs`, `integration/rpc_auth_tests.rs` exist. |

---

## Overview

| Priority | Issue | Effort | Dependencies |
|----------|-------|--------|--------------|
| P1 | Rate limiting when auth disabled | Medium | None |
| P1 | Quinn RPC auth/rate limiting | Medium | P1 (shared infra) |
| P2 | Batch RPC rate limiting | Medium | P1 |
| P2 | RPC connection rate limiting | Low | None |
| P3 | REST vault/pool error handling | Low | None |

---

## P1: Rate Limiting When Auth Disabled

### Problem

Both RPC (HTTP) and REST apply rate limiting only when `auth_manager` is `Some`. When auth is disabled (common for local/dev), there is no protection against request flooding.

### Solution

**Simplified approach:** Reuse `RpcAuthManager` for rate limiting. When auth is disabled but rate limiting is desired, create an `RpcAuthManager` with `auth_required=false` and no tokens. `authenticate_request` will return `UserId::Ip(addr)` for unauthenticated requests, and `check_ip_rate_limit_with_endpoint` will apply. No new type needed.

### Implementation

1. **RpcManager** (`src/rpc/mod.rs`):
   - When `rate_limit_when_auth_disabled=true` and `auth_manager` is `None`, create `RpcAuthManager::with_rate_limits(false, ip_burst, ip_rate)` (no tokens, auth not required).
   - Pass this to RPC server, REST server, and Quinn (when implemented).
   - Result: `auth_manager` is always `Some` when rate limiting is enabled.

2. **RPC server** / **REST server**:
   - No change to rate-limit logic—it already runs when `auth_manager` is `Some`.
   - When `auth_required=false`, `authenticate_request` returns `UserId::Ip(addr)`; rate limit applies.

3. **Config**:
   - Add `rpc_auth.rate_limit_when_auth_disabled: bool` (default: `true`).
   - Add `rpc_auth.ip_rate_limit_burst` and `rpc_auth.ip_rate_limit_rate` for unauthenticated defaults (or reuse existing `rate_limit_burst`/`rate_limit_rate` with different values when auth disabled).

### Files to Modify

- `src/rpc/mod.rs` — create minimal auth_manager when auth disabled + rate_limit_when_auth_disabled
- `src/config/mod.rs` — add `rate_limit_when_auth_disabled`, optional `ip_rate_limit_*` to RpcAuthConfig

### Tests

- `tests/rpc_auth_security_tests.rs` or `tests/rpc_auth_comprehensive_tests.rs` — add `test_rpc_rate_limiting_without_auth`
- `tests/integration/rpc_auth_tests.rs` — verify IP limit applies when auth disabled

---

## P1: Quinn RPC Auth and Rate Limiting

### Problem

Quinn RPC calls `RpcServer::process_request()` directly, bypassing HTTP layer and thus auth and rate limiting.

### Solution

Route Quinn requests through the same security checks as HTTP RPC. Quinn handler needs access to:
- Client address (from `connection.remote_address()`)
- Auth manager (if configured)
- Rate limit manager

### Implementation

1. **Quinn server signature change**:
   - `QuinnRpcServer` must hold `auth_manager: Option<Arc<RpcAuthManager>>` and `rate_limit_manager` (or equivalent).
   - Pass these from `RpcManager` when starting Quinn.

2. **Request handling**:
   - Before `process_request`, run:
     - Auth (if required): reject with JSON-RPC error if auth fails.
     - Rate limit: reject with JSON-RPC error if limit exceeded.
   - Parse request to extract `method` for rate limiting; for batch, use `"batch"` or apply per-call limits (see P2).

3. **RpcManager wiring** (`src/rpc/mod.rs`):
   - When spawning Quinn server, pass `auth_manager` and rate limit config.
   - Quinn server is created in `start()` — ensure it receives the same auth/rate-limit setup as HTTP RPC.

### Files to Modify

- `src/rpc/quinn_server.rs` — add auth/rate limit checks, accept auth_manager
- `src/rpc/mod.rs` — pass auth_manager to QuinnRpcServer

### Tests

- `tests/quic_rpc_smoke_tests.rs` — add test for rate limit rejection
- Add `tests/quic_rpc_auth_tests.rs` if auth is used with Quinn

### Documentation

- Update `docs/QUIC_RPC.md` and `docs/QUINN_INTEGRATION.md` to state that Quinn RPC uses the same auth/rate-limit model as HTTP RPC.

---

## P2: Batch RPC Rate Limiting

### Problem

For batch requests `[{...}, {...}]`, `method_name` resolves to `"unknown"`, so per-method rate limits do not apply. One batch can contain many expensive calls.

### Solution

**Option A (simpler):** Treat batch as a single request but apply a stricter global limit for batch (e.g. count batch as N requests for rate limit, where N = min(batch_len, 10) or similar).

**Option B (stricter):** For batch, iterate each sub-request and check per-method limit for each. If any exceeds, reject entire batch.

Recommendation: **Option A** — multiply batch size (capped) for rate limit consumption. E.g. `tokens_consumed = min(batch_len, 10)` so a 100-request batch consumes 10 tokens.

### Implementation

1. **Detect batch before rate limit**:
   - Parse body; if `request.as_array().is_some()`, it's a batch.
   - For batch: use `batch_rate_multiplier = min(requests.len(), 10)` for rate limit.

2. **Auth manager / rate limiter**:
   - Add `check_and_consume_n(&self, n: u32)` to `RpcRateLimiter` for batch.
   - Or call `check_and_consume` N times (simpler but may be inconsistent with token bucket).

3. **RPC server**:
   - When `method_name == "unknown"` and body is batch, compute batch size and apply batch multiplier before `check_rate_limit` / `check_method_rate_limit`.

### Files to Modify

- `src/rpc/auth_impl.rs` — add batch-aware rate limit (e.g. `consume_n` or equivalent)
- `src/rpc/server.rs` — detect batch, apply batch rate limit before processing

### Tests

- Add `test_batch_rate_limiting` — send batch of 20 requests, verify rate limit kicks in

---

## P2: RPC Connection Rate Limiting

### Problem

RPC server accepts connections without limit. Each connection spawns a task. An attacker can exhaust file descriptors or memory via connection flood.

### Solution

Add a **connection rate limiter** at the RPC listener level: per-IP connection attempts per time window (e.g. 10 connections per minute per IP). Reject or delay new connections when limit exceeded.

### Implementation

1. **Reuse or mirror `ConnectionRateLimiter`** from `src/network/dos_protection.rs`:
   - Create `RpcConnectionRateLimiter` or use `ConnectionRateLimiter` with RPC-specific config.
   - Check on `listener.accept()` — before spawning the task, check if client IP is within limit.

2. **Config**:
   - `rpc.max_connections_per_ip_per_minute: u32` (default: 10)
   - Or reuse `dos_protection` config if RPC shares that component.

3. **RPC server** (`server.rs`) and **REST server** (`rest/server.rs`):
   - Before `tokio::spawn`, call `connection_limiter.check_connection(addr.ip())`.
   - If false, close connection immediately (or send 503 and close).
   - Both servers have separate listeners; apply to both.

4. **ConnectionRateLimiter** is already `pub` in `crate::network::dos_protection`. Use `Arc<Mutex<ConnectionRateLimiter>>` with RPC-specific config (e.g. 10/min, 60s window). RPC module can depend on `crate::network::dos_protection`.

### Files to Modify

- `src/rpc/server.rs` — add connection check in accept loop
- `src/rpc/rest/server.rs` — add connection check in accept loop
- `src/rpc/mod.rs` — create and pass connection limiter to both servers
- `src/config/mod.rs` — RPC connection limit config

### Tests

- Integration test: open 15 connections from same IP in quick succession, verify some are rejected.

### Documentation

- SECURITY.md: note that RPC has connection rate limiting; for production, recommend reverse proxy (nginx/Caddy) for additional protection.

---

## P3: REST Vault/Pool/Congestion Error Handling

### Problem

Vault, pool, and congestion handlers use `read_json_body(req).await?`. The `?` requires `From<String> for hyper::Error`, which does not exist. `handle_request` returns `Result<Response, hyper::Error>`, so this does not compile (or would fail if it did). The payments handler uses the correct pattern: `match read_json_body(req).await { Ok(opt) => opt, Err(e) => return Ok(error_response(...)) }`.

### Solution

Replace `?` with explicit error handling at all three call sites (vault, pool, congestion) in `src/rpc/rest/server.rs`, matching the payments pattern:

```rust
let body = match crate::rpc::rest::types::read_json_body(req).await {
    Ok(opt) => opt,
    Err(e) => {
        return Ok(Self::error_response_with_headers(
            security_headers,
            StatusCode::PAYLOAD_TOO_LARGE,
            "PAYLOAD_TOO_LARGE",
            &e,
            None,
            request_id,
        ));
    }
};
```

### Files to Modify

- `src/rpc/rest/server.rs` — three call sites (vault ~line 346, pool ~line 382, congestion ~line 418)

### Tests

- Send oversized JSON body to vault/pool/congestion endpoint, verify 413 response.

---

## Implementation Order

1. **Phase 1 (P1):** Rate limiting when auth disabled + Quinn protection  
   - Unblocks consistent behavior across all RPC surfaces.

2. **Phase 2 (P2):** Batch rate limiting + connection rate limiting  
   - Hardens against batch and connection floods.

3. **Phase 3 (P3):** REST error handling audit and fix  
   - Cleanup and correctness.

---

## Config Additions (Summary)

Config uses separate `[rpc]` and `[rpc_auth]` sections. Add:

```toml
[rpc]
# Existing
max_request_size_bytes = 1048576

# New
max_connections_per_ip_per_minute = 10
batch_rate_multiplier_cap = 10
connection_rate_limit_window_seconds = 60

[rpc_auth]
# Existing
required = false
rate_limit_burst = 100
rate_limit_rate = 10

# New
rate_limit_when_auth_disabled = true
ip_rate_limit_burst = 50
ip_rate_limit_rate = 5
```

## Environment Variable Overrides

All RPC security settings can be overridden via environment variables (ENV takes precedence over config):

| ENV Variable | Type | Overrides | Default |
|--------------|------|-----------|---------|
| `BLVM_RPC_MAX_REQUEST_SIZE_BYTES` | usize | rpc.max_request_size_bytes | 1048576 |
| `BLVM_RPC_MAX_CONNECTIONS_PER_IP_PER_MINUTE` | u32 | rpc.max_connections_per_ip_per_minute | 10 |
| `BLVM_RPC_RATE_LIMIT_WHEN_AUTH_DISABLED` | bool | rpc.rate_limit_when_auth_disabled | true |
| `BLVM_RPC_IP_RATE_LIMIT_BURST` | u32 | rpc.ip_rate_limit_burst | 50 |
| `BLVM_RPC_IP_RATE_LIMIT_RATE` | u32 | rpc.ip_rate_limit_rate | 5 |
| `BLVM_RPC_BATCH_RATE_MULTIPLIER_CAP` | u32 | rpc.batch_rate_multiplier_cap | 10 |
| `BLVM_RPC_CONNECTION_RATE_LIMIT_WINDOW_SECS` | u64 | rpc.connection_rate_limit_window_seconds | 60 |

Bool values: `true`/`1`/`yes`/`on` = true; anything else = false.

---

## Verification Checklist

After implementation:

- [x] RPC without auth: IP rate limit applies (test_rpc_rate_limiting_without_auth)
- [x] REST without auth: IP rate limit applies (test_rest_api_ip_rate_limiting)
- [x] Quinn RPC: auth and rate limit apply when configured (quic_rpc_rate_limit_rejection)
- [x] Batch RPC: rate limit accounts for batch size (test_batch_rate_limiting)
- [x] Connection flood: excess connections rejected (test_connection_rate_limiter_flood_rejection)
- [x] REST vault/pool: oversized body returns 413 (test_rest_oversized_body_returns_error)
- [ ] All existing tests pass (run `cargo test -p blvm-node`)
- [x] SECURITY.md updated with new mitigations

---

## References

- Audit findings: RPC & External Endpoint Security Audit (March 2025)
- `SECURITY.md` — threat model and hardening roadmap
- `src/rpc/auth_impl.rs` — current rate limit implementation
- `src/network/dos_protection.rs` — connection rate limiter pattern
- `docs/CONSENSUS_IMPLEMENTATION_GROUPINGS.md` — consensus/protocol/primitives reorganization (blvm-node unchanged)
