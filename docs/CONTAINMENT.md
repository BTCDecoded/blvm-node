# Containment

Use this when a running node must stop, and when it is safe to start it again.

## Stop

1. Send SIGTERM to the `blvm` process. Wait until the process is gone.
2. If it is still running after 60 seconds, send SIGKILL.
3. Leave `ibd.log` and the chain directory in place. Do not delete `data.mdb`, an undo log, or a checkpoint to make the stop "clean".

## Restore a checkpoint

1. Confirm the process is gone (`pgrep -a blvm` prints nothing).
2. Point the next start at the last checkpoint the node wrote under its datadir (`ibd_utxos_ckpt_*` or the storage snapshot that start recorded). Do not copy a checkpoint over the only remaining copy.
3. Start `blvm` with the same `--network` and the same datadir.

## A healthy tip

- `getblockchaininfo` returns a chain name that matches `--network`.
- `initialblockdownload` is false once headers and blocks have caught the network, and `blocks` equals `headers`.
- The log is still printing new block heights. A frozen height with no process is a dead node, not a healthy tip.
