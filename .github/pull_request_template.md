<!--
GitHub hides this comment on the rendered pull request.

Replace each `_`. `none` and `no` are real answers. Delete a line only when it does not apply, and say why on that line.

- Class: `specification`, `validation`, `serialization`, `crypto`, `protocol`, `node`, `verification`, `release`, `docs`, or `other`.
- Consensus behavior: `yes` only when this change can accept a block or transaction the network would reject, or reject one the network would accept. Name the rule. A refactor that must not do that is `no`.
- What a wrong merge does: one sentence. Name the bad outcome.
- Spec: Orange Paper clause, BIP, or `none`.
- Regression test: path and test name. `none` needs a reason. Consensus behavior `yes` with regression test `none` is the line to stop on.
- Sibling repositories: other BTCDecoded repositories this forces a change in, or `none`.
- Public claim: `yes` if the diff edits a homepage, white paper, README status line, `.github/SYSTEM_STATUS.md`, or a sentence that says governance is enforced or solved. Name the file.
- Emergency: `no`, or `tier_1_critical`, `tier_2_urgent`, or `tier_3_elevated` from `BTCDecoded/governance` `config/emergency-tiers.yml`, plus the activation record. There is no one-maintainer timeout.

Enforcement is off. The written signature count and review period are not collected here.
-->

## Governance

- **Layer:** Layer 4, `blvm-node`. If a consensus-adjacent path is named below, say consensus-adjacent on this line too.
- **Class:** _
- **Consensus behavior:** _
- **What a wrong merge does:** _
- **Spec:** _
- **Regression test:** _
- **Sibling repositories:** _
- **Public claim:** _
- **Emergency:** _

## This repository

- **Consensus-adjacent paths:** none
- **Split or stall:** _

<!-- Replace `none` with every path this diff touches from this list: `src/storage/utxostore.rs`, `src/storage/disk_utxo.rs`, `src/storage/ibd_utxo_store.rs`, `src/node/reorg_executor.rs`, `src/storage/blockstore.rs`, `src/node/sync.rs`. Naming one means the change is consensus-adjacent. The layer 4 count stays 3 of 5 and 60 days. Split or stall: `chain split`, `sync or reorg stall`, or `local only`. -->

## Reviewer focus

- _
- _

Enforcement is off. This record does not block the merge.
