# Fork choice and chain reorganization

How **blvm-node** selects the active tip and performs reorgs, and how that relates to **blvm-consensus**.

## Split of responsibility

| Layer | Role |
|-------|------|
| **blvm-consensus** (`reorganization.rs`) | Pure disconnect/connect: given `current_chain` and `new_chain` block vectors, a UTXO set, and undo logs, returns the new UTXO and disconnected/connected blocks. No block tree, no tip policy. |
| **blvm-node** (`chain_selector`, `reorg_executor`, `sync`) | Block index, cumulative chainwork per hash, fork-choice policy, undo persistence, tip/`ChainInfo` updates, module events. |

The node does **not** call `should_reorganize` for live fork choice. That helper compares two block **vectors** (length and segment work), not tip `nChainWork` on a block tree.

## Fork-choice policy (production)

1. **Parent resolution** — `SyncCoordinator::process_block` resolves connect height from the block index (`parent` link), not from the caller’s height alone.
2. **Store** — Valid blocks are stored with witnesses, undo logs, and block-index entries (`record_connected_block`).
3. **Extend vs side chain** — If `parent == active_tip`, the block extends the active chain (tip + UTXO updated).
4. **Activation** — Otherwise `reorg_executor::try_activate_heavier_fork` runs when `chain_selector::should_activate_over_active_tip` is true:
   - strictly greater cumulative chainwork at the candidate tip, or
   - equal chainwork and **lower** `sequence_id` (first-connected block wins among siblings).
5. **Reorg execution** — Builds chains from the block index, calls `reorganize_chain_with_witnesses`, refreshes height index, publishes `BlockDisconnected` (tip-first) and `ChainReorg`.

## Block index (`storage/block_index.rs`)

- Hash-keyed entries: `height`, `prev_hash`, `status`, `sequence_id`.
- `sequence_id` is assigned once per hash on first insert (monotonic counter); used for equal-work tie-break.
- `chain_tips()` enumerates tips for `getchaintips` RPC.

## Undo logs

- Persisted in blockstore (`store_undo_log` / `get_undo_log`, `production` feature).
- Required for disconnect during reorg; written on every connect in the fork-choice path.

## Events

- `EventPublisher` on `SyncCoordinator` (set at node startup with mempool/network).
- Reorg path emits the same payload shapes as `invalidateblock` (`BlockDisconnected`, `ChainReorg`).

## Tests

| Test | Covers |
|------|--------|
| `tests/fork_reorg_storage.rs` | Height index last-write-wins; both hashes retrievable |
| `tests/fork_reorg_regtest.rs` | Linear control; higher-work mid-chain reorg |
| `tests/fork_reorg_chaintips.rs` | `getchaintips` with active + orphan |
| `tests/fork_reorg_events.rs` | Reorg module events |
| `tests/fork_reorg_equal_work.rs` | Equal-work sibling tie-break; child on lagging fork |
| `src/node/chain_selector.rs` (unit) | Selector comparisons |
| `src/storage/block_index.rs` (unit) | `sequence_id` assignment |

## Out of scope

- `preciousblock` RPC (Core testing hook; not implemented).
- Parallel IBD fork reorgs (download stays linear; every flushed block is indexed in the block tree).
- Consensus rule changes.

## Related code

- `SyncCoordinator::connect_block_wire` / `connect_mined_block` — shared connect path (P2P, `generatetoaddress`)
- `src/node/sync.rs` — `process_block_with_fork_choice`, `connect_mined_block`
- `src/node/parallel_ibd/blocks.rs` — `index_ibd_flushed_blocks` during batch flush
- `src/node/chain_selector.rs` — chainwork / sequence comparison
- `src/node/reorg_executor.rs` — activation + events
- `src/storage/chainstate.rs` — tip, chainwork cache, `index_connected_block`
