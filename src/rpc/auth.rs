//! RPC Authentication and Authorization
//!
//! Provides token-based and certificate-based authentication for RPC requests.
//! Also includes per-user rate limiting.

use anyhow::Result;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
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
    /// Per-IP rate limiters (for unauthenticated requests)
    ip_rate_limiters: Arc<Mutex<HashMap<SocketAddr, RpcRateLimiter>>>,
    /// Per-IP rate limit (burst, rate per second) - stricter than authenticated users
    ip_rate_limit: (u32, u32),
    /// Per-method rate limits (method_name -> (burst, rate))
    method_rate_limits: Arc<Mutex<HashMap<String, (u32, u32)>>>,
    /// Per-method rate limiters (method_name -> rate_limiter)
    method_rate_limiters: Arc<Mutex<HashMap<String, RpcRateLimiter>>>,
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
            ip_rate_limiters: Arc::new(Mutex::new(HashMap::new())),
            ip_rate_limit: (50, 5), // Stricter for unauthenticated: 50 burst, 5 req/sec
            method_rate_limits: Arc::new(Mutex::new(HashMap::new())),
            method_rate_limiters: Arc::new(Mutex::new(HashMap::new())),
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
            ip_rate_limiters: Arc::new(Mutex::new(HashMap::new())),
            ip_rate_limit: (default_burst / 2, default_rate / 2), // Half of authenticated limit
            method_rate_limits: Arc::new(Mutex::new(HashMap::new())),
            method_rate_limiters: Arc::new(Mutex::new(HashMap::new())),
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
    pub async fn authenticate_request(
        &self,
        headers: &hyper::HeaderMap,
        client_addr: SocketAddr,
    ) -> AuthResult {
        // Try token-based authentication first
        if let Some(auth_header) = headers.get("authorization") {
            if let Ok(auth_str) = auth_header.to_str() {
                // Support "Bearer <token>" format
                if let Some(token) = auth_str.strip_prefix("Bearer ") {
                    let tokens = self.valid_tokens.lock().await;
                    if let Some(user_id) = tokens.get(token) {
                        debug!("Token authentication successful for {}", client_addr);
                        return AuthResult {
                            user_id: Some(user_id.clone()),
                            requires_auth: self.auth_required,
                            error: None,
                        };
                    } else {
                        warn!("Invalid token from {}", client_addr);
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
                if let Some(user_id) = certs.get(fingerprint) {
                    debug!("Certificate authentication successful for {}", client_addr);
                    return AuthResult {
                        user_id: Some(user_id.clone()),
                        requires_auth: self.auth_required,
                        error: None,
                    };
                }
            }
        }

        // If authentication is required but not provided, reject
        if self.auth_required {
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

    /// Check rate limit for an IP address (for unauthenticated requests)
    pub async fn check_ip_rate_limit(&self, ip: SocketAddr) -> bool {
        let mut limiters = self.ip_rate_limiters.lock().await;

        // Get or create rate limiter for this IP
        let limiter = limiters.entry(ip).or_insert_with(|| {
            let (burst, rate) = self.ip_rate_limit;
            RpcRateLimiter::new(burst, rate)
        });

        limiter.check_and_consume()
    }

    /// Check rate limit for a specific RPC method
    pub async fn check_method_rate_limit(&self, method_name: &str) -> bool {
        let method_limits = self.method_rate_limits.lock().await;
        let mut method_limiters = self.method_rate_limiters.lock().await;

        // Check if there's a custom rate limit for this method
        let (burst, rate) = method_limits.get(method_name).copied().unwrap_or({
            // Default per-method limits (more restrictive for expensive methods)
            match method_name {
                "getblock" | "getblockheader" | "getrawtransaction" => (20, 2), // Expensive queries
                "sendrawtransaction" | "submitblock" => (10, 1),                // Write operations
                _ => (100, 10), // Default for other methods
            }
        });

        // Get or create rate limiter for this method
        let limiter = method_limiters
            .entry(method_name.to_string())
            .or_insert_with(|| RpcRateLimiter::new(burst, rate));

        limiter.check_and_consume()
    }

    /// Set rate limit for a specific RPC method
    pub async fn set_method_rate_limit(&self, method_name: &str, burst: u32, rate: u32) {
        let mut method_limits = self.method_rate_limits.lock().await;
        method_limits.insert(method_name.to_string(), (burst, rate));

        // Update existing rate limiter if present
        let mut method_limiters = self.method_rate_limiters.lock().await;
        if let Some(limiter) = method_limiters.get_mut(method_name) {
            *limiter = RpcRateLimiter::new(burst, rate);
        }
    }

    /// Clean up rate limiters for disconnected users (optional optimization)
    pub async fn cleanup_stale_limiters(&self) {
        // For now, we keep all limiters. In production, you might want to
        // remove limiters for users that haven't made requests in a while.
        // This is a placeholder for future optimization.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_token_authentication() {
        let auth = RpcAuthManager::new(true);
        auth.add_token("test-token-123".to_string()).await.unwrap();

        let mut headers = hyper::HeaderMap::new();
        headers.insert("authorization", "Bearer test-token-123".parse().unwrap());

        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let result = auth.authenticate_request(&headers, addr).await;

        assert!(result.user_id.is_some());
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn test_invalid_token() {
        let auth = RpcAuthManager::new(true);
        auth.add_token("valid-token".to_string()).await.unwrap();

        let mut headers = hyper::HeaderMap::new();
        headers.insert("authorization", "Bearer invalid-token".parse().unwrap());

        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let result = auth.authenticate_request(&headers, addr).await;

        assert!(result.user_id.is_none());
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_rate_limiting() {
        let auth = RpcAuthManager::with_rate_limits(false, 5, 1); // 5 burst, 1/sec
        let user_id = UserId::Ip("127.0.0.1:8080".parse().unwrap());

        // Should allow 5 requests (burst)
        for _ in 0..5 {
            assert!(auth.check_rate_limit(&user_id).await);
        }

        // 6th request should be rate limited
        assert!(!auth.check_rate_limit(&user_id).await);
    }

    #[tokio::test]
    async fn test_no_auth_when_not_required() {
        let auth = RpcAuthManager::new(false);
        let headers = hyper::HeaderMap::new();
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

        let result = auth.authenticate_request(&headers, addr).await;

        assert!(result.user_id.is_some());
        assert!(!result.requires_auth);
        assert!(result.error.is_none());
    }
}
