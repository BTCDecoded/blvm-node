//! Stratum V2 pool client handle.
//!
//! Construction only records the target URL; connection and mining protocol
//! integration are implemented incrementally.

/// Stratum V2 client (URL-based; connect when mining stack is wired).
#[derive(Debug, Clone)]
pub struct StratumV2Client {
    url: String,
}

impl StratumV2Client {
    /// Create a client for the given `tcp://`, `quinn://`, or `iroh://` URL.
    pub fn new(url: String) -> Self {
        Self { url }
    }

    /// Pool / proxy URL this client was constructed with.
    pub fn url(&self) -> &str {
        &self.url
    }
}
