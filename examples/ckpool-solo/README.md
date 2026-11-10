# ckpool solo mining smoke test (optional)

Prerequisites: built `blvm`, built [ckpool](https://github.com/mempool/ckpool), regtest or synced mainnet datadir.

## 1. Node config (`blvm.toml`)

```toml
[rpc_auth]
required = true
username = "ckpool"
password = "change-me"   # auto-granted admin (GBT/submitblock); use a strong random secret
```

Use `--rpc-addr 127.0.0.1:18443` (regtest default) or `127.0.0.1:8332` (mainnet). Do not expose RPC to the LAN without TLS.

## 2. Start node

```bash
blvm --network regtest --config blvm.toml --rpc-addr 127.0.0.1:18443
```

Regtest: seed chain with `generatetoaddress` (RPC) before ckpool starts.

## 3. ckpool (`ckpool.conf`)

```json
{
  "btcd": [{
    "url": "127.0.0.1:18443",
    "auth": "ckpool",
    "pass": "change-me",
    "notify": true
  }],
  "mindiff": 1,
  "startdiff": 42
}
```

```bash
/path/to/ckpool/src/ckpool -B -c ckpool.conf
```

## 4. Automated checks (no ckpool binary required)

**Integration tests** (regtest, in-process):

```bash
cargo test --test ckpool_integration_tests --features production
```

Covers GBT shape, HTTP Basic auth, `validateaddress`, chain-tip RPCs, block-hash display order (Core `GetHex`), and `submitblock` with a mined regtest block.

**Live node smoke** (node must be running):

```bash
./examples/ckpool-solo/smoke-rpc.sh
```

## 5. Bitaxe / Stratum miner

- Pool: `<host>:3333`
- User: your `bc1…` payout address
- Pass: `x`
