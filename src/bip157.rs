//! BIP157: Client-Side Block Filtering Network Protocol
//!
//! Specification: https://github.com/bitcoin/bips/blob/master/bip-0157.mediawiki
//!
//! Defines network messages for requesting and serving compact block filters.
//! Enables efficient transaction discovery for light clients.

use crate::bip158::CompactBlockFilter;
use protocol_engine::{BlockHeader, Hash};
use sha2::{Digest, Sha256};

/// Filter header - commits to previous filter header and current filter
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterHeader {
    /// Filter hash (double SHA256 of filter data)
    pub filter_hash: Hash,
    /// Previous filter header hash
    pub prev_header_hash: Hash,
}

impl FilterHeader {
    /// Calculate filter header from filter and previous header
    pub fn new(filter: &CompactBlockFilter, prev_header: Option<&FilterHeader>) -> Self {
        // Filter hash = SHA256(SHA256(filter_data))
        let mut hasher = Sha256::new();
        hasher.update(&filter.filter_data);
        let first_hash = hasher.finalize();

        let mut hasher2 = Sha256::new();
        hasher2.update(&first_hash);
        let filter_hash_bytes = hasher2.finalize();

        let mut filter_hash = [0u8; 32];
        filter_hash.copy_from_slice(&filter_hash_bytes);

        // Previous header hash
        let prev_header_hash = if let Some(prev) = prev_header {
            // Header hash = SHA256(SHA256(filter_hash || prev_header_hash))
            let mut combined = Vec::new();
            combined.extend_from_slice(&prev.filter_hash);
            combined.extend_from_slice(&prev.prev_header_hash);

            let mut hasher = Sha256::new();
            hasher.update(&combined);
            let first_hash = hasher.finalize();

            let mut hasher2 = Sha256::new();
            hasher2.update(&first_hash);
            let header_hash_bytes = hasher2.finalize();

            let mut header_hash = [0u8; 32];
            header_hash.copy_from_slice(&header_hash_bytes);
            header_hash
        } else {
            // Genesis filter header (all zeros or block hash)
            [0u8; 32]
        };

        FilterHeader {
            filter_hash,
            prev_header_hash,
        }
    }

    /// Calculate header hash (double SHA256 of filter_hash || prev_header_hash)
    pub fn header_hash(&self) -> Hash {
        let mut combined = Vec::new();
        combined.extend_from_slice(&self.filter_hash);
        combined.extend_from_slice(&self.prev_header_hash);

        let mut hasher = Sha256::new();
        hasher.update(&combined);
        let first_hash = hasher.finalize();

        let mut hasher2 = Sha256::new();
        hasher2.update(&first_hash);
        let hash_bytes = hasher2.finalize();

        let mut header_hash = [0u8; 32];
        header_hash.copy_from_slice(&hash_bytes);
        header_hash
    }
}

/// Filter type (currently only Basic Compact Filters)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterType {
    /// Basic compact filters (BIP158)
    Basic = 0,
}

impl FilterType {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(FilterType::Basic),
            _ => None,
        }
    }

    pub fn to_u8(self) -> u8 {
        self as u8
    }
}

/// getcfilters message - request filters for block range
#[derive(Debug, Clone)]
pub struct GetCfilters {
    /// Filter type
    pub filter_type: FilterType,
    /// Start block height
    pub start_height: u32,
    /// Stop block hash
    pub stop_hash: Hash,
}

/// cfilter message - compact block filter response
#[derive(Debug, Clone)]
pub struct CFilter {
    /// Filter type
    pub filter_type: FilterType,
    /// Block hash
    pub block_hash: Hash,
    /// Compact block filter
    pub filter: CompactBlockFilter,
}

/// getcfheaders message - request filter headers
#[derive(Debug, Clone)]
pub struct GetCfheaders {
    /// Filter type
    pub filter_type: FilterType,
    /// Start block height
    pub start_height: u32,
    /// Stop block hash
    pub stop_hash: Hash,
}

/// cfheaders message - filter headers response
#[derive(Debug, Clone)]
pub struct Cfheaders {
    /// Filter type
    pub filter_type: FilterType,
    /// Stop block hash
    pub stop_hash: Hash,
    /// Previous filter header
    pub prev_header: FilterHeader,
    /// Filter headers (one per block in range)
    pub filter_headers: Vec<Hash>,
}

/// getcfcheckpt message - request filter checkpoints
#[derive(Debug, Clone)]
pub struct GetCfcheckpt {
    /// Filter type
    pub filter_type: FilterType,
    /// Stop block hash
    pub stop_hash: Hash,
}

/// cfcheckpt message - filter checkpoint response
#[derive(Debug, Clone)]
pub struct Cfcheckpt {
    /// Filter type
    pub filter_type: FilterType,
    /// Stop block hash
    pub stop_hash: Hash,
    /// Filter header hashes at checkpoint intervals
    pub filter_header_hashes: Vec<Hash>,
}

/// BIP157 service flag bit
pub const NODE_COMPACT_FILTERS: u64 = 1 << 6;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_header() {
        let empty_filter = CompactBlockFilter {
            filter_data: vec![1, 2, 3],
            num_elements: 0,
        };

        let header1 = FilterHeader::new(&empty_filter, None);
        let header2 = FilterHeader::new(&empty_filter, Some(&header1));

        // Headers should be different when chained
        assert_ne!(header1.filter_hash, header2.filter_hash);
        assert_eq!(header2.prev_header_hash, header1.header_hash());
    }
}
