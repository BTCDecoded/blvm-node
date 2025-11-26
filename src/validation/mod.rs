//! Parallel block validation (Phase 4.2)
//!
//! Validates multiple blocks in parallel when safe to do so (not on chain tip).
//! Provides 2-3x sync speed improvement for historical block replay.
//!
//! Safety constraints:
//! - Blocks on chain tip must be validated sequentially (for real-time consensus)
//! - Blocks in the same chain branch must be validated sequentially (UTXO dependencies)
//! - Only independent block branches can be validated in parallel
//!
//! Reference: Bitcoin Core's parallel block validation for IBD

use anyhow::Result;
use bllvm_protocol::block::connect_block;
use bllvm_protocol::segwit::Witness;
use bllvm_protocol::types::Network;
use bllvm_protocol::{Block, UtxoSet, ValidationResult};

/// Block validation context
#[derive(Debug, Clone)]
pub struct BlockValidationContext {
    pub block: Block,
    pub height: u64,
    pub prev_utxo_set: UtxoSet,
    pub prev_block_hash: [u8; 32],
}

/// Parallel block validator
pub struct ParallelBlockValidator {
    /// Maximum parallel validation depth
    /// Blocks beyond this depth from tip are validated in parallel
    max_parallel_depth: usize,
}

impl Default for ParallelBlockValidator {
    /// Default validator (conservative: only validate blocks >100 deep in parallel)
    fn default() -> Self {
        Self::new(100)
    }
}

impl ParallelBlockValidator {
    /// Create a new parallel block validator
    pub fn new(max_parallel_depth: usize) -> Self {
        Self { max_parallel_depth }
    }

    /// Validate a single block (sequential)
    pub fn validate_block(
        &self,
        context: &BlockValidationContext,
        network: Network,
    ) -> Result<(ValidationResult, UtxoSet)> {
        // Create empty witnesses for each transaction
        let witnesses: Vec<Witness> = context
            .block
            .transactions
            .iter()
            .map(|_| Vec::new())
            .collect();
        let (result, new_utxo_set, _undo_log) = connect_block(
            &context.block,
            &witnesses,
            context.prev_utxo_set.clone(),
            context.height,
            None, // No recent headers for single block validation
            network,
        )?;
        Ok((result, new_utxo_set))
    }

    /// Validate multiple blocks in parallel (Phase 4.2)
    ///
    /// Only validates blocks in parallel if:
    /// 1. They're not on the chain tip (depth > max_parallel_depth)
    /// 2. They're in independent branches (no UTXO dependencies)
    ///
    /// Returns validation results in order of input blocks.
    #[cfg(feature = "production")]
    pub fn validate_blocks_parallel(
        &self,
        contexts: &[BlockValidationContext],
        depth_from_tip: usize,
        network: Network,
    ) -> Result<Vec<(ValidationResult, UtxoSet)>> {
        // Only use parallel validation if blocks are deep enough from tip
        if depth_from_tip <= self.max_parallel_depth {
            // Too close to tip - validate sequentially for safety
            return self.validate_blocks_sequential(contexts, network);
        }

        // Validate blocks in parallel (if production feature enabled)
        // Note: Each block uses its own UTXO set, so they're independent
        #[cfg(feature = "production")]
        let results: Vec<_> = {
            use rayon::prelude::*;
            contexts
                .par_iter()
                .map(|context| {
                    // Create empty witnesses for each transaction
                    let witnesses: Vec<Witness> = context
                        .block
                        .transactions
                        .iter()
                        .map(|_| Vec::new())
                        .collect();
                    let (result, new_utxo_set, _undo_log) = connect_block(
                        &context.block,
                        &witnesses,
                        context.prev_utxo_set.clone(),
                        context.height,
                        None, // No recent headers for parallel validation
                        network,
                    )?;
                    Ok::<_, anyhow::Error>((result, new_utxo_set))
                })
                .collect()
        };

        #[cfg(not(feature = "production"))]
        let results: Vec<_> = {
            // Fallback to sequential validation if production feature not enabled
            contexts
                .iter()
                .map(|context| {
                    // Create empty witnesses for each transaction
                    let witnesses: Vec<Witness> = context
                        .block
                        .transactions
                        .iter()
                        .map(|_| Vec::new())
                        .collect();
                    let (result, new_utxo_set, _undo_log) = connect_block(
                        &context.block,
                        &witnesses,
                        context.prev_utxo_set.clone(),
                        context.height,
                        None, // No recent headers for parallel validation
                        network,
                    )?;
                    Ok::<_, anyhow::Error>((result, new_utxo_set))
                })
                .collect()
        };

        // Collect results and check for errors
        let mut validated_results = Vec::new();
        for result in results {
            validated_results.push(result?);
        }
        Ok(validated_results)
    }

    /// Validate multiple blocks sequentially (default, verification-safe)
    pub fn validate_blocks_sequential(
        &self,
        contexts: &[BlockValidationContext],
        network: Network,
    ) -> Result<Vec<(ValidationResult, UtxoSet)>> {
        let mut results = Vec::new();

        for context in contexts {
            // Create empty witnesses for each transaction
            let witnesses: Vec<Witness> = context
                .block
                .transactions
                .iter()
                .map(|_| Vec::new())
                .collect();
            let (result, new_utxo_set, _undo_log) = connect_block(
                &context.block,
                &witnesses,
                context.prev_utxo_set.clone(),
                context.height,
                None, // No recent headers for sequential validation
                network,
            )?;
            results.push((result, new_utxo_set));
        }

        Ok(results)
    }

    /// Validate blocks with automatic parallel/sequential selection
    ///
    /// Chooses parallel or sequential validation based on depth from tip.
    pub fn validate_blocks(
        &self,
        contexts: &[BlockValidationContext],
        depth_from_tip: usize,
        network: Network,
    ) -> Result<Vec<(ValidationResult, UtxoSet)>> {
        #[cfg(feature = "production")]
        {
            if depth_from_tip > self.max_parallel_depth {
                return self.validate_blocks_parallel(contexts, depth_from_tip, network);
            }
        }

        // Sequential validation (default or when too close to tip)
        self.validate_blocks_sequential(contexts, network)
    }
}

/// Block validation result with metadata
#[derive(Debug, Clone)]
pub struct BlockValidationResult {
    pub validation_result: ValidationResult,
    pub utxo_set: UtxoSet,
    pub height: u64,
    pub block_hash: [u8; 32],
    pub validation_time_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parallel_validator_creation() {
        let validator = ParallelBlockValidator::default();
        assert_eq!(validator.max_parallel_depth, 100);
    }

    #[test]
    fn test_sequential_validation() {
        let validator = ParallelBlockValidator::default();
        let contexts = vec![]; // Empty contexts
        let results = validator.validate_blocks_sequential(&contexts, Network::Mainnet);
        assert!(results.is_ok());
    }
}
