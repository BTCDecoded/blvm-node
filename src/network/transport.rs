//! Transport abstraction layer for network communications
//!
//! This module provides a unified interface for different transport protocols
//! (TCP, Quinn QUIC, Iroh/QUIC) allowing the network layer to support multiple transports
//! simultaneously.

use anyhow::Result;
use bitflags::bitflags;
use std::net::SocketAddr;

/// Transport address - supports TCP, Quinn (SocketAddr), and Iroh (public key)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TransportAddr {
    /// Traditional TCP/IP address (for Bitcoin P2P compatibility)
    Tcp(SocketAddr),
    /// Quinn QUIC address (SocketAddr-based like TCP)
    #[cfg(feature = "quinn")]
    Quinn(SocketAddr),
    /// Iroh public key-based address (for QUIC transport with NAT traversal)
    #[cfg(feature = "iroh")]
    Iroh(Vec<u8>), // Public key bytes
}

impl TransportAddr {
    /// Create TCP transport address
    pub fn tcp(addr: SocketAddr) -> Self {
        Self::Tcp(addr)
    }

    #[cfg(feature = "quinn")]
    /// Create Quinn transport address
    pub fn quinn(addr: SocketAddr) -> Self {
        Self::Quinn(addr)
    }

    #[cfg(feature = "iroh")]
    /// Create Iroh transport address from public key
    pub fn iroh(pubkey: Vec<u8>) -> Self {
        Self::Iroh(pubkey)
    }

    /// Check if this is a TCP address
    pub fn is_tcp(&self) -> bool {
        matches!(self, Self::Tcp(_))
    }

    #[cfg(feature = "quinn")]
    /// Check if this is a Quinn address
    pub fn is_quinn(&self) -> bool {
        matches!(self, Self::Quinn(_))
    }

    #[cfg(feature = "iroh")]
    /// Check if this is an Iroh address
    pub fn is_iroh(&self) -> bool {
        matches!(self, Self::Iroh(_))
    }

    /// Get port number if available (TCP/Quinn only)
    pub fn port(&self) -> Option<u16> {
        match self {
            Self::Tcp(addr) => Some(addr.port()),
            #[cfg(feature = "quinn")]
            Self::Quinn(addr) => Some(addr.port()),
            #[cfg(feature = "iroh")]
            Self::Iroh(_) => None, // Iroh uses public keys, no port
        }
    }
}

impl From<SocketAddr> for TransportAddr {
    fn from(addr: SocketAddr) -> Self {
        Self::Tcp(addr)
    }
}

impl std::fmt::Display for TransportAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportAddr::Tcp(addr) => write!(f, "{addr}"),
            #[cfg(feature = "quinn")]
            TransportAddr::Quinn(addr) => write!(f, "quinn://{addr}"),
            #[cfg(feature = "iroh")]
            TransportAddr::Iroh(pubkey) => write!(f, "iroh://{}", hex::encode(&pubkey[..8])),
        }
    }
}

/// Transport type identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportType {
    /// Traditional TCP transport (Bitcoin P2P compatible)
    Tcp,
    /// Quinn QUIC transport (direct QUIC without NAT traversal)
    #[cfg(feature = "quinn")]
    Quinn,
    /// Iroh QUIC-based transport (with NAT traversal and DERP)
    #[cfg(feature = "iroh")]
    Iroh,
}

/// Transport trait - abstracts over different network transports
///
/// Implementations provide connection establishment and management
/// for their specific transport protocol.
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// Connection type for this transport
    type Connection: TransportConnection + Send;
    /// Listener type for accepting incoming connections
    type Listener: TransportListener + Send;

    /// Get the transport type
    fn transport_type(&self) -> TransportType;

    /// Listen for incoming connections on the given address
    async fn listen(&self, addr: SocketAddr) -> Result<Self::Listener>;

    /// Connect to a peer at the given address
    async fn connect(&self, addr: TransportAddr) -> Result<Self::Connection>;
}

/// Transport connection - abstraction for an active connection
///
/// Provides send/receive operations and connection metadata
#[async_trait::async_trait]
pub trait TransportConnection: Send + Sync {
    /// Send data to the peer
    async fn send(&mut self, data: &[u8]) -> Result<()>;

    /// Receive data from the peer
    ///
    /// Returns Ok(Vec<u8>) with received data, or error on failure
    /// May return Ok(vec![]) if connection closed gracefully
    async fn recv(&mut self) -> Result<Vec<u8>>;

    /// Send data on a specific channel (for protocols like Stratum V2 that use channels)
    ///
    /// Default implementation just calls `send()` - transports that don't support channels
    /// will use the default behavior. Transports that support channels (e.g., QUIC streams)
    /// should override this method.
    async fn send_on_channel(&mut self, _channel_id: Option<u32>, data: &[u8]) -> Result<()> {
        // Default: ignore channel_id and use standard send
        // Transports that support channels (e.g., QUIC streams) should override this method
        self.send(data).await
    }

    /// Get the peer's transport address
    fn peer_addr(&self) -> TransportAddr;

    /// Check if connection is still active
    fn is_connected(&self) -> bool;

    /// Close the connection
    async fn close(&mut self) -> Result<()>;
}

/// Transport listener - abstraction for accepting incoming connections
///
/// Handles incoming connection establishment for a transport
#[async_trait::async_trait]
pub trait TransportListener: Send + Sync {
    /// Connection type that this listener produces
    type Connection: TransportConnection + Send;

    /// Accept a new incoming connection
    ///
    /// Returns the connection and the peer's address
    async fn accept(&mut self) -> Result<(Self::Connection, TransportAddr)>;

    /// Get the local address this listener is bound to
    fn local_addr(&self) -> Result<SocketAddr>;
}

bitflags! {
    /// Transport preference for network manager
    ///
    /// Supports all combinations of TCP, Iroh, and Quinn transports.
    /// Use bitwise OR to combine transports: `TransportPreference::TCP | TransportPreference::IROH`
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TransportPreference: u8 {
        /// TCP transport (Bitcoin P2P compatible)
        const TCP   = 1 << 0;
        /// Iroh QUIC transport (with NAT traversal and DERP)
        #[cfg(feature = "iroh")]
        const IROH  = 1 << 1;
        /// Quinn QUIC transport (direct QUIC without NAT traversal)
        #[cfg(feature = "quinn")]
        const QUINN = 1 << 2;
    }
}

impl Default for TransportPreference {
    fn default() -> Self {
        Self::TCP
    }
}

impl TransportPreference {
    /// Check if TCP is allowed
    pub fn allows_tcp(&self) -> bool {
        self.contains(Self::TCP)
    }

    /// Check if Iroh is allowed
    #[cfg(feature = "iroh")]
    pub fn allows_iroh(&self) -> bool {
        self.contains(Self::IROH)
    }

    /// Check if Quinn is allowed
    #[cfg(feature = "quinn")]
    pub fn allows_quinn(&self) -> bool {
        self.contains(Self::QUINN)
    }

    /// Get list of enabled transport types
    pub fn enabled_transports(&self) -> Vec<TransportType> {
        let mut transports = Vec::new();

        if self.allows_tcp() {
            transports.push(TransportType::Tcp);
        }

        #[cfg(feature = "quinn")]
        if self.allows_quinn() {
            transports.push(TransportType::Quinn);
        }

        #[cfg(feature = "iroh")]
        if self.allows_iroh() {
            transports.push(TransportType::Iroh);
        }

        transports
    }

    // Backward compatibility constants
    /// TCP-only mode (Bitcoin P2P compatible, default)
    pub const TCP_ONLY: Self = Self::TCP;

    #[cfg(feature = "iroh")]
    /// Iroh-only mode
    pub const IROH_ONLY: Self = Self::IROH;

    #[cfg(feature = "quinn")]
    /// Quinn-only mode
    pub const QUINN_ONLY: Self = Self::QUINN;

    #[cfg(feature = "iroh")]
    /// Hybrid mode (TCP + Iroh) - backward compatibility
    pub fn hybrid() -> Self {
        Self::TCP | Self::IROH
    }

    #[cfg(all(feature = "iroh", feature = "quinn"))]
    /// All transports enabled (TCP + Iroh + Quinn)
    pub fn all_transports() -> Self {
        Self::TCP | Self::IROH | Self::QUINN
    }
}
