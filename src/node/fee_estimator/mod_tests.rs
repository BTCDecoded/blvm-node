use super::*;
use blvm_protocol::{BlockHeader, OutPoint, Transaction, TransactionInput, TransactionOutput};

fn observation(height: u64) -> BlockObservation {
    BlockObservation {
        height,
        hash: [height as u8; 32],
        weight: 4_000,
        tx_weights: Vec::new(),
    }
}

#[test]
fn duplicate_height_is_ignored_and_a_gap_is_counted() {
    let est = FeeEstimator::new(FeeEstimatorConfig::default(), true);
    est.on_block_connected(&observation(10));
    est.on_block_connected(&observation(10));
    est.on_block_connected(&observation(9));
    est.on_block_connected(&observation(12));
    let counts = est.hook_counts();
    assert_eq!(counts.blocks, 2);
    assert_eq!(counts.ignored_blocks, 2);
    assert_eq!(counts.gap_clears, 1);
}

#[test]
fn trace_keeps_hook_order() {
    let est = FeeEstimator::new(FeeEstimatorConfig::default(), false);
    est.enable_hook_trace();
    let txid = [7u8; 32];
    est.on_tx_admitted(TxAdmission {
        txid,
        fee_sats: 100,
        vsize: 100,
        admit_height: 1,
        parents: Vec::new(),
        from_reload: false,
        from_package: false,
    });
    est.on_block_connected(&observation(2));
    est.on_tx_removed(&txid);
    est.on_pool_cleared();
    assert_eq!(
        est.hook_events(),
        vec![
            HookEvent::Admitted {
                txid,
                from_reload: false,
                from_package: false,
            },
            HookEvent::Block { height: 2 },
            HookEvent::Removed { txid },
            HookEvent::Cleared,
        ]
    );
}

fn coinbase() -> Transaction {
    Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [0u8; 32],
                index: 0xffffffff,
            },
            script_sig: vec![1],
            sequence: 0xffffffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 50_0000_0000,
            script_pubkey: vec![0x51].into(),
        }]
        .into(),
        lock_time: 0,
    }
}

fn spend() -> Transaction {
    Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [1u8; 32],
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: 0xffffffff,
        }]
        .into(),
        outputs: vec![TransactionOutput {
            value: 1000,
            script_pubkey: vec![0x51].into(),
        }]
        .into(),
        lock_time: 0,
    }
}

#[test]
fn observation_skips_the_coinbase_and_keeps_block_weight() {
    let coinbase = coinbase();
    let spend = spend();
    let spend_id = blvm_protocol::block::calculate_tx_id(&spend);
    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1_700_000_000,
            bits: 0x1d00ffff,
            nonce: 0,
        },
        transactions: vec![coinbase, spend].into_boxed_slice(),
    };
    let witnesses = vec![vec![vec![vec![0u8; 32]]], vec![Vec::new()]];
    let obs = BlockObservation::from_block(&block, &witnesses, 1, [9u8; 32]).unwrap();
    assert_eq!(obs.tx_weights.len(), 1);
    assert_eq!(obs.tx_weights[0].0, spend_id);
    assert!(obs.tx_weights[0].1 > 0);
    assert!(obs.weight > obs.tx_weights[0].1);
}

fn admission(fee_sats: u64, vsize: u64) -> TxAdmission {
    TxAdmission {
        txid: [4u8; 32],
        fee_sats,
        vsize,
        admit_height: 1,
        parents: Vec::new(),
        from_reload: false,
        from_package: false,
    }
}

fn open_cfg() -> FeeEstimatorConfig {
    let mut cfg = FeeEstimatorConfig::default();
    cfg.health_required = Some(false);
    cfg.projection_full_threshold = 0.0;
    cfg.allow_mempool_only = true;
    cfg.multiplier = 1.0;
    cfg
}

#[test]
fn staleness_waits_for_a_new_block_and_serves_a_young_table() {
    let now = Instant::now();
    let fresh = now.checked_sub(Duration::from_secs(1)).unwrap();
    let interval = Duration::from_secs(7);
    assert_eq!(
        table_action(1, 2, 1, 1, fresh, now, interval),
        TableAction::Wait
    );
    assert_eq!(
        table_action(1, 1, 1, 4, fresh, now, interval),
        TableAction::Serve
    );
    let aging = now.checked_sub(Duration::from_secs(10)).unwrap();
    assert_eq!(
        table_action(1, 1, 1, 4, aging, now, interval),
        TableAction::Background
    );
    let stale = now.checked_sub(Duration::from_secs(60)).unwrap();
    assert_eq!(
        table_action(1, 1, 1, 4, stale, now, interval),
        TableAction::Wait
    );
    assert_eq!(
        table_action(1, 1, 4, 4, stale, now, interval),
        TableAction::Serve
    );
}

#[tokio::test]
async fn a_new_block_is_never_served_from_the_previous_table() {
    let est = FeeEstimator::new(open_cfg(), true);
    let first = est.table().await.unwrap();
    assert_eq!(first.block_seq, 0);
    est.on_tx_admitted(admission(5_000, 100));
    let during_interval = est.table().await.unwrap();
    assert!(Arc::ptr_eq(&first, &during_interval));
    est.on_block_connected(&observation(1));
    let second = est.table().await.unwrap();
    assert_eq!(second.block_seq, 1);
    assert_eq!(second.tip_height, 1);
    assert!(!Arc::ptr_eq(&first, &second));
    let quote = est
        .quote(1, FeeMode::Conservative, SourceView::Combined)
        .await
        .unwrap();
    assert_eq!(quote.sat_per_kvb, 50_000);
    assert_eq!(quote.source, FeeSource::Mempool);
    assert_eq!(quote.blocks, 1);
}

#[tokio::test]
async fn quotes_follow_the_health_gate_and_an_empty_or_disabled_estimator() {
    let regtest = FeeEstimator::new(FeeEstimatorConfig::default(), true);
    assert_eq!(
        regtest
            .quote(1, FeeMode::Conservative, SourceView::Combined)
            .await,
        Err(FeeUnavailable::EmptyMempool)
    );

    let mainnet = FeeEstimator::new(FeeEstimatorConfig::default(), false);
    assert_eq!(
        mainnet
            .quote(1, FeeMode::Conservative, SourceView::Combined)
            .await,
        Err(FeeUnavailable::WarmingUp)
    );

    let mut disabled = open_cfg();
    disabled.enabled = false;
    let disabled = FeeEstimator::new(disabled, true);
    assert_eq!(
        disabled.table().await.unwrap_err(),
        FeeUnavailable::Disabled
    );

    let mut strict = open_cfg();
    strict.health_required = Some(true);
    strict.health_window_blocks = 1;
    let strict = FeeEstimator::new(strict, false);
    strict.on_tx_admitted(admission(5_000, 100));
    strict.on_block_connected(&observation(1));
    assert_eq!(
        strict
            .quote(1, FeeMode::Conservative, SourceView::Combined)
            .await,
        Err(FeeUnavailable::MempoolUnhealthy)
    );

    let mut no_only = open_cfg();
    no_only.allow_mempool_only = false;
    let no_only = FeeEstimator::new(no_only, true);
    no_only.on_tx_admitted(admission(5_000, 100));
    no_only.on_block_connected(&observation(1));
    assert_eq!(
        no_only
            .quote(1, FeeMode::Conservative, SourceView::Combined)
            .await,
        Err(FeeUnavailable::WarmingUp)
    );
    assert_eq!(
        no_only
            .quote(1, FeeMode::Conservative, SourceView::Mempool)
            .await
            .unwrap()
            .sat_per_kvb,
        50_000
    );
}

#[tokio::test]
async fn external_store_keeps_native_quotes_and_announces_one_table_per_tip() {
    let est = FeeEstimator::new(open_cfg(), true);
    let first = observation(1);
    est.on_tx_admitted(admission(5_000, 100));
    est.on_block_connected(&first);
    let events = Arc::new(crate::module::api::events::EventManager::new());
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    events
        .subscribe_module(
            "listener".into(),
            vec![crate::module::traits::EventType::FeeTableReady],
            tx,
        )
        .await
        .unwrap();
    est.set_event_publisher(Some(Arc::new(
        crate::node::event_publisher::EventPublisher::new(Arc::clone(&events)),
    )));
    let id = "fees_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let registered = est.register_estimators(
        id,
        vec![FeeEstimatorSpec {
            name: "blend".to_string(),
            description: "blend".to_string(),
            modes: vec![FeeMode::Conservative],
            default_merge: ExternalMode::LowerOnly,
            auto_priority: Some(3),
        }],
    );
    assert_eq!(registered.accepted, vec!["blend".to_string()]);
    let generation = est
        .submit_fee_table(
            id,
            ExternalFeeTable {
                estimator: "blend".to_string(),
                tip_hash: first.hash,
                upstream_height: 1,
                mode: None,
                quotes: vec![(1, 1)],
            },
            Instant::now(),
        )
        .unwrap();
    assert!(generation >= 1);

    let (_tip, _height, _generation, quotes) = est
        .native_fee_table(SourceView::Combined, FeeMode::Conservative, &[1])
        .await
        .unwrap();
    let quote = quotes[0].1.as_ref().expect("native quote");
    assert_eq!(quote.sat_per_kvb, 50_000);
    assert_eq!(quote.estimator, "native");

    let message = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("FeeTableReady")
        .expect("channel");
    let crate::module::ipc::protocol::ModuleMessage::Event(event) = message else {
        panic!("expected an event");
    };
    assert_eq!(
        event.event_type,
        crate::module::traits::EventType::FeeTableReady
    );
    est.mark_table_stale_for_test();
    let _ = est
        .quote(1, FeeMode::Conservative, SourceView::Combined)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .is_err()
    );

    est.on_block_connected(&observation(2));
    let mut named = open_cfg();
    named.estimator = "blend".to_string();
    est.configure(named, true).unwrap();
    let selected = est.select_estimator(Instant::now()).unwrap();
    assert!(selected.degraded);
    assert_eq!(selected.active, "native");
    est.on_reorg();
    assert!(est.select_estimator(Instant::now()).unwrap().degraded);
    est.drop_module_estimates(id);
    let gone = est.select_estimator(Instant::now()).unwrap();
    assert!(gone.unresolved);
}

fn blend_spec(mode: ExternalMode) -> FeeEstimatorSpec {
    FeeEstimatorSpec {
        name: "blend".to_string(),
        description: "blend".to_string(),
        modes: vec![FeeMode::Conservative],
        default_merge: mode,
        auto_priority: Some(2),
    }
}

#[tokio::test]
async fn auto_without_consent_stays_native_and_a_merge_cannot_beat_the_floor() {
    let id = "fees_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let denied = FeeEstimator::new(open_cfg(), true);
    let tip = observation(1);
    denied.on_tx_admitted(admission(5_000, 100));
    denied.on_block_connected(&tip);
    denied.register_estimators(id, vec![blend_spec(ExternalMode::LowerOnly)]);
    denied
        .submit_fee_table(
            id,
            ExternalFeeTable {
                estimator: "blend".to_string(),
                tip_hash: tip.hash,
                upstream_height: 1,
                mode: None,
                quotes: vec![(1, 1)],
            },
            Instant::now(),
        )
        .unwrap();
    let native = denied
        .quote(1, FeeMode::Conservative, SourceView::Combined)
        .await
        .unwrap();
    assert_eq!(native.sat_per_kvb, 50_000);
    assert_eq!(native.estimator, "native");
    assert!(
        denied
            .fee_estimator_rows()
            .iter()
            .any(|row| row.name == "blend" && !row.active && row.fresh)
    );

    let mut allowed_cfg = open_cfg();
    allowed_cfg.allow_auto_override = vec![id.to_string()];
    let allowed = FeeEstimator::new(allowed_cfg, true);
    allowed.on_tx_admitted(admission(5_000, 100));
    allowed.on_block_connected(&tip);
    allowed.register_estimators(id, vec![blend_spec(ExternalMode::LowerOnly)]);
    allowed
        .submit_fee_table(
            id,
            ExternalFeeTable {
                estimator: "blend".to_string(),
                tip_hash: tip.hash,
                upstream_height: 1,
                mode: None,
                quotes: vec![(1, 1)],
            },
            Instant::now(),
        )
        .unwrap();
    let lowered = allowed
        .quote(1, FeeMode::Conservative, SourceView::Combined)
        .await
        .unwrap();
    assert_eq!(lowered.sat_per_kvb, 100);
    assert_eq!(lowered.source, FeeSource::ExternalMin);
    assert_eq!(lowered.estimator, "blend");
    assert!(
        allowed
            .fee_estimator_rows()
            .iter()
            .any(|row| row.name == "native" && !row.active)
    );

    let mut replace_cfg = open_cfg();
    replace_cfg.estimator = "blend".to_string();
    replace_cfg.external.by_name.insert(
        "blend".to_string(),
        crate::config::ExternalEstimatorOverride {
            mode: Some("replace".to_string()),
        },
    );
    let replace = FeeEstimator::new(replace_cfg, true);
    replace.on_tx_admitted(admission(5_000, 100));
    replace.on_block_connected(&tip);
    replace.register_estimators(id, vec![blend_spec(ExternalMode::LowerOnly)]);
    replace
        .submit_fee_table(
            id,
            ExternalFeeTable {
                estimator: "blend".to_string(),
                tip_hash: tip.hash,
                upstream_height: 1,
                mode: None,
                quotes: vec![(1, 80_000)],
            },
            Instant::now(),
        )
        .unwrap();
    let replaced = replace
        .quote(1, FeeMode::Conservative, SourceView::Combined)
        .await
        .unwrap();
    assert_eq!(replaced.sat_per_kvb, 80_000);
    assert_eq!(replaced.source, FeeSource::External);
    assert_eq!(replaced.estimator, "blend");

    let mut strict = open_cfg();
    strict.estimator = "blend".to_string();
    strict.fallback = "none".to_string();
    let strict = FeeEstimator::new(strict, true);
    assert_eq!(
        strict
            .quote(1, FeeMode::Conservative, SourceView::Combined)
            .await,
        Err(FeeUnavailable::ExternalStale)
    );
}
