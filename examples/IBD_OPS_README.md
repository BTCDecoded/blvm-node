# IBD ops examples (lab / rematch)

Lab and soak helpers for parallel IBD checkpoints and body trees. Build with
`cargo run -p blvm-node --example <name> --features production,heed3`.

**Hard rule:** never wipe archives, GOLDEN pins, or `blvm.a31-frontier` unless
explicitly ordered in the current session.

## Rematch-safe

| Example | Role |
|---------|------|
| `strip_bodies_above` | Tip90 / TRUE-WAN: strip bodies above export height (`STRIP=` in `wan-bench-archive-tip90.sh`) |
| `inspect_local_block` | Opt-in local body inspection (tip90 can call) |

## Lab / destructive

| Example | Role |
|---------|------|
| `copy_ibd_ckpt` | Copy active IBD UTXO ckpt tree between data dirs |
| `clear_ckpt_slot` | Wipe one ping-pong ckpt slot |
| `fill_empty_ckpt_slot` | Copy SRC active ckpt into an empty DST slot |
| `recover_ckpt_slot` | Recover a ckpt slot from salvage paths |
| `probe_ckpt_utxo` | Probe ckpt UTXO (synth-wan helper) |
| `set_ibd_watermark` | Set IBD watermark metadata |
| `backfill_engine_export_muhash` | Backfill engine export muhash |

Doc-comment paths use `$DATADIR` placeholders. Binary names are stable for harness scripts.
