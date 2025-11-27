//! Unified BIP70 Payment Protocol
//!
//! Provides transport-agnostic payment processing that works over both HTTP and P2P.
//! Supports module payments with 75/15/10 split (author/commons/node).

pub mod processor;

#[cfg(feature = "bip70-http")]
pub mod http;

pub use processor::PaymentProcessor;

#[cfg(feature = "bip70-http")]
pub use http::handle_payment_routes;
