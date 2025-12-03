//! CTV Covenant Engine for Payment Proofs
//!
//! Creates and verifies CTV (CheckTemplateVerify) covenants for instant payment proofs.
//! Provides cryptographic commitment to payment structure without requiring instant settlement.

use crate::payment::processor::PaymentError;
use crate::{Hash, Transaction};
#[cfg(feature = "ctv")]
use blvm_consensus::bip119::calculate_template_hash;
use blvm_protocol::payment::PaymentOutput;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// CTV covenant proof for payment commitment
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CovenantProof {
    /// CTV template hash (32 bytes)
    pub template_hash: Hash,
    /// Transaction template structure (without signatures)
    pub transaction_template: TransactionTemplate,
    /// Payment request ID this proof commits to
    pub payment_request_id: String,
    /// Timestamp when proof was created
    pub created_at: u64,
    /// Optional cryptographic signature of the proof
    pub signature: Option<Vec<u8>>,
}

/// Transaction template for CTV (without scriptSig)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransactionTemplate {
    pub version: u32, // Natural in consensus, but we use u32 for template
    pub inputs: Vec<TemplateInput>,
    pub outputs: Vec<TemplateOutput>,
    pub lock_time: u32, // Natural in consensus, but we use u32 for template
}

/// Template input (CTV format: no scriptSig)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemplateInput {
    pub prevout_hash: Hash,
    pub prevout_index: u32,
    pub sequence: u32,
    // NO scriptSig (CTV requirement)
}

/// Template output
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemplateOutput {
    pub value: u64,
    pub script_pubkey: Vec<u8>,
}

/// Settlement status for payment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SettlementStatus {
    /// Payment proof created, not yet broadcast
    ProofCreated,
    /// Payment proof broadcast, transaction not yet in mempool
    ProofBroadcast,
    /// Transaction in mempool, waiting for confirmation
    InMempool { tx_hash: Hash },
    /// Settlement confirmed on-chain
    Settled {
        tx_hash: Hash,
        block_hash: Hash,
        confirmation_count: u32,
    },
    /// Payment failed or rejected
    Failed { reason: String },
}

/// CTV Covenant Engine
pub struct CovenantEngine;

impl CovenantEngine {
    /// Create a new covenant engine
    pub fn new() -> Self {
        Self
    }

    /// Create CTV covenant proof for payment commitment
    ///
    /// Creates a transaction template that commits to specific payment outputs
    /// using CTV (CheckTemplateVerify). This provides instant proof of payment
    /// intent without requiring instant on-chain settlement.
    ///
    /// # Arguments
    ///
    /// * `payment_request_id` - ID of the payment request
    /// * `payment_outputs` - Payment outputs to commit to
    /// * `input_prevout` - Previous output to spend (optional, for template)
    ///
    /// # Returns
    ///
    /// CTV covenant proof with template hash and transaction structure
    pub fn create_payment_covenant(
        &self,
        payment_request_id: &str,
        payment_outputs: &[PaymentOutput],
        input_prevout: Option<(Hash, u32)>,
    ) -> Result<CovenantProof, PaymentError> {
        #[cfg(not(feature = "ctv"))]
        {
            return Err(PaymentError::FeatureNotEnabled(
                "CTV covenant requires --features ctv".to_string(),
            ));
        }

        #[cfg(feature = "ctv")]
        {
            // Create transaction template from payment outputs
            let template = self.create_transaction_template(payment_outputs, input_prevout)?;

            // Calculate CTV template hash
            let template_hash = self.calculate_template_hash_for_template(&template, 0)?;

            let created_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();

            Ok(CovenantProof {
                template_hash,
                transaction_template: template,
                payment_request_id: payment_request_id.to_string(),
                created_at,
                signature: None,
            })
        }
    }

    /// Verify CTV covenant proof matches expected outputs
    ///
    /// Verifies that the covenant proof commits to the expected payment outputs.
    ///
    /// # Arguments
    ///
    /// * `proof` - The covenant proof to verify
    /// * `expected_outputs` - Expected payment outputs
    ///
    /// # Returns
    ///
    /// `true` if proof matches expected outputs, `false` otherwise
    pub fn verify_covenant_proof(
        &self,
        proof: &CovenantProof,
        expected_outputs: &[PaymentOutput],
    ) -> Result<bool, PaymentError> {
        #[cfg(not(feature = "ctv"))]
        {
            return Err(PaymentError::FeatureNotEnabled(
                "CTV covenant requires --features ctv".to_string(),
            ));
        }

        #[cfg(feature = "ctv")]
        {
            // Verify outputs match
            if proof.transaction_template.outputs.len() != expected_outputs.len() {
                return Ok(false);
            }

            for (template_output, expected_output) in proof
                .transaction_template
                .outputs
                .iter()
                .zip(expected_outputs.iter())
            {
                if template_output.value != expected_output.amount.unwrap_or(0) {
                    return Ok(false);
                }
                if template_output.script_pubkey != expected_output.script {
                    return Ok(false);
                }
            }

            // Recalculate template hash to verify it matches
            let recalculated_hash =
                self.calculate_template_hash_for_template(&proof.transaction_template, 0)?;

            Ok(recalculated_hash == proof.template_hash)
        }
    }

    /// Verify that a transaction matches the CTV covenant
    ///
    /// Verifies that an actual transaction matches the CTV template hash
    /// from the covenant proof.
    ///
    /// # Arguments
    ///
    /// * `tx` - The transaction to verify
    /// * `covenant_proof` - The covenant proof to verify against
    /// * `input_index` - Index of the input being verified
    ///
    /// # Returns
    ///
    /// `true` if transaction matches covenant, `false` otherwise
    pub fn verify_transaction_matches_covenant(
        &self,
        tx: &Transaction,
        covenant_proof: &CovenantProof,
        input_index: usize,
    ) -> Result<bool, PaymentError> {
        #[cfg(not(feature = "ctv"))]
        {
            return Err(PaymentError::FeatureNotEnabled(
                "CTV covenant requires --features ctv".to_string(),
            ));
        }

        #[cfg(feature = "ctv")]
        {
            // Transaction type from bllvm-node is the same as bllvm-consensus (re-exported)
            // Calculate template hash for the transaction
            let tx_template_hash = calculate_template_hash(tx, input_index).map_err(|e| {
                PaymentError::ProcessingError(format!("CTV hash calculation failed: {}", e))
            })?;

            // Verify template hash matches
            Ok(tx_template_hash == covenant_proof.template_hash)
        }
    }

    /// Create transaction template from payment outputs
    fn create_transaction_template(
        &self,
        payment_outputs: &[PaymentOutput],
        input_prevout: Option<(Hash, u32)>,
    ) -> Result<TransactionTemplate, PaymentError> {
        // Create template inputs
        let inputs = if let Some((prevout_hash, prevout_index)) = input_prevout {
            vec![TemplateInput {
                prevout_hash,
                prevout_index,
                sequence: 0xffffffff, // Default sequence
            }]
        } else {
            // Dummy input for template (will be replaced with actual input when creating transaction)
            vec![TemplateInput {
                prevout_hash: [0u8; 32],
                prevout_index: 0,
                sequence: 0xffffffff,
            }]
        };

        // Convert payment outputs to template outputs
        let outputs: Result<Vec<TemplateOutput>, PaymentError> = payment_outputs
            .iter()
            .map(|output| {
                Ok(TemplateOutput {
                    value: output.amount.unwrap_or(0),
                    script_pubkey: output.script.clone(),
                })
            })
            .collect();

        Ok(TransactionTemplate {
            version: 2, // Standard transaction version
            inputs,
            outputs: outputs?,
            lock_time: 0, // No locktime by default
        })
    }

    /// Calculate template hash for transaction template
    #[cfg(feature = "ctv")]
    fn calculate_template_hash_for_template(
        &self,
        template: &TransactionTemplate,
        input_index: usize,
    ) -> Result<Hash, PaymentError> {
        // Convert template to actual transaction for hash calculation
        // (CTV hash calculation requires a Transaction struct)
        let tx = self.template_to_transaction(template)?;

        // CTV (CheckTemplateVerify) hash calculation
        // blvm_protocol does not currently re-export CTV functions, so we use
        // blvm_consensus directly. This is the correct approach as CTV is a
        // consensus-level feature that belongs in the consensus crate.
        use blvm_consensus::bip119::calculate_template_hash;

        calculate_template_hash(&tx, input_index).map_err(|e| {
            PaymentError::ProcessingError(format!("CTV hash calculation failed: {}", e))
        })
    }

    /// Convert transaction template to Transaction (for CTV hash calculation)
    #[cfg(feature = "ctv")]
    fn template_to_transaction(
        &self,
        template: &TransactionTemplate,
    ) -> Result<blvm_consensus::types::Transaction, PaymentError> {
        // Convert template inputs to transaction inputs
        use blvm_consensus::types::{
            ByteString, Integer, Natural, OutPoint, Transaction as ConsensusTransaction,
            TransactionInput as ConsensusInput, TransactionOutput as ConsensusOutput,
        };

        let inputs: Vec<ConsensusInput> = template
            .inputs
            .iter()
            .map(|ti| {
                ConsensusInput {
                    prevout: OutPoint {
                        hash: ti.prevout_hash,
                        index: Natural::from(ti.prevout_index as u64),
                    },
                    script_sig: ByteString::from(vec![]), // CTV: no scriptSig
                    sequence: Natural::from(ti.sequence as u64),
                }
            })
            .collect();

        // Convert template outputs to transaction outputs
        // Integer is i64, so we need to convert u64 to i64 (saturating at i64::MAX)
        let outputs: Vec<ConsensusOutput> = template
            .outputs
            .iter()
            .map(|to| ConsensusOutput {
                value: Integer::from(to.value.min(i64::MAX as u64) as i64),
                script_pubkey: ByteString::from(to.script_pubkey.clone()),
            })
            .collect();

        Ok(ConsensusTransaction {
            version: Natural::from(template.version as u64),
            inputs: inputs.into(),
            outputs: outputs.into(),
            lock_time: Natural::from(template.lock_time as u64),
        })
    }
}

impl Default for CovenantEngine {
    fn default() -> Self {
        Self::new()
    }
}
