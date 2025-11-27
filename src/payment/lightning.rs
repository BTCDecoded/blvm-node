//! Lightning payment stub
//!
//! Stub implementation for Lightning payments that decrypts immediately.
//! Real Lightning integration will replace this in the future.

use crate::payment::processor::PaymentError;
use tracing::info;

/// Lightning payment stub
///
/// This is a placeholder for future Lightning Network integration.
/// For now, it just marks payments as confirmed immediately (for testing).
pub struct LightningPaymentStub;

impl LightningPaymentStub {
    /// Create a new Lightning payment stub
    pub fn new() -> Self {
        Self
    }

    /// Process Lightning payment (stub)
    ///
    /// For now, just marks payment as confirmed immediately.
    /// Real implementation will verify Lightning payment via Lightning node.
    ///
    /// # Arguments
    ///
    /// * `invoice` - Lightning invoice (BOLT11 format)
    /// * `payment_id` - Payment ID to mark as confirmed
    ///
    /// # Returns
    ///
    /// Ok(()) if payment is processed (stub always succeeds)
    pub async fn process_lightning_payment(
        &self,
        _invoice: &str,
        payment_id: &str,
    ) -> Result<(), PaymentError> {
        // Stub: Mark as confirmed immediately
        // Real implementation will:
        // 1. Verify invoice is valid
        // 2. Check if payment was made via Lightning network
        // 3. Verify payment hash matches invoice
        // 4. Only then mark as confirmed
        info!(
            "Lightning payment stub: marking {} as confirmed (real Lightning integration pending)",
            payment_id
        );
        Ok(())
    }

    /// Check if Lightning payment is confirmed (stub)
    ///
    /// For now, always returns true (decrypt immediately).
    /// Real implementation will check Lightning network.
    ///
    /// # Arguments
    ///
    /// * `payment_id` - Payment ID to check
    ///
    /// # Returns
    ///
    /// Ok(true) if payment is confirmed (stub always returns true)
    pub async fn is_payment_confirmed(&self, payment_id: &str) -> Result<bool, PaymentError> {
        // Stub: Always return true (decrypt immediately)
        // Real implementation will:
        // 1. Look up payment in Lightning node
        // 2. Check if HTLC is settled
        // 3. Verify payment on-chain (if needed)
        info!(
            "Lightning payment stub: checking {} (always confirmed in stub)",
            payment_id
        );
        Ok(true)
    }
}

impl Default for LightningPaymentStub {
    fn default() -> Self {
        Self::new()
    }
}
