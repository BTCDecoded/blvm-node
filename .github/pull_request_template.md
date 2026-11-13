## Governance

- **Layer:** Layer 4, `blvm-node`. If this touches `src/storage/utxostore.rs`, `src/storage/disk_utxo.rs`, `src/storage/ibd_utxo_store.rs`, `src/node/reorg_executor.rs`, `src/storage/blockstore.rs`, or `src/node/sync.rs`, say consensus-adjacent.
- **Consensus behavior:** Does this change consensus behavior? yes / no
- **Regression test:** Which test covers that behavior?
- **Emergency:** Is this an emergency-tier change? If yes, cite the tier in `BTCDecoded/governance` `config/emergency-tiers.yml` and the activation record.

This records the classification. Enforcement is off. The template does not block the merge.
