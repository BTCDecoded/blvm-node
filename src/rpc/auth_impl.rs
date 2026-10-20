//! RPC Authentication Manager Implementation
//!
//! Bearer-token authentication: opaque tokens registered via `add_token` / `add_admin_token`,
//! validated with constant-time comparison (`constant_time_eq`). Tokens are stored server-side
//! only; clients send `Authorization: Bearer <token>`. This is not HMAC-signed JWT — rotate
//! tokens via config reload or `remove_token` when compromised.

use crate::utils::current_timestamp;
use anyhow::Result;
use constant_time_eq::constant_time_eq;
use hyper::HeaderMap;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{debug, warn};

/// Opaque bearer token registered server-side (not a self-describing JWT/HMAC credential).
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
///
/// Refill is driven by the monotonic [`tokio::time::Instant`] clock, so wall-clock
/// adjustments (NTP steps, manual clock changes) cannot add or withhold tokens.
/// Elapsed time is credited exactly in integer arithmetic: each refill adds
/// `elapsed_ns * rate` to a sub-token remainder (in units of 1e-9 token), whole
/// tokens are moved into the bucket and the rest is carried, so tokens accrue at
/// exactly `rate` per second however calls are spaced.
pub struct RpcRateLimiter {
    /// Current number of tokens available
    tokens: u32,
    /// Maximum burst size (initial token count)
    burst_limit: u32,
    /// Tokens per second refill rate
    rate: u32,
    /// Monotonic time of the last refill
    last_refill: Instant,
    /// Fractional token carried between refills, in units of 1e-9 token (< 1e9)
    carry: u64,
}

const NANOS_PER_SEC: u128 = 1_000_000_000;

impl RpcRateLimiter {
    /// Create a new rate limiter
    pub fn new(burst_limit: u32, rate: u32) -> Self {
        Self {
            tokens: burst_limit,
            burst_limit,
            rate,
            last_refill: Instant::now(),
            carry: 0,
        }
    }

    /// Credit tokens for the time elapsed since the last refill, carrying any fractional
    /// remainder forward. Never exceeds `burst_limit`.
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed_ns = now.saturating_duration_since(self.last_refill).as_nanos();
        self.last_refill = now;

        if self.rate == 0 || self.tokens >= self.burst_limit {
            // Nothing can be added (no refill, or bucket already full); time spent
            // here is not banked toward future tokens.
            self.tokens = self.tokens.min(self.burst_limit);
            self.carry = 0;
            return;
        }

        // u128 cannot overflow: elapsed_ns < 2^94 (Duration max), rate < 2^32, carry < 2^30.
        let credit = elapsed_ns
            .saturating_mul(u128::from(self.rate))
            .saturating_add(u128::from(self.carry));
        let earned = credit / NANOS_PER_SEC;
        let room = u128::from(self.burst_limit - self.tokens);
        if earned >= room {
            // Bucket fills: excess tokens and the remainder are dropped.
            self.tokens = self.burst_limit;
            self.carry = 0;
        } else {
            // earned < room <= u32::MAX and the remainder is < 1e9: casts are lossless.
            self.tokens += earned as u32;
            self.carry = (credit % NANOS_PER_SEC) as u64;
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

        self.refill();

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

    /// Monotonic time of the last refill, i.e. the last rate-limit check (for stale
    /// limiter eviction)
    pub fn last_refill(&self) -> Instant {
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
                    }
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

                    if self.auth_required {
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
                    }
                } else if self.auth_required {
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
        const STALE_THRESHOLD: Duration = Duration::from_secs(3600); // 1 hour
        let now = Instant::now();
        let mut limiters = self.rate_limiters.lock().await;
        limiters.retain(|_, limiter| {
            now.saturating_duration_since(limiter.last_refill()) < STALE_THRESHOLD
        });
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

#[cfg(test)]
mod rate_limiter_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::time::advance;

    fn exhaust(limiter: &mut RpcRateLimiter) {
        while limiter.check_and_consume() {}
        assert_eq!(limiter.tokens_remaining(), 0);
    }

    /// Regression for the CI flake in `test_rate_limiting_time_window`: refill used whole
    /// wall-clock Unix seconds, so a short wait that straddled a second boundary was
    /// credited as a full second. Runs on the real clock on purpose.
    #[tokio::test]
    async fn short_wait_across_wall_clock_second_boundary_earns_nothing() {
        // Line up just before a wall-clock second boundary.
        loop {
            let sub_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .subsec_millis();
            if (900..950).contains(&sub_ms) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut limiter = RpcRateLimiter::new(5, 1);
        exhaust(&mut limiter);
        // ~200ms of real time crosses the boundary but is only 0.2 tokens at 1/sec.
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !limiter.check_and_consume(),
            "200ms at 1 token/sec must not refill a token"
        );
    }

    /// The limiter follows the monotonic tokio clock, not the wall clock: real (wall)
    /// time passing while the monotonic clock stands still must not add tokens.
    #[tokio::test(start_paused = true)]
    async fn wall_clock_progress_does_not_refill() {
        let mut limiter = RpcRateLimiter::new(3, 1);
        exhaust(&mut limiter);
        std::thread::sleep(Duration::from_millis(1100));
        assert!(!limiter.check_and_consume());
        advance(Duration::from_secs(1)).await;
        assert!(limiter.check_and_consume());
        assert!(!limiter.check_and_consume());
    }

    /// Many small advances credit exactly `rate * elapsed` whole tokens, never more.
    #[tokio::test(start_paused = true)]
    async fn fractional_carry_never_overcredits() {
        for rate in [1u32, 3, 7, 10, 999, 1000, 1001] {
            let mut limiter = RpcRateLimiter::new(u32::MAX, rate);
            limiter.tokens = 0;
            for step in 1..=1000u64 {
                advance(Duration::from_millis(1)).await;
                limiter.refill();
                let exact = u64::from(rate) * step / 1000; // floor(rate * step ms)
                assert_eq!(
                    u64::from(limiter.tokens_remaining()),
                    exact,
                    "rate {rate}/s after {step}ms"
                );
            }
            assert_eq!(limiter.tokens_remaining(), rate, "rate {rate}/s after 1s");
        }
    }

    /// Consuming between small advances does not leak or gain tokens either.
    #[tokio::test(start_paused = true)]
    async fn carry_with_interleaved_consumption_is_exact() {
        let mut limiter = RpcRateLimiter::new(1000, 7);
        exhaust(&mut limiter);
        let mut granted = 0u32;
        for _ in 0..10_000 {
            advance(Duration::from_millis(1)).await;
            if limiter.check_and_consume() {
                granted += 1;
            }
        }
        // 10s at 7/sec
        assert_eq!(granted, 70);
        assert_eq!(limiter.tokens_remaining(), 0);
    }

    /// Tokens are clamped to the burst limit no matter how long the limiter idles.
    #[tokio::test(start_paused = true)]
    async fn burst_cap_never_exceeded() {
        for (burst, rate) in [(1u32, 1u32), (5, 100), (100, 10), (10, u32::MAX)] {
            let mut limiter = RpcRateLimiter::new(burst, rate);
            exhaust(&mut limiter);
            for idle in [100u64, 3600, 86_400 * 365] {
                advance(Duration::from_secs(idle)).await;
                limiter.refill();
                assert_eq!(limiter.tokens_remaining(), burst);
                exhaust(&mut limiter);
            }
            // Idle while full: time spent full is not banked beyond the cap.
            advance(Duration::from_secs(10)).await;
            limiter.refill();
            assert_eq!(limiter.tokens_remaining(), burst);
            assert!(!limiter.check_and_consume_n(burst + 1));
        }
    }

    /// Extreme elapsed time and rates neither overflow nor panic.
    #[tokio::test(start_paused = true)]
    async fn huge_elapsed_and_rate_do_not_overflow() {
        let mut limiter = RpcRateLimiter::new(u32::MAX, u32::MAX);
        assert!(limiter.check_and_consume_n(u32::MAX));
        assert_eq!(limiter.tokens_remaining(), 0);
        // ~100 years
        advance(Duration::from_secs(100 * 365 * 86_400)).await;
        assert!(limiter.check_and_consume_n(u32::MAX));
        // Partial refill at maximum rate: 1ns earns floor(u32::MAX / 1e9) = 4 tokens.
        advance(Duration::from_nanos(1)).await;
        limiter.refill();
        assert_eq!(limiter.tokens_remaining(), 4);

        // A limiter last refilled long ago clamps cleanly at maximum rate.
        let mut old = RpcRateLimiter::new(3, u32::MAX);
        exhaust(&mut old);
        advance(Duration::from_secs(10 * 365 * 86_400)).await;
        assert!(old.check_and_consume());
        assert_eq!(old.tokens_remaining(), 2);
    }

    /// Stale-limiter eviction uses the monotonic clock: idle limiters go after an hour,
    /// active ones stay.
    #[tokio::test(start_paused = true)]
    async fn stale_limiters_evicted_on_monotonic_clock() {
        let auth = RpcAuthManager::with_rate_limits(false, 5, 1);
        let idle = UserId::Ip("127.0.0.1:1".parse().unwrap());
        let active = UserId::Ip("127.0.0.1:2".parse().unwrap());
        assert!(auth.check_rate_limit(&idle).await);
        assert!(auth.check_rate_limit(&active).await);
        advance(Duration::from_secs(3599)).await;
        assert!(auth.check_rate_limit(&active).await);
        advance(Duration::from_secs(2)).await;
        auth.cleanup_stale_limiters().await;
        let limiters = auth.rate_limiters.lock().await;
        assert!(!limiters.contains_key(&idle));
        assert!(limiters.contains_key(&active));
    }
}
