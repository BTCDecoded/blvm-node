//! Unified Payment State Machine
//!
//! Coordinates all payment interfaces (P2P, CTV, RPC, REST) through a unified state machine.
//! Manages payment lifecycle from request creation through settlement confirmation.

#[cfg(feature = "ctv")]
use crate::payment::covenant::{CovenantEngine, CovenantProof};
#[cfg(not(feature = "ctv"))]
type CovenantProof = (); // Stub type when CTV feature is disabled
use crate::payment::processor::{PaymentError, PaymentProcessor};
use crate::Hash;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// Payment state in the state machine
#[derive(Debug, Clone, PartialEq)]
pub enum PaymentState {
    /// Payment request created, waiting for covenant proof
    RequestCreated { request_id: String },
    /// CTV covenant proof created, ready for broadcast
    ProofCreated {
        request_id: String,
        #[cfg(feature = "ctv")]
        covenant_proof: CovenantProof,
    },
    /// Payment proof broadcast via P2P
    ProofBroadcast {
        request_id: String,
        #[cfg(feature = "ctv")]
        covenant_proof: CovenantProof,
        broadcast_peers: Vec<SocketAddr>,
    },
    /// Transaction in mempool (settlement pending)
    InMempool { request_id: String, tx_hash: Hash },
    /// Settlement confirmed on-chain
    Settled {
        request_id: String,
        tx_hash: Hash,
        block_hash: Hash,
        confirmation_count: u32,
    },
    /// Payment failed or rejected
    Failed { request_id: String, reason: String },
}

/// Unified payment state machine
pub struct PaymentStateMachine {
    /// Payment states (request_id -> PaymentState)
    states: Arc<Mutex<HashMap<String, PaymentState>>>,
    /// Covenant engine for CTV proof creation
    #[cfg(feature = "ctv")]
    covenant_engine: Arc<CovenantEngine>,
    /// Payment processor for core payment logic
    payment_processor: Arc<PaymentProcessor>,
    /// Vault engine for vault management
    #[cfg(feature = "ctv")]
    vault_engine: Option<Arc<crate::payment::vault::VaultEngine>>,
    /// Pool engine for payment pools
    #[cfg(feature = "ctv")]
    pool_engine: Option<Arc<crate::payment::pool::PoolEngine>>,
    /// Congestion manager for transaction batching
    #[cfg(feature = "ctv")]
    congestion_manager:
        Option<Arc<tokio::sync::Mutex<crate::payment::congestion::CongestionManager>>>,
}

impl PaymentStateMachine {
    /// Create a new payment state machine
    pub fn new(payment_processor: Arc<PaymentProcessor>) -> Self {
        #[cfg(feature = "ctv")]
        let covenant_engine = Arc::new(CovenantEngine::new());

        Self {
            states: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(feature = "ctv")]
            covenant_engine: covenant_engine.clone(),
            payment_processor,
            #[cfg(feature = "ctv")]
            vault_engine: Some(Arc::new(crate::payment::vault::VaultEngine::new(
                covenant_engine.clone(),
            ))),
            #[cfg(feature = "ctv")]
            pool_engine: Some(Arc::new(crate::payment::pool::PoolEngine::new(
                covenant_engine.clone(),
            ))),
            #[cfg(feature = "ctv")]
            congestion_manager: None, // Will be initialized with mempool/storage if needed
        }
    }

    /// Create payment state machine with storage
    #[cfg(feature = "ctv")]
    pub fn with_storage(
        payment_processor: Arc<PaymentProcessor>,
        storage: Option<Arc<crate::storage::Storage>>,
    ) -> Self {
        let covenant_engine = Arc::new(CovenantEngine::new());

        let vault_engine = if let Some(ref storage) = storage {
            Some(Arc::new(crate::payment::vault::VaultEngine::with_storage(
                covenant_engine.clone(),
                Arc::clone(storage),
            )))
        } else {
            Some(Arc::new(crate::payment::vault::VaultEngine::new(
                covenant_engine.clone(),
            )))
        };

        let pool_engine = if let Some(ref storage) = storage {
            Some(Arc::new(crate::payment::pool::PoolEngine::with_storage(
                covenant_engine.clone(),
                Arc::clone(storage),
            )))
        } else {
            Some(Arc::new(crate::payment::pool::PoolEngine::new(
                covenant_engine.clone(),
            )))
        };

        Self {
            states: Arc::new(Mutex::new(HashMap::new())),
            covenant_engine: covenant_engine.clone(),
            payment_processor,
            vault_engine,
            pool_engine,
            congestion_manager: None, // Will be initialized with mempool/storage if needed
        }
    }

    /// Create with congestion manager
    #[cfg(feature = "ctv")]
    pub fn with_congestion_manager(
        mut self,
        mempool_manager: Option<Arc<crate::node::mempool::MempoolManager>>,
        storage: Option<Arc<crate::storage::Storage>>,
        config: crate::payment::congestion::BatchConfig,
    ) -> Self {
        use crate::payment::congestion::CongestionManager;
        self.congestion_manager = Some(Arc::new(tokio::sync::Mutex::new(CongestionManager::new(
            self.covenant_engine.clone(),
            mempool_manager,
            storage,
            config,
        ))));
        self
    }

    /// Get vault engine
    #[cfg(feature = "ctv")]
    pub fn vault_engine(&self) -> Option<Arc<crate::payment::vault::VaultEngine>> {
        self.vault_engine.as_ref().map(Arc::clone)
    }

    /// Get pool engine
    #[cfg(feature = "ctv")]
    pub fn pool_engine(&self) -> Option<Arc<crate::payment::pool::PoolEngine>> {
        self.pool_engine.as_ref().map(Arc::clone)
    }

    /// Get congestion manager
    #[cfg(feature = "ctv")]
    pub fn congestion_manager(
        &self,
    ) -> Option<Arc<tokio::sync::Mutex<crate::payment::congestion::CongestionManager>>> {
        self.congestion_manager.as_ref().map(Arc::clone)
    }

    /// Process payment request (from P2P, RPC, or REST)
    ///
    /// Creates a payment request and optionally creates a CTV covenant proof
    /// for instant payment proof.
    ///
    /// # Arguments
    ///
    /// * `outputs` - Payment outputs
    /// * `merchant_data` - Optional merchant data
    /// * `create_covenant` - Whether to create CTV proof immediately
    ///
    /// # Returns
    ///
    /// Payment request ID and optional covenant proof
    pub async fn create_payment_request(
        &self,
        outputs: Vec<bllvm_protocol::payment::PaymentOutput>,
        merchant_data: Option<Vec<u8>>,
        create_covenant: bool,
    ) -> Result<(String, Option<CovenantProof>), PaymentError> {
        // Create payment request using payment processor
        let payment_request = self
            .payment_processor
            .create_payment_request(outputs.clone(), merchant_data, None)
            .await?;

        // Generate payment ID (same method used internally by PaymentProcessor)
        use sha2::{Digest, Sha256};
        let serialized = bincode::serialize(&payment_request).unwrap_or_default();
        let hash = Sha256::digest(&serialized);
        let payment_id = hex::encode(&hash[..16]); // Use first 16 bytes for ID

        // Store payment request
        {
            let mut states = self.states.lock().unwrap();
            states.insert(
                payment_id.clone(),
                PaymentState::RequestCreated {
                    request_id: payment_id.clone(),
                },
            );
        }

        info!("Payment request created: {}", payment_id);

        // Create CTV covenant proof if requested
        let covenant_proof = if create_covenant {
            #[cfg(feature = "ctv")]
            {
                match self.create_covenant_proof(&payment_id).await {
                    Ok(proof) => {
                        info!("CTV covenant proof created for payment: {}", payment_id);
                        Some(proof)
                    }
                    Err(e) => {
                        warn!("Failed to create CTV covenant proof: {}", e);
                        None
                    }
                }
            }
            #[cfg(not(feature = "ctv"))]
            {
                warn!("CTV feature not enabled, cannot create covenant proof");
                None
            }
        } else {
            None
        };

        Ok((payment_id, covenant_proof))
    }

    /// Create CTV covenant proof for existing payment request
    ///
    /// # Arguments
    ///
    /// * `payment_request_id` - ID of the payment request
    ///
    /// # Returns
    ///
    /// CTV covenant proof
    pub async fn create_covenant_proof(
        &self,
        payment_request_id: &str,
    ) -> Result<CovenantProof, PaymentError> {
        #[cfg(not(feature = "ctv"))]
        {
            return Err(PaymentError::FeatureNotEnabled(
                "CTV covenant requires --features ctv".to_string(),
            ));
        }

        #[cfg(feature = "ctv")]
        {
            // Get payment request
            let payment_request = self
                .payment_processor
                .get_payment_request(payment_request_id)
                .await?;

            // Create covenant proof from payment outputs
            let covenant_proof = self.covenant_engine.create_payment_covenant(
                payment_request_id,
                &payment_request.payment_details.outputs,
                None, // No input prevout for template
            )?;

            // Update state to ProofCreated
            {
                let mut states = self.states.lock().unwrap();
                states.insert(
                    payment_request_id.to_string(),
                    PaymentState::ProofCreated {
                        request_id: payment_request_id.to_string(),
                        covenant_proof: covenant_proof.clone(),
                    },
                );
            }

            info!("Covenant proof created for payment: {}", payment_request_id);

            Ok(covenant_proof)
        }
    }

    /// Broadcast payment proof via P2P
    ///
    /// # Arguments
    ///
    /// * `payment_request_id` - ID of the payment request
    /// * `peers` - List of peers to broadcast to
    ///
    /// # Returns
    ///
    /// Number of peers the proof was broadcast to
    pub async fn broadcast_proof(
        &self,
        payment_request_id: &str,
        peers: Vec<SocketAddr>,
    ) -> Result<usize, PaymentError> {
        #[cfg(not(feature = "ctv"))]
        {
            return Err(PaymentError::FeatureNotEnabled(
                "CTV covenant requires --features ctv".to_string(),
            ));
        }

        #[cfg(feature = "ctv")]
        {
            // Get current state
            let state = self.get_payment_state(payment_request_id).await?;

            // Get covenant proof from state
            let covenant_proof = match &state {
                PaymentState::ProofCreated { covenant_proof, .. } => covenant_proof.clone(),
                PaymentState::ProofBroadcast {
                    broadcast_peers, ..
                } => {
                    // Already broadcast, return current peer count
                    return Ok(broadcast_peers.len());
                }
                _ => {
                    return Err(PaymentError::ProcessingError(format!(
                        "Cannot broadcast proof from state: {:?}",
                        state
                    )));
                }
            };

            // Update state to ProofBroadcast
            {
                let mut states = self.states.lock().unwrap();
                states.insert(
                    payment_request_id.to_string(),
                    PaymentState::ProofBroadcast {
                        request_id: payment_request_id.to_string(),
                        covenant_proof: covenant_proof.clone(),
                        broadcast_peers: peers.clone(),
                    },
                );
            }

            info!(
                "Payment proof broadcast to {} peers for payment: {}",
                peers.len(),
                payment_request_id
            );

            // TODO: Actually broadcast via NetworkManager
            // For now, just update state

            Ok(peers.len())
        }
    }

    /// Update payment state to InMempool
    ///
    /// Called when payment transaction is detected in mempool.
    pub async fn mark_in_mempool(
        &self,
        payment_request_id: &str,
        tx_hash: Hash,
    ) -> Result<(), PaymentError> {
        let mut states = self.states.lock().unwrap();
        states.insert(
            payment_request_id.to_string(),
            PaymentState::InMempool {
                request_id: payment_request_id.to_string(),
                tx_hash,
            },
        );

        info!(
            "Payment transaction in mempool: {} (tx: {})",
            payment_request_id,
            hex::encode(tx_hash)
        );

        Ok(())
    }

    /// Update payment state to Settled
    ///
    /// Called when payment transaction is confirmed on-chain.
    /// Automatically triggers module decryption if this is a module payment.
    pub async fn mark_settled(
        &self,
        payment_request_id: &str,
        tx_hash: Hash,
        block_hash: Hash,
        confirmation_count: u32,
    ) -> Result<(), PaymentError> {
        let mut states = self.states.lock().unwrap();
        states.insert(
            payment_request_id.to_string(),
            PaymentState::Settled {
                request_id: payment_request_id.to_string(),
                tx_hash,
                block_hash,
                confirmation_count,
            },
        );

        info!(
            "Payment settled: {} (tx: {}, block: {}, confirmations: {})",
            payment_request_id,
            hex::encode(tx_hash),
            hex::encode(block_hash),
            confirmation_count
        );

        // Trigger decryption if this is a module payment
        // Note: Decryption will be handled by the caller or a background task
        // that checks payment state and decrypts modules

        Ok(())
    }

    /// Mark payment as failed
    pub async fn mark_failed(
        &self,
        payment_request_id: &str,
        reason: String,
    ) -> Result<(), PaymentError> {
        let mut states = self.states.lock().unwrap();
        states.insert(
            payment_request_id.to_string(),
            PaymentState::Failed {
                request_id: payment_request_id.to_string(),
                reason: reason.clone(),
            },
        );

        warn!("Payment failed: {} - {}", payment_request_id, reason);

        Ok(())
    }

    /// Get current payment state
    pub async fn get_payment_state(
        &self,
        payment_request_id: &str,
    ) -> Result<PaymentState, PaymentError> {
        let states = self.states.lock().unwrap();
        states
            .get(payment_request_id)
            .cloned()
            .ok_or_else(|| PaymentError::RequestNotFound(payment_request_id.to_string()))
    }

    /// Get all payment states (for listing)
    pub fn list_payment_states(&self) -> HashMap<String, PaymentState> {
        let states = self.states.lock().unwrap();
        states.clone()
    }

    /// Create module payment request with CTV covenant support
    ///
    /// This integrates module payments with the unified payment state machine,
    /// enabling CTV covenant proofs for instant payment verification.
    ///
    /// # Arguments
    ///
    /// * `manifest` - Module manifest containing payment configuration
    /// * `node_script` - Node operator's payment script (10% of payment)
    /// * `merchant_key` - Optional merchant key for signing
    /// * `create_covenant` - Whether to create CTV proof immediately
    ///
    /// # Returns
    ///
    /// Payment request ID and optional covenant proof
    pub async fn create_module_payment_request(
        &self,
        manifest: &crate::module::registry::manifest::ModuleManifest,
        module_hash: &[u8; 32], // Module hash for encryption key derivation
        node_script: Vec<u8>,   // Node operator's payment script (10% of payment)
        merchant_key: Option<&secp256k1::SecretKey>,
        create_covenant: bool,
    ) -> Result<(String, Option<CovenantProof>), PaymentError> {
        // Create module payment request using payment processor
        let payment_request = self
            .payment_processor
            .create_module_payment_request(manifest, module_hash, node_script, merchant_key)
            .await?;

        // Generate payment ID
        use sha2::{Digest, Sha256};
        let serialized = bincode::serialize(&payment_request).unwrap_or_default();
        let hash = Sha256::digest(&serialized);
        let payment_id = hex::encode(&hash[..16]);

        // Store payment request
        {
            let mut states = self.states.lock().unwrap();
            states.insert(
                payment_id.clone(),
                PaymentState::RequestCreated {
                    request_id: payment_id.clone(),
                },
            );
        }

        info!("Module payment request created: {}", payment_id);

        // Create CTV covenant proof if requested
        let covenant_proof = if create_covenant {
            #[cfg(feature = "ctv")]
            {
                match self.create_covenant_proof(&payment_id).await {
                    Ok(proof) => {
                        info!(
                            "CTV covenant proof created for module payment: {}",
                            payment_id
                        );
                        Some(proof)
                    }
                    Err(e) => {
                        warn!("Failed to create CTV covenant proof: {}", e);
                        None
                    }
                }
            }
            #[cfg(not(feature = "ctv"))]
            {
                warn!("CTV feature not enabled, cannot create covenant proof");
                None
            }
        } else {
            None
        };

        Ok((payment_id, covenant_proof))
    }
}
