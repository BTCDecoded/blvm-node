//! RPC Authentication Manager Implementation
//!
//! This file contains the full RpcAuthManager implementation with constant-time token comparison.

use crate::utils::current_timestamp;
use anyhow::Result;
use constant_time_eq::constant_time_eq;
use hyper::HeaderMap;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// Authentication token (simple string-based for now)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AuthToken(String);

impl AuthToken {
    pub fn new(token: String) -> Self {
        Self(token)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Authenticated user identifier
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UserId {
    /// Token-based user (identified by token)
    Token(AuthToken),
    /// Certificate-based user (identified by certificate fingerprint)
    Certificate(String),
    /// IP-based user (for unauthenticated requests, rate limited by IP)
    Ip(SocketAddr),
}

/// Authentication result
#[derive(Debug, Clone)]
pub struct AuthResult {
    /// User identifier if authenticated
    pub user_id: Option<UserId>,
    /// Whether authentication is required
    pub requires_auth: bool,
    /// Error message if authentication failed
    pub error: Option<String>,
}

/// Token bucket rate limiter for RPC requests
pub struct RpcRateLimiter {
    /// Current number of tokens available
    tokens: u32,
    /// Maximum burst size (initial token count)
    burst_limit: u32,
    /// Tokens per second refill rate
    rate: u32,
    /// Last refill timestamp (Unix seconds)
    last_refill: u64,
}

impl RpcRateLimiter {
    /// Create a new rate limiter
    pub fn new(burst_limit: u32, rate: u32) -> Self {
        let now = current_timestamp();
        Self {
            tokens: burst_limit,
            burst_limit,
            rate,
            last_refill: now,
        }
    }

    /// Check if a request is allowed and consume a token
    pub fn check_and_consume(&mut self) -> bool {
        self.check_and_consume_n(1)
    }

    /// Check if n requests are allowed and consume that many tokens (for batch rate limiting).
    /// Returns true only if at least n tokens were available and consumed.
    /// If n exceeds the burst limit, always returns false — no refill can ever satisfy it.
    pub fn check_and_consume_n(&mut self, n: u32) -> bool {
        // A batch larger than the burst limit can never be served; reject immediately.
        if n > self.burst_limit {
            return false;
        }

        let now = current_timestamp();

        // Refill tokens based on elapsed time
        let elapsed = now.saturating_sub(self.last_refill);
        if elapsed > 0 {
            let tokens_to_add = (elapsed as u32).saturating_mul(self.rate);
            self.tokens = self
                .tokens
                .saturating_add(tokens_to_add)
                .min(self.burst_limit);
            self.last_refill = now;
        }

        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }

    /// Get current token count (for monitoring)
    pub fn tokens_remaining(&self) -> u32 {
        self.tokens
    }

    /// Last refill timestamp (for stale limiter eviction)
    pub fn last_refill(&self) -> u64 {
        self.last_refill
    }
}

/// RPC authentication manager
pub struct RpcAuthManager {
    /// Valid authentication tokens
    valid_tokens: Arc<Mutex<HashMap<String, UserId>>>,
    /// Certificate fingerprints (for certificate-based auth)
    valid_certificates: Arc<Mutex<HashMap<String, UserId>>>,
    /// Whether authentication is required
    auth_required: bool,
    /// Rate limiters per user
    rate_limiters: Arc<Mutex<HashMap<UserId, RpcRateLimiter>>>,
    /// Default rate limit (burst, rate per second)
    default_rate_limit: (u32, u32),
    /// Per-user rate limits (overrides default)
    user_rate_limits: Arc<Mutex<HashMap<UserId, (u32, u32)>>>,
    /// Per-method rate limits (method_name -> (burst, rate))
    method_rate_limits: Arc<Mutex<HashMap<String, (u32, u32)>>>,
    /// Per-method rate limiters (global, one per method)
    method_rate_limiters: Arc<Mutex<HashMap<String, RpcRateLimiter>>>,
    /// Authentication failure tracker for DoS protection
    auth_failure_tracker: AuthFailureTracker,
    /// Tokens granted admin (destructive-method) access.
    /// When empty AND `all_tokens_are_admin` is false (the default), no token is admin.
    /// Set `all_tokens_are_admin = true` only for single-operator deployments that pre-date RBAC.
    admin_tokens: Arc<Mutex<HashSet<String>>>,
    /// When `true`, every authenticated token is treated as admin regardless of `admin_tokens`.
    /// Defaults to `false`. Prefer explicit admin token registration instead.
    all_tokens_are_admin: bool,
    /// Optional HTTP Basic credentials (Bitcoin Core / ckpool `rpcuser` + `rpcpassword`).
    basic_auth_user: Arc<Mutex<Option<String>>>,
    basic_auth_password: Arc<Mutex<Option<String>>>,
}

impl RpcAuthManager {
    /// Create a new authentication manager
    pub fn new(auth_required: bool) -> Self {
        Self {
            valid_tokens: Arc::new(Mutex::new(HashMap::new())),
            valid_certificates: Arc::new(Mutex::new(HashMap::new())),
            auth_required,
            rate_limiters: Arc::new(Mutex::new(HashMap::new())),
            default_rate_limit: (100, 10), // 100 burst, 10 req/sec
            user_rate_limits: Arc::new(Mutex::new(HashMap::new())),
            method_rate_limits: Arc::new(Mutex::new(HashMap::new())),
            method_rate_limiters: Arc::new(Mutex::new(HashMap::new())),
            auth_failure_tracker: AuthFailureTracker::new(),
            admin_tokens: Arc::new(Mutex::new(HashSet::new())),
            all_tokens_are_admin: false,
            basic_auth_user: Arc::new(Mutex::new(None)),
            basic_auth_password: Arc::new(Mutex::new(None)),
        }
    }

    /// Create with custom rate limits
    pub fn with_rate_limits(auth_required: bool, default_burst: u32, default_rate: u32) -> Self {
        Self {
            valid_tokens: Arc::new(Mutex::new(HashMap::new())),
            valid_certificates: Arc::new(Mutex::new(HashMap::new())),
            auth_required,
            rate_limiters: Arc::new(Mutex::new(HashMap::new())),
            default_rate_limit: (default_burst, default_rate),
            user_rate_limits: Arc::new(Mutex::new(HashMap::new())),
            method_rate_limits: Arc::new(Mutex::new(HashMap::new())),
            method_rate_limiters: Arc::new(Mutex::new(HashMap::new())),
            auth_failure_tracker: AuthFailureTracker::new(),
            admin_tokens: Arc::new(Mutex::new(HashSet::new())),
            all_tokens_are_admin: false,
            basic_auth_user: Arc::new(Mutex::new(None)),
            basic_auth_password: Arc::new(Mutex::new(None)),
        }
    }

    /// Configure Bitcoin Core-style HTTP Basic credentials (ckpool, legacy tools).
    pub async fn set_basic_auth(&self, username: Option<String>, password: String) {
        *self.basic_auth_user.lock().await = username;
        *self.basic_auth_password.lock().await = Some(password);
    }

    /// Grant every authenticated token admin privileges, regardless of `admin_tokens`.
    /// Use only for single-operator setups upgrading from pre-RBAC versions.
    /// Prefer `add_admin_token` for production deployments.
    pub fn with_all_tokens_admin(mut self, enabled: bool) -> Self {
        self.all_tokens_are_admin = enabled;
        self
    }

    /// Returns true when RPC clients must authenticate (no anonymous access).
    #[must_use]
    pub fn is_authentication_required(&self) -> bool {
        self.auth_required
    }

    /// Add a valid authentication token
    pub async fn add_token(&self, token: String) -> Result<()> {
        let user_id = UserId::Token(AuthToken::new(token.clone()));
        let mut tokens = self.valid_tokens.lock().await;
        tokens.insert(token, user_id.clone());

        // Initialize rate limiter for this user
        let (burst, rate) = self.get_rate_limit_for_user(&user_id).await;
        let mut limiters = self.rate_limiters.lock().await;
        limiters.insert(user_id, RpcRateLimiter::new(burst, rate));

        Ok(())
    }

    /// Add a token with admin (destructive-method) privileges.
    /// Also registers it as a valid auth token.
    pub async fn add_admin_token(&self, token: String) -> Result<()> {
        self.add_token(token.clone()).await?;
        self.admin_tokens.lock().await.insert(token);
        Ok(())
    }

    /// Returns `true` when `user_id` may call admin-only RPC methods.
    ///
    /// The rule: if `all_tokens_are_admin` is set, every authenticated user is admin.
    /// Otherwise only tokens explicitly registered via `add_admin_token` are admin.
    /// An empty `admin_tokens` set with `all_tokens_are_admin = false` (the default)
    /// means **no token is admin** — configure admin tokens or set `all_tokens_are_admin`.
    pub async fn is_user_admin(&self, user_id: &UserId) -> bool {
        if self.all_tokens_are_admin {
            return true;
        }
        let admins = self.admin_tokens.lock().await;
        if let UserId::Token(tok) = user_id {
            admins.contains(tok.as_str())
        } else {
            false
        }
    }

    /// Remove an authentication token
    pub async fn remove_token(&self, token: &str) -> Result<()> {
        let mut tokens = self.valid_tokens.lock().await;
        if let Some(user_id) = tokens.remove(token) {
            let mut limiters = self.rate_limiters.lock().await;
            limiters.remove(&user_id);
        }
        self.admin_tokens.lock().await.remove(token);
        Ok(())
    }

    /// Add a valid certificate fingerprint
    pub async fn add_certificate(&self, fingerprint: String) -> Result<()> {
        let user_id = UserId::Certificate(fingerprint.clone());
        let mut certs = self.valid_certificates.lock().await;
        certs.insert(fingerprint, user_id.clone());

        // Initialize rate limiter for this user
        let (burst, rate) = self.get_rate_limit_for_user(&user_id).await;
        let mut limiters = self.rate_limiters.lock().await;
        limiters.insert(user_id, RpcRateLimiter::new(burst, rate));

        Ok(())
    }

    /// Remove a certificate fingerprint
    pub async fn remove_certificate(&self, fingerprint: &str) -> Result<()> {
        let mut certs = self.valid_certificates.lock().await;
        if let Some(user_id) = certs.remove(fingerprint) {
            let mut limiters = self.rate_limiters.lock().await;
            limiters.remove(&user_id);
        }
        Ok(())
    }

    /// Set rate limit for a specific user
    pub async fn set_user_rate_limit(&self, user_id: &UserId, burst: u32, rate: u32) {
        let mut limits = self.user_rate_limits.lock().await;
        limits.insert(user_id.clone(), (burst, rate));

        // Update existing rate limiter if present
        let mut limiters = self.rate_limiters.lock().await;
        if let Some(limiter) = limiters.get_mut(user_id) {
            *limiter = RpcRateLimiter::new(burst, rate);
        }
    }

    /// Set rate limit for a specific RPC method (e.g. stricter for getrawmempool)
    pub async fn set_method_rate_limit(&self, method_name: &str, burst: u32, rate: u32) {
        let mut limits = self.method_rate_limits.lock().await;
        limits.insert(method_name.to_string(), (burst, rate));

        let mut limiters = self.method_rate_limiters.lock().await;
        limiters.insert(method_name.to_string(), RpcRateLimiter::new(burst, rate));
    }

    /// Get rate limit for a user (checks per-user limits first)
    async fn get_rate_limit_for_user(&self, user_id: &UserId) -> (u32, u32) {
        let limits = self.user_rate_limits.lock().await;
        limits
            .get(user_id)
            .copied()
            .unwrap_or(self.default_rate_limit)
    }

    /// Authenticate a request from HTTP headers
    /// Uses constant-time comparison for token validation to prevent timing attacks
    pub async fn authenticate_request(
        &self,
        headers: &HeaderMap,
        client_addr: SocketAddr,
    ) -> AuthResult {
        if self.auth_failure_tracker.is_blocked(client_addr).await {
            SecurityEvent::RepeatedAuthFailures {
                client_addr,
                failure_count: self.auth_failure_tracker.failure_count(client_addr).await,
                time_window_seconds: self.auth_failure_tracker.time_window_seconds(),
            }
            .log();
            return AuthResult {
                user_id: None,
                requires_auth: self.auth_required,
                error: Some("Too many authentication failures; try again later".to_string()),
            };
        }

        // Try token-based authentication first
        if let Some(auth_header) = headers.get("authorization") {
            if let Ok(auth_str) = auth_header.to_str() {
                // Support "Bearer <token>" format
                if let Some(token) = auth_str.strip_prefix("Bearer ") {
                    let tokens = self.valid_tokens.lock().await;

                    // Use constant-time comparison to prevent timing attacks
                    // Iterate through all tokens and compare using constant_time_eq
                    // This ensures timing is always O(n) regardless of which token matches
                    let mut matched_user_id = None;
                    for (stored_token, user_id) in tokens.iter() {
                        // Constant-time string comparison
                        if constant_time_eq(stored_token.as_bytes(), token.as_bytes()) {
                            matched_user_id = Some(user_id.clone());
                            break;
                        }
                    }

                    if let Some(user_id) = matched_user_id {
                        debug!("Token authentication successful for {}", client_addr);
                        SecurityEvent::AuthSuccess {
                            user_id: format!("{user_id:?}"),
                            client_addr,
                            auth_method: "token".to_string(),
                        }
                        .log();
                        return AuthResult {
                            user_id: Some(user_id),
                            requires_auth: self.auth_required,
                            error: None,
                        };
                    } else {
                        self.auth_failure_tracker.record_failure(client_addr).await;
                        SecurityEvent::AuthFailure {
                            client_addr,
                            reason: "Invalid authentication token".to_string(),
                        }
                        .log();
                        return AuthResult {
                            user_id: None,
                            requires_auth: self.auth_required,
                            error: Some("Invalid authentication token".to_string()),
                        };
                    }
                } else if let Some(b64) = auth_str.strip_prefix("Basic ") {
                    if let Some((user, pass)) = decode_http_basic_credentials(b64.trim()) {
                        let expected_user = self.basic_auth_user.lock().await.clone();
                        let expected_pass = self.basic_auth_password.lock().await.clone();
                        if let Some(ref expected_password) = expected_pass {
                            let user_ok = expected_user
                                .as_ref()
                                .map(|u| constant_time_eq(u.as_bytes(), user.as_bytes()))
                                .unwrap_or(true);
                            if user_ok
                                && constant_time_eq(expected_password.as_bytes(), pass.as_bytes())
                            {
                                let token = AuthToken::new(pass.clone());
                                let user_id = UserId::Token(token);
                                debug!("HTTP Basic authentication successful for {}", client_addr);
                                SecurityEvent::AuthSuccess {
                                    user_id: format!("{user_id:?}"),
                                    client_addr,
                                    auth_method: "basic".to_string(),
                                }
                                .log();
                                return AuthResult {
                                    user_id: Some(user_id),
                                    requires_auth: self.auth_required,
                                    error: None,
                                };
                            }
                        }
                    }

                    self.auth_failure_tracker.record_failure(client_addr).await;
                    SecurityEvent::AuthFailure {
                        client_addr,
                        reason: "Invalid HTTP Basic credentials".to_string(),
                    }
                    .log();
                    return AuthResult {
                        user_id: None,
                        requires_auth: self.auth_required,
                        error: Some("Invalid authentication credentials".to_string()),
                    };
                } else {
                    self.auth_failure_tracker.record_failure(client_addr).await;
                    SecurityEvent::AuthFailure {
                        client_addr,
                        reason: "Invalid authentication token".to_string(),
                    }
                    .log();
                    return AuthResult {
                        user_id: None,
                        requires_auth: self.auth_required,
                        error: Some("Invalid authentication token".to_string()),
                    };
                }
            }
        }

        // Certificate-based authentication via TLS-proxy header.
        // SECURITY: Only trust this header when the direct TCP connection is from the loopback
        // interface. A remote peer could forge the header otherwise — this guard ensures only
        // a local TLS-terminating proxy (127.0.0.1 / ::1) can inject the fingerprint.
        let is_loopback = client_addr.ip().is_loopback();
        if is_loopback {
            if let Some(cert_header) = headers.get("x-client-cert-fingerprint") {
                if let Ok(fingerprint) = cert_header.to_str() {
                    let certs = self.valid_certificates.lock().await;

                    // Use constant-time comparison for certificate fingerprints
                    let mut matched_user_id = None;
                    for (stored_fingerprint, user_id) in certs.iter() {
                        if constant_time_eq(stored_fingerprint.as_bytes(), fingerprint.as_bytes()) {
                            matched_user_id = Some(user_id.clone());
                            break;
                        }
                    }

                    if let Some(user_id) = matched_user_id {
                        debug!("Certificate authentication successful for {}", client_addr);
                        SecurityEvent::AuthSuccess {
                            user_id: format!("{user_id:?}"),
                            client_addr,
                            auth_method: "certificate".to_string(),
                        }
                        .log();
                        return AuthResult {
                            user_id: Some(user_id),
                            requires_auth: self.auth_required,
                            error: None,
                        };
                    }
                }
            }
        } // end is_loopback guard

        // If authentication is required but not provided, reject
        if self.auth_required {
            // Record authentication failure
            self.auth_failure_tracker.record_failure(client_addr).await;

            SecurityEvent::AuthFailure {
                client_addr,
                reason: "Authentication required".to_string(),
            }
            .log();

            return AuthResult {
                user_id: None,
                requires_auth: true,
                error: Some("Authentication required".to_string()),
            };
        }

        // No authentication required - use IP-based user ID for rate limiting
        AuthResult {
            user_id: Some(UserId::Ip(client_addr)),
            requires_auth: false,
            error: None,
        }
    }

    /// Check rate limit for a user
    pub async fn check_rate_limit(&self, user_id: &UserId) -> bool {
        self.check_rate_limit_n(user_id, 1).await
    }

    /// Check rate limit for n requests (e.g. batch with batch_multiplier)
    pub async fn check_rate_limit_n(&self, user_id: &UserId, n: u32) -> bool {
        let mut limiters = self.rate_limiters.lock().await;

        // Get or create rate limiter for this user
        let limiter = limiters.entry(user_id.clone()).or_insert_with(|| {
            let (burst, rate) = self.default_rate_limit;
            RpcRateLimiter::new(burst, rate)
        });

        limiter.check_and_consume_n(n)
    }

    /// Check rate limit for a method/endpoint.
    /// When method-specific limits are set via set_method_rate_limit, enforces them.
    /// Otherwise allows (per-user rate limiting happens after authentication).
    pub async fn check_method_rate_limit(&self, method_name: &str) -> bool {
        let mut limiters = self.method_rate_limiters.lock().await;
        if let Some(limiter) = limiters.get_mut(method_name) {
            return limiter.check_and_consume();
        }
        true // No method-specific limit configured; allow
    }

    /// Check rate limit with endpoint information (for method-specific rate limiting)
    pub async fn check_rate_limit_with_endpoint(
        &self,
        user_id: &UserId,
        client_addr: Option<SocketAddr>,
        endpoint: Option<&str>,
    ) -> bool {
        self.check_rate_limit_with_endpoint_n(user_id, client_addr, endpoint, 1)
            .await
    }

    /// Check rate limit for n requests with endpoint info (for batch)
    pub async fn check_rate_limit_with_endpoint_n(
        &self,
        user_id: &UserId,
        client_addr: Option<SocketAddr>,
        endpoint: Option<&str>,
        n: u32,
    ) -> bool {
        let allowed = self.check_rate_limit_n(user_id, n).await;

        if !allowed {
            // Log rate limit violation
            if let Some(addr) = client_addr {
                SecurityEvent::RateLimitViolation {
                    user_id: format!("{user_id:?}"),
                    client_addr: addr,
                    endpoint: endpoint.unwrap_or("unknown").to_string(),
                }
                .log();
            }
        }

        allowed
    }

    /// Check IP-based rate limit (for unauthenticated requests)
    pub async fn check_ip_rate_limit_with_endpoint(
        &self,
        client_addr: SocketAddr,
        endpoint: Option<&str>,
    ) -> bool {
        self.check_ip_rate_limit_with_endpoint_n(client_addr, endpoint, 1)
            .await
    }

    /// Check IP-based rate limit for n requests (for batch)
    pub async fn check_ip_rate_limit_with_endpoint_n(
        &self,
        client_addr: SocketAddr,
        endpoint: Option<&str>,
        n: u32,
    ) -> bool {
        let user_id = UserId::Ip(client_addr);
        // When auth not required (rate-limit-only mode), use default_rate_limit directly.
        // When auth required, unauthenticated IPs get stricter limits (half of authenticated).
        let (ip_burst, ip_rate) = if self.auth_required {
            let (burst, rate) = self.default_rate_limit;
            (burst / 2, rate / 2)
        } else {
            self.default_rate_limit
        };

        let mut limiters = self.rate_limiters.lock().await;
        let limiter = limiters
            .entry(user_id.clone())
            .or_insert_with(|| RpcRateLimiter::new(ip_burst, ip_rate));

        let allowed = limiter.check_and_consume_n(n);

        if !allowed {
            // Log rate limit violation
            SecurityEvent::RateLimitViolation {
                user_id: format!("{user_id:?}"),
                client_addr,
                endpoint: endpoint.unwrap_or("unknown").to_string(),
            }
            .log();
        }

        allowed
    }

    /// Clean up rate limiters for users inactive beyond threshold (default: 1 hour)
    pub async fn cleanup_stale_limiters(&self) {
        const STALE_THRESHOLD_SECS: u64 = 3600; // 1 hour
        let now = current_timestamp();
        let mut limiters = self.rate_limiters.lock().await;
        limiters
            .retain(|_, limiter| now.saturating_sub(limiter.last_refill()) < STALE_THRESHOLD_SECS);
    }
}

/// Security event types for structured logging
#[derive(Debug, Clone)]
pub enum SecurityEvent {
    /// Authentication failure
    AuthFailure {
        client_addr: SocketAddr,
        reason: String,
    },
    /// Rate limit violation
    RateLimitViolation {
        user_id: String,
        client_addr: SocketAddr,
        endpoint: String,
    },
    /// Repeated authentication failures (potential brute force)
    RepeatedAuthFailures {
        client_addr: SocketAddr,
        failure_count: u32,
        time_window_seconds: u64,
    },
    /// Successful authentication
    AuthSuccess {
        user_id: String,
        client_addr: SocketAddr,
        auth_method: String,
    },
}

impl SecurityEvent {
    /// Log the security event using structured logging
    pub fn log(&self) {
        use tracing::{debug, error, warn};

        match self {
            SecurityEvent::AuthFailure {
                client_addr,
                reason,
            } => {
                warn!(
                    target: "blvm_node::rpc::security",
                    client_addr = %client_addr,
                    reason = %reason,
                    "Authentication failed"
                );
            }
            SecurityEvent::RateLimitViolation {
                user_id,
                client_addr,
                endpoint,
            } => {
                warn!(
                    target: "blvm_node::rpc::security",
                    user_id = %user_id,
                    client_addr = %client_addr,
                    endpoint = %endpoint,
                    "Rate limit violated"
                );
            }
            SecurityEvent::RepeatedAuthFailures {
                client_addr,
                failure_count,
                time_window_seconds,
            } => {
                tracing::error!(
                    target: "blvm_node::rpc::security",
                    client_addr = %client_addr,
                    failure_count = %failure_count,
                    time_window_seconds = %time_window_seconds,
                    "Repeated authentication failures detected (potential brute force attack)"
                );
            }
            SecurityEvent::AuthSuccess {
                user_id,
                client_addr,
                auth_method,
            } => {
                debug!(
                    target: "blvm_node::rpc::security",
                    user_id = %user_id,
                    client_addr = %client_addr,
                    auth_method = %auth_method,
                    "Authentication successful"
                );
            }
        }
    }
}

/// Decode `Authorization: Basic <base64>` into `(username, password)`.
fn decode_http_basic_credentials(b64: &str) -> Option<(String, String)> {
    let decoded = decode_base64_standard(b64.as_bytes()).ok()?;
    let creds = std::str::from_utf8(&decoded).ok()?;
    let (user, pass) = creds.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

fn decode_base64_standard(input: &[u8]) -> Result<Vec<u8>, ()> {
    const DECODE: [i8; 128] = {
        let mut table = [-1i8; 128];
        let mut i = 0u8;
        while i < 26 {
            table[(b'A' + i) as usize] = i as i8;
            table[(b'a' + i) as usize] = i as i8 + 26;
            i += 1;
        }
        let mut i = 0u8;
        while i < 10 {
            table[(b'0' + i) as usize] = i as i8 + 52;
            i += 1;
        }
        table[b'+' as usize] = 62;
        table[b'/' as usize] = 63;
        table
    };

    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;

    for &byte in input {
        if byte == b'=' {
            break;
        }
        if byte >= 128 {
            return Err(());
        }
        let val = DECODE[byte as usize];
        if val < 0 {
            continue;
        }
        buf = (buf << 6) | val as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

/// Tracks authentication failures to detect brute force attacks
struct AuthFailureTracker {
    failures: Arc<Mutex<HashMap<SocketAddr, Vec<u64>>>>, // addr -> timestamps
    failure_threshold: u32,
    time_window_seconds: u64,
}

impl AuthFailureTracker {
    fn new() -> Self {
        Self {
            failures: Arc::new(Mutex::new(HashMap::new())),
            failure_threshold: 5,
            time_window_seconds: 300, // 5 minutes
        }
    }

    fn time_window_seconds(&self) -> u64 {
        self.time_window_seconds
    }

    async fn failure_count(&self, addr: SocketAddr) -> u32 {
        let now = current_timestamp();
        let failures = self.failures.lock().await;
        failures
            .get(&addr)
            .map(|timestamps| {
                timestamps
                    .iter()
                    .filter(|&&t| now.saturating_sub(t) < self.time_window_seconds)
                    .count() as u32
            })
            .unwrap_or(0)
    }

    async fn is_blocked(&self, addr: SocketAddr) -> bool {
        self.failure_count(addr).await >= self.failure_threshold
    }

    async fn record_failure(&self, addr: SocketAddr) -> bool {
        let now = current_timestamp();

        let mut failures = self.failures.lock().await;
        let timestamps = failures.entry(addr).or_insert_with(Vec::new);

        // Remove old failures outside the time window
        timestamps.retain(|&t| now.saturating_sub(t) < self.time_window_seconds);

        // Add current failure
        timestamps.push(now);

        // Check if threshold exceeded
        let exceeded = timestamps.len() >= self.failure_threshold as usize;

        if exceeded {
            SecurityEvent::RepeatedAuthFailures {
                client_addr: addr,
                failure_count: timestamps.len() as u32,
                time_window_seconds: self.time_window_seconds,
            }
            .log();
        }

        exceeded
    }
}
