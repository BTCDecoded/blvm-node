//! RPC Authentication and Authorization
//!
//! Provides token-based and certificate-based authentication for RPC requests.
//! Also includes per-user rate limiting.

/// Redact sensitive tokens from log messages
/// Replaces token patterns with [REDACTED] to prevent token exposure in logs
pub fn redact_tokens_from_log(msg: &str, tokens: &[String]) -> String {
    let mut redacted = msg.to_string();
    for token in tokens {
        // Redact full token
        redacted = redacted.replace(token, "[REDACTED]");
        // Redact Bearer token format
        let bearer_token = format!("Bearer {}", token);
        redacted = redacted.replace(&bearer_token, "Bearer [REDACTED]");
    }
    redacted
}

/// Redact authorization headers from log messages
/// Replaces "Authorization: Bearer ..." patterns with [REDACTED]
pub fn redact_auth_header(msg: &str) -> String {
    // Simple regex-like replacement for "Authorization: Bearer <token>"
    // This is a simple implementation - for more complex patterns, consider using regex crate
    let mut redacted = msg.to_string();
    
    // Find and replace "Authorization: Bearer <anything>" patterns
    // This is a simple approach - in production, might want to use regex for more accuracy
    if let Some(start) = redacted.find("Authorization: Bearer ") {
        if let Some(end) = redacted[start..].find(|c: char| c == '\n' || c == '\r' || c == '"' || c == '\'') {
            let token_start = start + "Authorization: Bearer ".len();
            let token_end = start + end;
            if token_end > token_start {
                redacted.replace_range(token_start..token_end, "[REDACTED]");
            }
        } else {
            // No delimiter found, redact to end of string
            let token_start = start + "Authorization: Bearer ".len();
            if token_start < redacted.len() {
                redacted.replace_range(token_start.., "[REDACTED]");
            }
        }
    }
    
    redacted
}

// Re-export types and implementation from auth_impl module
#[path = "auth_impl.rs"]
mod auth_impl;
pub use auth_impl::*;
