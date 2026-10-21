//! Inbound P2P frame plus the per-peer queue permit that keeps it alive.
//!
//! The permit is released when the last clone of the frame is dropped, so one
//! peer cannot leave an unbounded number of max-sized messages sitting in
//! `peer_tx` after the read loop has moved on.

use std::ops::Deref;
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Byte budget for frames queued from one peer and not yet dropped.
#[derive(Clone)]
pub struct RawRxPermit {
    hold: Option<Arc<RxHold>>,
}

impl std::fmt::Debug for RawRxPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawRxPermit")
            .field("held", &self.hold.is_some())
            .finish()
    }
}

struct RxHold {
    _permit: OwnedSemaphorePermit,
}

impl RawRxPermit {
    pub(crate) fn none() -> Self {
        Self { hold: None }
    }

    /// One full protocol message may sit in the queue. Further reads wait.
    pub(crate) fn budget(max_message_length: usize) -> Arc<Semaphore> {
        Arc::new(Semaphore::new(max_message_length.max(24)))
    }

    pub(crate) async fn acquire(budget: &Arc<Semaphore>, bytes: usize) -> Option<Self> {
        if bytes == 0 {
            return Some(Self::none());
        }
        let n = u32::try_from(bytes).ok()?;
        match budget.clone().acquire_many_owned(n).await {
            Ok(permit) => Some(Self {
                hold: Some(Arc::new(RxHold { _permit: permit })),
            }),
            Err(_) => None,
        }
    }
}

/// Bytes of one inbound frame. Cloning shares the queue permit.
pub struct RawFrame {
    bytes: Vec<u8>,
    #[allow(dead_code)]
    permit: RawRxPermit,
}

impl RawFrame {
    pub(crate) fn new(bytes: Vec<u8>, permit: RawRxPermit) -> Self {
        Self { bytes, permit }
    }

    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.bytes
    }
}

impl From<Vec<u8>> for RawFrame {
    fn from(bytes: Vec<u8>) -> Self {
        Self::new(bytes, RawRxPermit::none())
    }
}

impl Deref for RawFrame {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

impl AsRef<[u8]> for RawFrame {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Clone for RawFrame {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
            permit: self.permit.clone(),
        }
    }
}

impl std::fmt::Debug for RawFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawFrame")
            .field("len", &self.bytes.len())
            .finish()
    }
}
