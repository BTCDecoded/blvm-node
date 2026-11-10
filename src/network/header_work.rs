//! Required-work check for a header before it is written into the height index.
//!
//! The compact bits must be the bits [`next_required_bits`](blvm_protocol::pow::next_required_bits)
//! returns for that height, and the header hash must meet that target. Parents accepted
//! earlier in the same message are not stored yet; callers pass them in `pending`.

use blvm_protocol::BlockHeader;
use blvm_protocol::types::Network;

/// `pending` entries are `(height, bits, timestamp)` for headers this message already
/// accepted. A stored row is used only when the height is not in that list.
pub(crate) fn ancestor_seeing_pending<'a>(
    pending: &'a [(u64, u64, u64)],
    stored: &'a dyn Fn(u64) -> Option<(u64, u64)>,
) -> impl Fn(u64) -> Option<(u64, u64)> + 'a {
    move |height| {
        if let Some((_, bits, timestamp)) = pending.iter().rev().find(|(h, _, _)| *h == height) {
            return Some((*bits, *timestamp));
        }
        stored(height)
    }
}

/// The header at `height` carries the bits that height requires, and its hash meets them.
///
/// Height 0 has no parent. A missing ancestor, or bits other than the required bits,
/// rejects the header.
pub(crate) fn header_meets_required_work(
    header: &BlockHeader,
    height: u64,
    network: Network,
    ancestor: &dyn Fn(u64) -> Option<(u64, u64)>,
) -> bool {
    if height == 0 {
        return blvm_protocol::pow::check_proof_of_work(header).unwrap_or(false);
    }
    let Ok(Some(required)) =
        blvm_protocol::pow::next_required_bits(network, height, header.timestamp, ancestor)
    else {
        return false;
    };
    header.bits == required && blvm_protocol::pow::check_proof_of_work(header).unwrap_or(false)
}

/// Protocol version wins. The chain record is the fallback when the engine is absent.
pub(crate) fn network_for_header_work(
    protocol: Option<blvm_protocol::ProtocolVersion>,
    stored: Network,
) -> Network {
    protocol
        .map(blvm_protocol::ProtocolVersion::consensus_network)
        .unwrap_or(stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grind(mut header: BlockHeader) -> BlockHeader {
        for nonce in 0..200_000 {
            header.nonce = nonce;
            if blvm_protocol::pow::check_proof_of_work(&header).unwrap_or(false) {
                return header;
            }
        }
        panic!("header did not meet its own target");
    }

    #[test]
    fn easy_bits_do_not_meet_a_harder_parent() {
        let parent_time = 1_600_000_000u64;
        let header = grind(BlockHeader {
            version: 4,
            prev_block_hash: [1u8; 32],
            merkle_root: [2u8; 32],
            timestamp: parent_time + 1,
            bits: 0x207fffff,
            nonce: 0,
        });
        let ancestor = |h: u64| {
            if h == 100 {
                Some((0x1d00ffffu64, parent_time))
            } else {
                None
            }
        };
        assert!(blvm_protocol::pow::check_proof_of_work(&header).unwrap_or(false));
        assert!(!header_meets_required_work(
            &header,
            101,
            Network::Mainnet,
            &ancestor,
        ));
    }

    #[test]
    fn second_header_uses_the_parent_accepted_in_this_batch() {
        let parent_time = 1_600_000_000u64;
        let stored = |h: u64| {
            if h == 100 {
                Some((0x207fffffu64, parent_time))
            } else {
                None
            }
        };
        let first = grind(BlockHeader {
            version: 4,
            prev_block_hash: [1u8; 32],
            merkle_root: [2u8; 32],
            timestamp: parent_time + 1,
            bits: 0x207fffff,
            nonce: 0,
        });
        assert!(header_meets_required_work(
            &first,
            101,
            Network::Mainnet,
            &stored
        ));
        let pending = [(101u64, first.bits, first.timestamp)];
        let with_batch = ancestor_seeing_pending(&pending, &stored);
        let second = grind(BlockHeader {
            version: 4,
            prev_block_hash: [3u8; 32],
            merkle_root: [4u8; 32],
            timestamp: parent_time + 2,
            bits: 0x207fffff,
            nonce: 0,
        });
        assert!(
            header_meets_required_work(&second, 102, Network::Mainnet, &with_batch),
            "the parent accepted earlier in this message supplies the required bits"
        );
        assert!(
            !header_meets_required_work(&second, 102, Network::Mainnet, &stored),
            "without that parent the next header has no required work"
        );

        let hard_parent = [(101u64, 0x1d00ffffu64, parent_time)];
        let hard = ancestor_seeing_pending(&hard_parent, &stored);
        assert!(!header_meets_required_work(
            &second,
            102,
            Network::Mainnet,
            &hard
        ));
    }
}
