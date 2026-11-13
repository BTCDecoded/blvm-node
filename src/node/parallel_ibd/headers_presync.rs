//! Header pre-sync. Headers stay here until their combined work reaches the
//! network chain-work floor. The block assigner is idle during that phase.

use blvm_consensus::block::block_header_hash;
use blvm_consensus::pow::{U256, get_block_proof};
use blvm_protocol::BlockHeader;

/// How often a pre-sync commitment is recorded, and how many headers a
/// redownload step may hand to the assigner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeadersSyncParams {
    pub commitment_period: u64,
    pub redownload_buffer: u64,
}

pub(crate) fn headers_sync_params(network: &str) -> HeadersSyncParams {
    let (commitment_period, redownload_buffer) = match network.to_lowercase().as_str() {
        "mainnet" | "bitcoinv1" => (649, 15_400),
        "testnet" | "testnet3" => (682, 14_613),
        "testnet4" => (616, 16_199),
        "signet" => (629, 15_885),
        "regtest" => (275, 7_017),
        _ => (649, 15_400),
    };
    HeadersSyncParams {
        commitment_period,
        redownload_buffer,
    }
}

pub(crate) fn network_key(network: blvm_protocol::types::Network) -> &'static str {
    use blvm_protocol::types::Network;
    match network {
        Network::Mainnet => "mainnet",
        Network::Testnet => "testnet3",
        Network::Testnet4 => "testnet4",
        Network::Signet => "signet",
        Network::Regtest => "regtest",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadersSyncPhase {
    Presync,
    Redownload,
    Final,
}

/// Holds headers until chain work reaches the floor, then releases them.
pub(crate) struct HeadersPresync {
    phase: HeadersSyncPhase,
    params: HeadersSyncParams,
    minimum: U256,
    work: U256,
    held: Vec<BlockHeader>,
    seen: u64,
    commitments: Vec<[u8; 32]>,
}

impl HeadersPresync {
    pub(crate) fn new(network: &str) -> Self {
        let params = headers_sync_params(network);
        let minimum = U256::from_u128(crate::config::minimum_chain_work_for_network(network));
        let phase = if minimum.is_zero() {
            HeadersSyncPhase::Final
        } else {
            HeadersSyncPhase::Presync
        };
        Self {
            phase,
            params,
            minimum,
            work: U256::zero(),
            held: Vec::new(),
            seen: 0,
            commitments: Vec::new(),
        }
    }

    pub(crate) fn phase(&self) -> HeadersSyncPhase {
        self.phase
    }

    /// The assigner runs only in redownload or final.
    pub(crate) fn releases_assigner(&self) -> bool {
        self.phase != HeadersSyncPhase::Presync
    }

    /// A resume already has chainwork for the parent. Counting only the new
    /// headers leaves a short catch-up under the floor, so the assigner never starts.
    pub(crate) fn release_if_prior_work_meets_floor(&mut self, prior: U256) -> bool {
        if self.phase != HeadersSyncPhase::Presync {
            return self.releases_assigner();
        }
        if prior >= self.minimum {
            self.work = prior;
            self.phase = HeadersSyncPhase::Final;
        }
        self.releases_assigner()
    }

    pub(crate) fn note(&mut self, header: &BlockHeader) {
        self.accept(std::slice::from_ref(header), |_| {});
    }

    /// `assign` observes a header only after the phase has left pre-sync.
    pub(crate) fn accept(
        &mut self,
        headers: &[BlockHeader],
        mut assign: impl FnMut(&[BlockHeader]),
    ) {
        for header in headers {
            let proof = get_block_proof(header.bits).unwrap_or_else(|_| U256::zero());
            self.work = self.work.saturating_add(proof);
            self.seen = self.seen.saturating_add(1);
            self.held.push(header.clone());
            if self.phase == HeadersSyncPhase::Presync {
                if self.params.commitment_period > 0
                    && self.seen.is_multiple_of(self.params.commitment_period)
                {
                    self.commitments.push(block_header_hash(header));
                    self.held.clear();
                }
                if self.work >= self.minimum {
                    self.phase = HeadersSyncPhase::Redownload;
                }
            }
        }
        self.release(&mut assign);
    }

    fn release(&mut self, assign: &mut impl FnMut(&[BlockHeader])) {
        match self.phase {
            HeadersSyncPhase::Presync => {}
            HeadersSyncPhase::Redownload => {
                let n = (self.params.redownload_buffer as usize).min(self.held.len());
                if n == 0 {
                    if self.held.is_empty() {
                        self.phase = HeadersSyncPhase::Final;
                    }
                    return;
                }
                let batch: Vec<BlockHeader> = self.held.drain(..n).collect();
                assign(&batch);
                if self.held.is_empty() {
                    self.phase = HeadersSyncPhase::Final;
                }
            }
            HeadersSyncPhase::Final => {
                if self.held.is_empty() {
                    return;
                }
                let batch = std::mem::take(&mut self.held);
                assign(&batch);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn easy_header() -> BlockHeader {
        BlockHeader {
            version: 1,
            prev_block_hash: [0; 32],
            merkle_root: [1; 32],
            timestamp: 1_700_000_000,
            bits: 0x207fffff,
            nonce: 1,
        }
    }

    #[test]
    fn params_follow_the_network() {
        let mainnet = headers_sync_params("mainnet");
        assert_eq!(mainnet.commitment_period, 649);
        assert_eq!(mainnet.redownload_buffer, 15_400);
        assert_eq!(headers_sync_params("bitcoinv1"), mainnet);
        let testnet3 = headers_sync_params("testnet3");
        assert_eq!(testnet3.commitment_period, 682);
        assert_eq!(testnet3.redownload_buffer, 14_613);
        assert_eq!(headers_sync_params("testnet"), testnet3);
        let testnet4 = headers_sync_params("testnet4");
        assert_eq!(testnet4.commitment_period, 616);
        assert_eq!(testnet4.redownload_buffer, 16_199);
        let signet = headers_sync_params("signet");
        assert_eq!(signet.commitment_period, 629);
        assert_eq!(signet.redownload_buffer, 15_885);
        let regtest = headers_sync_params("regtest");
        assert_eq!(regtest.commitment_period, 275);
        assert_eq!(regtest.redownload_buffer, 7_017);
    }

    #[test]
    fn low_work_headers_past_one_period_do_not_reach_the_assigner() {
        let period = headers_sync_params("mainnet").commitment_period;
        let mut sync = HeadersPresync::new("mainnet");
        let headers = vec![easy_header(); period as usize + 1];
        let mut observed = 0usize;
        sync.accept(&headers, |batch| observed += batch.len());
        assert!(observed == 0);
        assert_eq!(sync.phase(), HeadersSyncPhase::Presync);
        assert!(!sync.releases_assigner());
        let floor = crate::config::minimum_chain_work_for_network("mainnet");
        assert!(sync.work < U256::from_u128(floor));
    }

    #[test]
    fn short_signet_catch_up_uses_stored_chainwork() {
        let batch = vec![easy_header(); 8];
        let mut fresh = HeadersPresync::new("signet");
        let mut observed = 0usize;
        fresh.accept(&batch, |headers| observed += headers.len());
        assert_eq!(observed, 0);
        assert!(!fresh.releases_assigner());

        let floor = U256::from_u128(crate::config::minimum_chain_work_for_network("signet"));
        let mut caught_up = HeadersPresync::new("signet");
        assert!(caught_up.release_if_prior_work_meets_floor(floor));
        let mut released = 0usize;
        caught_up.accept(&batch, |headers| released += headers.len());
        assert_eq!(released, 8);
        assert!(caught_up.releases_assigner());

        let mut below = HeadersPresync::new("signet");
        assert!(!below.release_if_prior_work_meets_floor(U256::from_u128(1)));
        assert!(!below.releases_assigner());
    }

    #[test]
    fn work_at_the_floor_reaches_the_assigner() {
        let mut sync = HeadersPresync::new("mainnet");
        let mut header = easy_header();
        header.bits = 0x03000001;
        let mut observed = 0usize;
        sync.accept(std::slice::from_ref(&header), |batch| {
            observed += batch.len()
        });
        assert_eq!(observed, 1);
        assert!(sync.releases_assigner());
    }
}
