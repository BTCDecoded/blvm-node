use super::*;
use crate::network::transport::TransportType;

#[test]
fn test_calculate_short_tx_id() {
    let tx_hash = [0u8; 32];
    let nonce = 12345u64;
    let header = BlockHeader {
        version: 1,
        prev_block_hash: [0; 32],
        merkle_root: [0; 32],
        timestamp: 0,
        bits: 0,
        nonce: 0,
    };
    let short_id = calculate_short_tx_id(&header, &tx_hash, nonce);

    // Short ID should be 6 bytes
    assert_eq!(short_id.len(), 6);
}

#[test]
fn short_id_uses_header_keys_and_witness_hash() {
    use blvm_protocol::opcodes::OP_1;

    let wtxid = [1u8; 32];
    let nonce = 7u64;
    let header = BlockHeader {
        version: 1,
        prev_block_hash: [2u8; 32],
        merkle_root: [3u8; 32],
        timestamp: 0,
        bits: 0,
        nonce: 0,
    };
    let other_header = BlockHeader {
        merkle_root: [4u8; 32],
        ..header.clone()
    };
    assert_ne!(
        calculate_short_tx_id(&header, &wtxid, nonce),
        calculate_short_tx_id(&other_header, &wtxid, nonce)
    );

    let tx = Transaction {
        version: 1,
        inputs: vec![blvm_protocol::TransactionInput {
            prevout: blvm_protocol::OutPoint {
                hash: [5u8; 32],
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![blvm_protocol::TransactionOutput {
            value: 50_000,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    };
    let block = Block {
        header: header.clone(),
        transactions: vec![tx.clone()].into_boxed_slice(),
    };
    let bare = create_compact_block_with_witnesses(&block, nonce, &HashSet::new(), &[]);
    let stacks = vec![vec![vec![OP_1]]];
    let with_witness = create_compact_block_with_witnesses(
        &block,
        nonce,
        &HashSet::new(),
        &[Some(stacks.clone())],
    );
    assert_ne!(bare.short_ids, with_witness.short_ids);
    let wtxid = crate::network::txhash::calculate_wtxid(&tx, Some(&stacks));
    assert_eq!(
        with_witness.short_ids,
        vec![calculate_short_tx_id(&header, &wtxid, nonce)]
    );
}

#[test]
fn test_reconstruct_block_empty_mempool() {
    let compact_block = CompactBlock {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0; 32],
            merkle_root: [0; 32],
            timestamp: 0,
            bits: 0,
            nonce: 0,
        },
        nonce: 0,
        short_ids: vec![[0u8; 6]],
        prefilled_txs: vec![],
    };

    let mempool_txs = HashMap::new();
    let missing = reconstruct_block(&compact_block, &mempool_txs).unwrap();

    // All transactions should be missing
    assert_eq!(missing.len(), 1);
}

#[test]
fn missing_short_id_keeps_the_block_index_after_a_prefilled_coinbase() {
    use blvm_protocol::constants::SEQUENCE_FINAL;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput};

    let tx = |prev: u8, value: i64| Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [prev; 32],
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    };
    let header = BlockHeader {
        version: 1,
        prev_block_hash: [0; 32],
        merkle_root: [0; 32],
        timestamp: 0,
        bits: 0,
        nonce: 0,
    };
    let block = Block {
        header,
        transactions: vec![tx(1, 50_000), tx(2, 40_000)].into_boxed_slice(),
    };
    let compact = create_compact_block(&block, 7, &HashSet::from([0]));
    assert_eq!(compact.prefilled_txs[0].0, 0);
    assert_eq!(compact.short_ids.len(), 1);

    let missing = reconstruct_block(&compact, &HashMap::new()).unwrap();
    assert_eq!(missing, vec![1]);
}

#[test]
fn prefilled_witness_survives_wire_conversion() {
    use blvm_protocol::constants::SEQUENCE_FINAL;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput};

    let tx = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [1u8; 32],
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 50_000,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    };
    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0; 32],
            merkle_root: [0; 32],
            timestamp: 0,
            bits: 0,
            nonce: 0,
        },
        transactions: vec![tx].into_boxed_slice(),
    };
    let witness = vec![vec![vec![OP_1]]];
    let compact = create_compact_block_with_witnesses(
        &block,
        7,
        &HashSet::from([0]),
        &[Some(witness.clone())],
    );
    assert_eq!(compact.prefilled_txs[0].2, Some(witness.clone()));

    let message = blvm_protocol::network::CmpctBlockMessage::try_from(compact).unwrap();
    assert_eq!(message.prefilled_txs[0].witness, Some(witness.clone()));

    let back = blvm_protocol::bip152::CompactBlock::from(&message);
    assert_eq!(back.prefilled_txs[0].2, Some(witness));
}

#[test]
fn ambiguous_short_id_is_requested() {
    use blvm_protocol::constants::SEQUENCE_FINAL;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput};

    let header = BlockHeader {
        version: 0,
        prev_block_hash: [0; 32],
        merkle_root: [0; 32],
        timestamp: 0,
        bits: 0,
        nonce: 0,
    };
    let nonce = 0u64;
    let mut first = [0u8; 32];
    first[24] = 0x99;
    first[25] = 0x13;
    first[26] = 0x4b;
    let mut second = [0u8; 32];
    second[24] = 0xd7;
    second[25] = 0x08;
    second[26] = 0x79;
    let shared = calculate_short_tx_id(&header, &first, nonce);
    assert_eq!(shared, calculate_short_tx_id(&header, &second, nonce));

    let unique = [9u8; 32];
    let unique_id = calculate_short_tx_id(&header, &unique, nonce);
    assert_ne!(unique_id, shared);

    let tx = |version: u64| Transaction {
        version,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [version as u8; 32],
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 50_000,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    };
    let mut pool = HashMap::new();
    pool.insert(first, tx(1));
    pool.insert(second, tx(2));
    pool.insert(unique, tx(3));
    let compact = CompactBlock {
        header,
        nonce,
        short_ids: vec![shared, unique_id],
        prefilled_txs: vec![],
    };
    let missing = reconstruct_block(&compact, &pool).unwrap();
    assert_eq!(missing, vec![0]);
}

#[test]
fn test_should_prefer_compact_blocks_tcp() {
    // TCP: compact blocks optional, not preferred by default
    assert!(!should_prefer_compact_blocks(TransportType::Tcp));
}

#[cfg(feature = "quinn")]
#[test]
fn test_should_prefer_compact_blocks_quinn() {
    // Quinn QUIC: prefer compact blocks
    assert_eq!(should_prefer_compact_blocks(TransportType::Quinn), true);
}

#[cfg(feature = "iroh")]
#[test]
fn test_should_prefer_compact_blocks_iroh() {
    // Iroh QUIC: definitely prefer compact blocks
    assert_eq!(should_prefer_compact_blocks(TransportType::Iroh), true);
}

#[test]
fn test_recommended_compact_block_version_tcp() {
    // TCP: version 1 is sufficient
    assert_eq!(recommended_compact_block_version(TransportType::Tcp), 1);
}

#[cfg(feature = "quinn")]
#[test]
fn test_recommended_compact_block_version_quinn() {
    // Quinn: version 2 for better optimization
    assert_eq!(recommended_compact_block_version(TransportType::Quinn), 2);
}

#[cfg(feature = "iroh")]
#[test]
fn test_recommended_compact_block_version_iroh() {
    // Iroh: version 2 for NAT traversal benefits
    assert_eq!(recommended_compact_block_version(TransportType::Iroh), 2);
}

#[test]
fn test_is_quic_transport_tcp() {
    // TCP is not QUIC
    assert!(!is_quic_transport(TransportType::Tcp));
}

#[cfg(feature = "quinn")]
#[test]
fn test_is_quic_transport_quinn() {
    // Quinn is QUIC
    assert_eq!(is_quic_transport(TransportType::Quinn), true);
}

#[cfg(feature = "iroh")]
#[test]
fn test_is_quic_transport_iroh() {
    // Iroh is QUIC
    assert_eq!(is_quic_transport(TransportType::Iroh), true);
}

#[test]
fn blocktxn_witness_fills_the_short_id_after_a_prefilled_coinbase() {
    use blvm_protocol::constants::SEQUENCE_FINAL;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput};

    let tx = |prev: u8| Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [prev; 32],
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 50_000,
            script_pubkey: vec![OP_1],
        }]
        .into(),
        lock_time: 0,
    };
    let header = BlockHeader {
        version: 1,
        prev_block_hash: [0; 32],
        merkle_root: [0; 32],
        timestamp: 0,
        bits: 0,
        nonce: 0,
    };
    let block = Block {
        header,
        transactions: vec![tx(1), tx(2)].into_boxed_slice(),
    };
    let compact = create_compact_block(&block, 7, &HashSet::from([0]));
    let mut assembly = begin_compact_assembly(&compact, &HashMap::new()).unwrap();
    assert_eq!(assembly.missing, vec![1]);
    let witness = vec![vec![vec![OP_1]]];
    apply_blocktxn(&mut assembly, &[tx(2)], Some(&[witness.clone()])).unwrap();
    let (reconstructed, witnesses) = completed_compact_block(&assembly).unwrap();
    assert_eq!(witnesses[1], witness);
    assert_eq!(
        reconstructed.transactions[1].outputs[0].script_pubkey,
        vec![OP_1]
    );
}

fn bare_compact(short_ids: Vec<ShortTxId>) -> CompactBlock {
    CompactBlock {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0; 32],
            merkle_root: [0; 32],
            timestamp: 0,
            bits: 0,
            nonce: 0,
        },
        nonce: 1,
        short_ids,
        prefilled_txs: vec![],
    }
}

fn open_budget() -> CompactBudget {
    CompactBudget {
        assemblies: 0,
        retained_bytes: 0,
        same_hash: false,
    }
}

#[test]
fn a_fifth_assembly_is_fetched_whole() {
    let compact = bare_compact(vec![[1u8; 6]]);
    let held = decide_compact_block(
        &compact,
        &HashMap::new(),
        CompactBudget {
            assemblies: MAX_PENDING_COMPACT,
            retained_bytes: 0,
            same_hash: false,
        },
    )
    .unwrap();
    assert!(matches!(held, CompactDecision::FullBlock));
    let replaced = decide_compact_block(
        &compact,
        &HashMap::new(),
        CompactBudget {
            assemblies: MAX_PENDING_COMPACT - 1,
            retained_bytes: 0,
            same_hash: true,
        },
    )
    .unwrap();
    assert!(matches!(replaced, CompactDecision::Hold(_)));
}

#[test]
fn retained_bytes_past_eight_mib_are_fetched_whole() {
    let compact = bare_compact(vec![[1u8; 6]]);
    let held = decide_compact_block(
        &compact,
        &HashMap::new(),
        CompactBudget {
            assemblies: 0,
            retained_bytes: MAX_PENDING_COMPACT_BYTES,
            same_hash: false,
        },
    )
    .unwrap();
    assert!(matches!(held, CompactDecision::FullBlock));
}

#[test]
fn a_replacement_past_the_byte_cap_keeps_the_current_assembly() {
    let compact = bare_compact(vec![[1u8; 6]]);
    let kept = decide_compact_block(
        &compact,
        &HashMap::new(),
        CompactBudget {
            assemblies: 0,
            retained_bytes: MAX_PENDING_COMPACT_BYTES,
            same_hash: true,
        },
    )
    .unwrap();
    assert!(matches!(kept, CompactDecision::KeepCurrent));
}

#[test]
fn more_than_128_holes_are_fetched_whole() {
    let compact = bare_compact(vec![[2u8; 6]; MAX_COMPACT_MISSING + 1]);
    let held = decide_compact_block(&compact, &HashMap::new(), open_budget()).unwrap();
    assert!(matches!(held, CompactDecision::FullBlock));
    let within = bare_compact(vec![[2u8; 6]; MAX_COMPACT_MISSING]);
    let held = decide_compact_block(&within, &HashMap::new(), open_budget()).unwrap();
    assert!(matches!(held, CompactDecision::Hold(_)));
}

#[test]
fn more_than_100_000_transactions_are_fetched_whole() {
    let compact = bare_compact(vec![[3u8; 6]; MAX_COMPACT_BLOCK_TXS + 1]);
    let held = decide_compact_block(&compact, &HashMap::new(), open_budget()).unwrap();
    assert!(matches!(held, CompactDecision::FullBlock));
}

#[test]
fn an_index_past_u16_is_fetched_whole() {
    let compact = bare_compact(vec![[4u8; 6]; (u16::MAX as usize) + 2]);
    let held = decide_compact_block(&compact, &HashMap::new(), open_budget()).unwrap();
    assert!(matches!(held, CompactDecision::FullBlock));
}

#[test]
fn an_assembly_older_than_sixty_seconds_is_expired() {
    let compact = bare_compact(vec![[5u8; 6]]);
    let CompactDecision::Hold(mut assembly) =
        decide_compact_block(&compact, &HashMap::new(), open_budget()).unwrap()
    else {
        panic!("one hole fits the budget");
    };
    let now = Instant::now();
    assert!(!assembly.expired_at(now));
    assembly.started = now - COMPACT_ASSEMBLY_DEADLINE;
    assert!(assembly.expired_at(now));
}

#[test]
fn short_id_lookup_clones_only_the_match() {
    use crate::node::mempool::MempoolManager;
    use blvm_protocol::constants::SEQUENCE_FINAL;
    use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput};
    use std::sync::Arc;

    let tx = |prev: u8| Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [prev; 32],
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: SEQUENCE_FINAL as u64,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 10_000,
            script_pubkey: vec![0x6a],
        }]
        .into(),
        lock_time: 0,
    };
    let mempool = MempoolManager::new();
    let mut policy = crate::config::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    policy.min_relay_fee_rate = 0;
    mempool.set_policy_config(Some(policy));
    for prev in [1u8, 2u8] {
        let tx = tx(prev);
        let mut utxo = blvm_protocol::UtxoSet::default();
        utxo.insert(
            tx.inputs[0].prevout,
            Arc::new(blvm_protocol::UTXO {
                value: 20_000,
                script_pubkey: vec![0x51].into(),
                height: 1,
                is_coinbase: false,
            }),
        );
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo)));
        assert!(
            mempool.add_transaction(tx).unwrap(),
            "prev {prev} was not admitted"
        );
    }
    assert_eq!(mempool.get_transactions().len(), 2);
    let header = BlockHeader {
        version: 1,
        prev_block_hash: [0; 32],
        merkle_root: [0; 32],
        timestamp: 0,
        bits: 0,
        nonce: 0,
    };
    let nonce = 11u64;
    let wanted = {
        let txs = mempool.get_transactions();
        let wtxid = crate::network::txhash::calculate_wtxid(&txs[0], None);
        calculate_short_tx_id(&header, &wtxid, nonce)
    };
    let matched = mempool.transactions_for_short_ids(&header, nonce, &[wanted]);
    assert_eq!(matched.len(), 1);
    let _ = wanted;
}
