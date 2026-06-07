//! `create_chunks` / parallel IBD chunk assignment invariants.

use blvm_node::node::parallel_ibd::{ParallelIBD, ParallelIBDConfig};

fn ibd_with_chunk_size(chunk_size: u64) -> ParallelIBD {
    let mut config = ParallelIBDConfig::default();
    config.chunk_size = chunk_size;
    ParallelIBD::new(config)
}

#[test]
fn test_create_chunks_bootstrap_from_genesis() {
    let ibd = ibd_with_chunk_size(128);
    let peers = vec!["peer-a".into()];
    let chunks = ibd.create_chunks(0, 500, &peers, None);
    assert!(!chunks.is_empty());
    assert_eq!(chunks[0].start_height, 0);
    assert!(
        chunks[0].end_height >= 99,
        "bootstrap chunk must cover height 99"
    );
    assert_eq!(chunks[0].peer_id, "peer-a");
    for w in chunks.windows(2) {
        assert_eq!(w[1].start_height, w[0].end_height + 1);
    }
}

#[test]
fn test_create_chunks_round_robin_assigns_peers() {
    let ibd = ibd_with_chunk_size(64);
    let peers = vec!["p1".into(), "p2".into()];
    let chunks = ibd.create_chunks(200, 400, &peers, None);
    assert!(chunks.len() >= 2);
    let p1 = chunks.iter().filter(|c| c.peer_id == "p1").count();
    let p2 = chunks.iter().filter(|c| c.peer_id == "p2").count();
    assert!(p1 > 0 && p2 > 0, "round-robin should use both peers");
}

#[test]
fn test_create_chunks_earliest_first_uses_fastest_peer() {
    let mut config = ParallelIBDConfig::default();
    config.chunk_size = 64;
    config.mode = "earliest".to_string();
    config.earliest_first = true;
    let ibd = ParallelIBD::new(config);
    let peers = vec!["slow".into(), "fast".into()];
    let scored = vec![("slow".into(), 1.0), ("fast".into(), 100.0)];
    let chunks = ibd.create_chunks(200, 400, &peers, Some(&scored));
    assert!(!chunks.is_empty());
    assert!(
        chunks.iter().all(|c| c.peer_id == "fast"),
        "earliest-first assigns all chunks to fastest peer"
    );
}

#[test]
fn test_create_chunks_resume_mid_chain_no_bootstrap() {
    let ibd = ibd_with_chunk_size(128);
    let peers = vec!["p1".into()];
    let chunks = ibd.create_chunks(10_000, 10_300, &peers, None);
    assert!(!chunks.is_empty());
    assert_eq!(chunks[0].start_height, 10_000);
    assert!(chunks[0].end_height <= 10_127);
}

#[test]
fn test_create_chunks_single_peer_gets_all_work() {
    let ibd = ibd_with_chunk_size(32);
    let peers = vec!["solo".into()];
    let chunks = ibd.create_chunks(0, 100, &peers, None);
    assert!(!chunks.is_empty());
    assert!(
        chunks.iter().all(|c| c.peer_id == "solo"),
        "single peer should own every chunk"
    );
}

#[test]
fn test_create_chunks_empty_peer_list_uses_placeholder() {
    let ibd = ibd_with_chunk_size(64);
    let chunks = ibd.create_chunks(0, 128, &[], None);
    assert!(!chunks.is_empty());
    assert_eq!(chunks[0].start_height, 0);
}
