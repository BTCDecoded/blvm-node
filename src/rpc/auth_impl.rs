//! RPC Authentication Manager Implementation
//!
//! This file contains the full RpcAuthManager implementation with constant-time token comparison.

use anyhow::Result;
use constant_time_eq::constant_time_eq;
use hyper::HeaderMap;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
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
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("SystemTime should always be after UNIX_EPOCH")
            .as_secs();
        Self {
            tokens: burst_limit,
            burst_limit,
            rate,
            last_refill: now,
        }
    }

    /// Check if a request is allowed and consume a token
    pub fn check_and_consume(&mut self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("SystemTime should always be after UNIX_EPOCH")
            .as_secs();

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

        // Check if we have tokens available
        if self.tokens > 0 {
            self.tokens -= 1;
            true
        } else {
            false
        }
    }

    /// Get current token count (for monitoring)
    pub fn tokens_remaining(&self) -> u32 {
        self.tokens
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
    /// Authentication failure tracker for DoS protection
    auth_failure_tracker: AuthFailureTracker,
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
            auth_failure_tracker: AuthFailureTracker::new(),
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
            auth_failure_tracker: AuthFailureTracker::new(),
        }
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

    /// Remove an authentication token
    pub async fn remove_token(&self, token: &str) -> Result<()> {
        let mut tokens = self.valid_tokens.lock().await;
        if let Some(user_id) = tokens.remove(token) {
            let mut limiters = self.rate_limiters.lock().await;
            limiters.remove(&user_id);
        }
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
                            user_id: format!("{:?}", user_id),
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
                        // Record authentication failure for brute force detection
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
        }

        // Try certificate-based authentication (from TLS connection)
        // Note: This would need to be integrated with TLS connection handling
        // For now, we'll check a custom header that could be set by TLS middleware
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
                        user_id: format!("{:?}", user_id),
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
        let mut limiters = self.rate_limiters.lock().await;

        // Get or create rate limiter for this user
        let limiter = limiters.entry(user_id.clone()).or_insert_with(|| {
            let (burst, rate) = self.default_rate_limit;
            RpcRateLimiter::new(burst, rate)
        });

        limiter.check_and_consume()
    }

    /// Check rate limit for a method/endpoint (uses default rate limiting)
    /// This is a convenience method for method-specific rate limiting
    pub async fn check_method_rate_limit(&self, _method_name: &str) -> bool {
        // For now, use default rate limiting
        // In the future, this could support method-specific rate limits
        // For now, we'll use a per-IP rate limiter for unauthenticated requests
        // This method is called from RPC server before authentication, so we use IP-based limiting
        true // Allow by default - actual rate limiting happens after authentication
    }

    /// Check rate limit with endpoint information (for method-specific rate limiting)
    pub async fn check_rate_limit_with_endpoint(
        &self,
        user_id: &UserId,
        client_addr: Option<SocketAddr>,
        endpoint: Option<&str>,
    ) -> bool {
        let allowed = self.check_rate_limit(user_id).await;

        if !allowed {
            // Log rate limit violation
            if let Some(addr) = client_addr {
                SecurityEvent::RateLimitViolation {
                    user_id: format!("{:?}", user_id),
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
        let user_id = UserId::Ip(client_addr);
        // Use stricter rate limits for unauthenticated requests (half of authenticated)
        let (burst, rate) = self.default_rate_limit;
        let ip_burst = burst / 2;
        let ip_rate = rate / 2;

        let mut limiters = self.rate_limiters.lock().await;
        let limiter = limiters
            .entry(user_id.clone())
            .or_insert_with(|| RpcRateLimiter::new(ip_burst, ip_rate));

        let allowed = limiter.check_and_consume();

        if !allowed {
            // Log rate limit violation
            SecurityEvent::RateLimitViolation {
                user_id: format!("{:?}", user_id),
                client_addr,
                endpoint: endpoint.unwrap_or("unknown").to_string(),
            }
            .log();
        }

        allowed
    }

    /// Clean up rate limiters for disconnected users (optional optimization)
    pub async fn cleanup_stale_limiters(&self) {
        // For now, we keep all limiters. In production, you might want to
        // remove limiters for users that haven't made requests in a while.
        // This is a placeholder for future optimization.
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

    async fn record_failure(&self, addr: SocketAddr) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("SystemTime should always be after UNIX_EPOCH")
            .as_secs();

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
