//! Package messages carried as witness transaction ids (inventory type 5).

use anyhow::{Result, bail};
use blvm_protocol::Hash;
use sha2::{Digest, Sha256};

use crate::network::protocol::MAX_PROTOCOL_PAYLOAD;

/// Bit 0: package transactions (`pkgtxns`).
pub const PACKAGE_TX: u64 = 1 << 0;
/// Bit 1: ancestor package info (`ancpkginfo`).
pub const PACKAGE_ANCESTORS: u64 = 1 << 1;
/// Version word this node sends in `sendpackages`.
pub const PACKAGE_VERSION_LOCAL: u64 = PACKAGE_TX | PACKAGE_ANCESTORS;
/// Most wtxids in `ancpkginfo` or `getpkgtxns`.
pub const MAX_PACKAGE_WTXIDS: usize = 100;

/// Features both sides named. Bits outside the word we send stay off.
pub fn package_enabled(remote: u64) -> u64 {
    remote & PACKAGE_VERSION_LOCAL
}

/// One SHA-256 of the wtxids sorted by their reversed bytes.
pub fn package_hash(wtxids: &[Hash]) -> Hash {
    let mut ordered = wtxids.to_vec();
    ordered.sort_by(|left, right| left.iter().rev().cmp(right.iter().rev()));
    let mut bytes = Vec::with_capacity(ordered.len() * 32);
    for wtxid in ordered {
        bytes.extend_from_slice(&wtxid);
    }
    let digest = Sha256::digest(&bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

pub fn encode_sendpackages(version: u64) -> [u8; 8] {
    version.to_le_bytes()
}

pub fn decode_sendpackages(payload: &[u8]) -> Result<u64> {
    if payload.len() != 8 {
        bail!("sendpackages is 8 bytes");
    }
    Ok(u64::from_le_bytes(
        payload.try_into().expect("length checked"),
    ))
}

pub fn encode_wtxid_list(wtxids: &[Hash]) -> Result<Vec<u8>> {
    if wtxids.is_empty() || wtxids.len() > MAX_PACKAGE_WTXIDS {
        bail!("wtxid list count rejected");
    }
    let mut out = Vec::with_capacity(1 + wtxids.len() * 32);
    write_compact_size(wtxids.len() as u64, &mut out);
    for wtxid in wtxids {
        out.extend_from_slice(wtxid);
    }
    Ok(out)
}

pub fn decode_wtxid_list(payload: &[u8]) -> Result<Vec<Hash>> {
    let (count, offset) =
        read_compact_size(payload).ok_or_else(|| anyhow::anyhow!("wtxid list truncated"))?;
    if count == 0 || count > MAX_PACKAGE_WTXIDS as u64 {
        bail!("wtxid list count rejected");
    }
    let count = count as usize;
    let body = payload
        .get(offset..)
        .ok_or_else(|| anyhow::anyhow!("wtxid list truncated"))?;
    if body.len() != count * 32 {
        bail!("wtxid list length rejected");
    }
    let mut wtxids = Vec::with_capacity(count);
    for chunk in body.chunks(32) {
        let mut wtxid = [0u8; 32];
        wtxid.copy_from_slice(chunk);
        wtxids.push(wtxid);
    }
    Ok(wtxids)
}

pub fn encode_ancpkginfo(wtxids: &[Hash]) -> Result<Vec<u8>> {
    if has_repeat(wtxids) {
        bail!("ancpkginfo repeated wtxid rejected");
    }
    encode_wtxid_list(wtxids)
}

pub fn decode_ancpkginfo(payload: &[u8]) -> Result<Vec<Hash>> {
    let wtxids = decode_wtxid_list(payload)?;
    if has_repeat(&wtxids) {
        bail!("ancpkginfo repeated wtxid rejected");
    }
    Ok(wtxids)
}

pub fn encode_pkgtxns(transactions: &[Vec<u8>]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write_compact_size(transactions.len() as u64, &mut out);
    for tx in transactions {
        out.extend_from_slice(tx);
    }
    if out.len() > MAX_PROTOCOL_PAYLOAD {
        bail!("pkgtxns exceeds payload cap");
    }
    Ok(out)
}

pub fn decode_pkgtxns(payload: &[u8]) -> Result<Vec<Vec<u8>>> {
    if payload.len() > MAX_PROTOCOL_PAYLOAD {
        bail!("pkgtxns exceeds payload cap");
    }
    let (count, mut pos) =
        read_compact_size(payload).ok_or_else(|| anyhow::anyhow!("pkgtxns truncated"))?;
    let mut transactions = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let rest = payload
            .get(pos..)
            .ok_or_else(|| anyhow::anyhow!("pkgtxns truncated"))?;
        let consumed = blvm_protocol::serialization::deserialize_transaction_with_witness(rest)
            .map_err(|err| anyhow::anyhow!("pkgtxns transaction rejected: {err}"))?
            .2;
        if consumed == 0 || pos + consumed > payload.len() {
            bail!("pkgtxns truncated");
        }
        transactions.push(payload[pos..pos + consumed].to_vec());
        pos += consumed;
    }
    if pos != payload.len() {
        bail!("pkgtxns trailing bytes");
    }
    Ok(transactions)
}

fn has_repeat(wtxids: &[Hash]) -> bool {
    let mut seen = std::collections::HashSet::with_capacity(wtxids.len());
    wtxids.iter().any(|wtxid| !seen.insert(*wtxid))
}

fn write_compact_size(n: u64, out: &mut Vec<u8>) {
    if n < 253 {
        out.push(n as u8);
    } else if n <= u16::MAX as u64 {
        out.push(0xfd);
        out.extend_from_slice(&(n as u16).to_le_bytes());
    } else if n <= u32::MAX as u64 {
        out.push(0xfe);
        out.extend_from_slice(&(n as u32).to_le_bytes());
    } else {
        out.push(0xff);
        out.extend_from_slice(&n.to_le_bytes());
    }
}

fn read_compact_size(data: &[u8]) -> Option<(u64, usize)> {
    let first = *data.first()?;
    match first {
        n @ 0..=252 => Some((n as u64, 1)),
        0xfd => {
            let bytes = data.get(1..3)?;
            Some((u16::from_le_bytes([bytes[0], bytes[1]]) as u64, 3))
        }
        0xfe => {
            let bytes: [u8; 4] = data.get(1..5)?.try_into().ok()?;
            Some((u32::from_le_bytes(bytes) as u64, 5))
        }
        _ => {
            let bytes: [u8; 8] = data.get(1..9)?.try_into().ok()?;
            Some((u64::from_le_bytes(bytes), 9))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;
    use sha2::Sha256;

    #[test]
    fn sendpackages_is_eight_little_endian_bytes_and_the_intersection_is_the_enabled_set() {
        let word = PACKAGE_TX | PACKAGE_ANCESTORS | (1 << 4);
        let bytes = encode_sendpackages(word);
        assert_eq!(bytes.len(), 8);
        assert_eq!(decode_sendpackages(&bytes).unwrap(), word);
        assert!(decode_sendpackages(&[0u8; 7]).is_err());
        assert_eq!(package_enabled(word), PACKAGE_VERSION_LOCAL);
        assert_eq!(package_enabled(PACKAGE_TX), PACKAGE_TX);
        assert_eq!(package_enabled(PACKAGE_ANCESTORS), PACKAGE_ANCESTORS);
        assert_eq!(package_enabled(1 << 4), 0);
    }

    #[test]
    fn wtxid_lists_reject_empty_too_long_and_a_repeated_ancestor() {
        let one = [1u8; 32];
        let two = [2u8; 32];
        let encoded = encode_wtxid_list(&[one, two]).unwrap();
        assert_eq!(decode_wtxid_list(&encoded).unwrap(), vec![one, two]);
        assert_eq!(decode_ancpkginfo(&encoded).unwrap(), vec![one, two]);
        assert!(encode_wtxid_list(&[]).is_err());
        assert!(decode_wtxid_list(&[0]).is_err());
        let mut too_many = vec![[0u8; 32]; MAX_PACKAGE_WTXIDS + 1];
        for (i, wtxid) in too_many.iter_mut().enumerate() {
            wtxid[0] = i as u8;
            wtxid[1] = (i >> 8) as u8;
        }
        assert!(encode_wtxid_list(&too_many).is_err());
        assert!(encode_ancpkginfo(&[one, one]).is_err());
        let mut raw = encode_wtxid_list(&[one, two]).unwrap();
        raw[0] = 2;
        raw[33..65].copy_from_slice(&one);
        assert!(decode_ancpkginfo(&raw).is_err());
        assert_eq!(decode_wtxid_list(&raw).unwrap(), vec![one, one]);
    }

    #[test]
    fn package_hash_sorts_by_reversed_bytes_and_hashes_once() {
        let mut first = [0u8; 32];
        first[31] = 2;
        let mut second = [0u8; 32];
        second[31] = 1;
        let hash = package_hash(&[first, second]);
        let mut cat = Vec::new();
        cat.extend_from_slice(&second);
        cat.extend_from_slice(&first);
        let once = Sha256::digest(&cat);
        let mut expected = [0u8; 32];
        expected.copy_from_slice(once.as_ref());
        assert_eq!(hash, expected);
        assert_ne!(package_hash(&[first, second]), package_hash(&[second]));
    }

    #[test]
    fn pkgtxns_round_trips_inside_the_payload_cap() {
        use blvm_protocol::constants::SEQUENCE_FINAL;
        use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput};
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [3u8; 32],
                    index: 0,
                },
                script_sig: vec![0x51],
                sequence: SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 1_000,
                script_pubkey: vec![0x51],
            }]
            .into(),
            lock_time: 0,
        };
        let raw = blvm_protocol::serialization::serialize_transaction(&tx);
        let payload = encode_pkgtxns(&[raw.clone()]).unwrap();
        assert!(payload.len() <= MAX_PROTOCOL_PAYLOAD);
        assert_eq!(decode_pkgtxns(&payload).unwrap(), vec![raw]);
        assert_eq!(MAX_PROTOCOL_PAYLOAD, 4_000_000);
    }

    #[test]
    fn four_package_commands_round_trip() {
        use crate::network::protocol::{ProtocolMessage, ProtocolParser};
        use blvm_protocol::constants::SEQUENCE_FINAL;
        use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput};
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [4u8; 32],
                    index: 1,
                },
                script_sig: Vec::new(),
                sequence: SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 2_000,
                script_pubkey: vec![0x51],
            }]
            .into(),
            lock_time: 0,
        };
        let raw = blvm_protocol::serialization::serialize_transaction(&tx);
        let ancestors = vec![[1u8; 32], [2u8; 32]];
        let requested = vec![[3u8; 32]];
        let frames = [
            ProtocolParser::serialize_message(&ProtocolMessage::SendPackages(
                PACKAGE_VERSION_LOCAL,
            ))
            .unwrap(),
            ProtocolParser::serialize_message(&ProtocolMessage::AncPkgInfo(ancestors.clone()))
                .unwrap(),
            ProtocolParser::serialize_message(&ProtocolMessage::GetPkgTxns(requested.clone()))
                .unwrap(),
            ProtocolParser::serialize_message(&ProtocolMessage::PkgTxns(vec![raw.clone()]))
                .unwrap(),
        ];
        match ProtocolParser::parse_message(&frames[0]).unwrap() {
            ProtocolMessage::SendPackages(version) => assert_eq!(version, PACKAGE_VERSION_LOCAL),
            _ => panic!("sendpackages"),
        }
        match ProtocolParser::parse_message(&frames[1]).unwrap() {
            ProtocolMessage::AncPkgInfo(wtxids) => assert_eq!(wtxids, ancestors),
            _ => panic!("ancpkginfo"),
        }
        match ProtocolParser::parse_message(&frames[2]).unwrap() {
            ProtocolMessage::GetPkgTxns(wtxids) => assert_eq!(wtxids, requested),
            _ => panic!("getpkgtxns"),
        }
        match ProtocolParser::parse_message(&frames[3]).unwrap() {
            ProtocolMessage::PkgTxns(transactions) => assert_eq!(transactions, vec![raw]),
            _ => panic!("pkgtxns"),
        }
    }

    /// Payload bytes written out by hand. Decode runs before encode, so a bug
    /// shared by both functions cannot invent the expected bytes.
    #[test]
    fn known_payload_bytes_are_the_wire_encoding() {
        use crate::network::protocol::{ProtocolMessage, ProtocolParser};

        let send = hex("0300000000000000");
        assert_eq!(decode_sendpackages(&send).unwrap(), PACKAGE_VERSION_LOCAL);
        assert_eq!(
            encode_sendpackages(PACKAGE_VERSION_LOCAL).as_slice(),
            send.as_slice()
        );

        let one = [1u8; 32];
        let anc = hex("010101010101010101010101010101010101010101010101010101010101010101");
        assert_eq!(decode_ancpkginfo(&anc).unwrap(), vec![one]);
        assert_eq!(decode_wtxid_list(&anc).unwrap(), vec![one]);
        assert_eq!(encode_ancpkginfo(&[one]).unwrap(), anc);
        assert_eq!(encode_wtxid_list(&[one]).unwrap(), anc);

        let repeated = hex(&format!("02{}", "01".repeat(64)));
        assert!(decode_ancpkginfo(&repeated).is_err());
        assert_eq!(decode_wtxid_list(&repeated).unwrap(), vec![one, one]);

        let tx = hex(
            "01000000010303030303030303030303030303030303030303030303030303030303030303000000000151ffffffff01e803000000000000015100000000",
        );
        let pkg = hex(
            "0101000000010303030303030303030303030303030303030303030303030303030303030303000000000151ffffffff01e803000000000000015100000000",
        );
        assert_eq!(decode_pkgtxns(&pkg).unwrap(), vec![tx.clone()]);
        assert_eq!(encode_pkgtxns(&[tx]).unwrap(), pkg);

        let mut high = [0u8; 32];
        high[31] = 2;
        let mut low = [0u8; 32];
        low[31] = 1;
        let mut expected_hash = [0u8; 32];
        expected_hash.copy_from_slice(&hex(
            "d6ba9329f8932c12192b37849f772104d20048f76434a3290512d9d814e4116f",
        ));
        assert_eq!(package_hash(&[high, low]), expected_hash);

        let frames = [
            (
                hex("f9beb4d973656e647061636b6167657308000000f05d0d220300000000000000"),
                "sendpackages",
            ),
            (
                hex(
                    "f9beb4d9616e63706b67696e666f0000210000005c7c0c9a010101010101010101010101010101010101010101010101010101010101010101",
                ),
                "ancpkginfo",
            ),
            (
                hex(
                    "f9beb4d9676574706b6774786e730000210000005c7c0c9a010101010101010101010101010101010101010101010101010101010101010101",
                ),
                "getpkgtxns",
            ),
            (
                hex(
                    "f9beb4d9706b6774786e7300000000003f000000611be8720101000000010303030303030303030303030303030303030303030303030303030303030303000000000151ffffffff01e803000000000000015100000000",
                ),
                "pkgtxns",
            ),
        ];
        match ProtocolParser::parse_message(&frames[0].0).unwrap() {
            ProtocolMessage::SendPackages(version) => assert_eq!(version, 3),
            _ => panic!("{}", frames[0].1),
        }
        match ProtocolParser::parse_message(&frames[1].0).unwrap() {
            ProtocolMessage::AncPkgInfo(wtxids) => assert_eq!(wtxids, vec![one]),
            _ => panic!("{}", frames[1].1),
        }
        match ProtocolParser::parse_message(&frames[2].0).unwrap() {
            ProtocolMessage::GetPkgTxns(wtxids) => assert_eq!(wtxids, vec![one]),
            _ => panic!("{}", frames[2].1),
        }
        match ProtocolParser::parse_message(&frames[3].0).unwrap() {
            ProtocolMessage::PkgTxns(transactions) => assert_eq!(transactions, vec![tx_from_pkg()]),
            _ => panic!("{}", frames[3].1),
        }
    }

    #[test]
    fn boundary_vectors_reject_empty_lists_oversized_packages_and_a_zero_count_package() {
        assert!(decode_ancpkginfo(&[0]).is_err());
        assert!(decode_wtxid_list(&[0]).is_err());
        assert_eq!(decode_pkgtxns(&[0]).unwrap(), Vec::<Vec<u8>>::new());
        assert_eq!(encode_pkgtxns(&[]).unwrap(), vec![0]);
        assert!(decode_pkgtxns(&[0, 0]).is_err());
        assert!(decode_pkgtxns(&[1]).is_err());

        let mut too_many = vec![101u8];
        too_many.resize(1 + 101 * 32, 0);
        for i in 0..101 {
            too_many[1 + i * 32] = i as u8;
            too_many[2 + i * 32] = (i >> 8) as u8;
        }
        assert!(decode_wtxid_list(&too_many).is_err());
        assert!(decode_ancpkginfo(&too_many).is_err());

        let over = vec![0u8; MAX_PROTOCOL_PAYLOAD + 1];
        assert!(decode_pkgtxns(&over).is_err());
        assert!(encode_pkgtxns(&[vec![0u8; MAX_PROTOCOL_PAYLOAD]]).is_err());
    }

    fn tx_from_pkg() -> Vec<u8> {
        hex(
            "01000000010303030303030303030303030303030303030303030303030303030303030303000000000151ffffffff01e803000000000000015100000000",
        )
    }

    fn hex(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }
}
