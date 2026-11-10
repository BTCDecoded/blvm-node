# Bitcoin Core test fixtures

## `core-regtest-101/`

Real Bitcoin Core regtest datadir (101 blocks) for Core drop-in migration tests.

**Not committed** — generate locally or in CI:

```bash
cd blvm-node
./scripts/gen-core-regtest-fixture.sh
```

Requires `bitcoind` and `bitcoin-cli` on `PATH`, or set `BITCOIND` / `BITCOIN_CLI`
(e.g. workspace `bitcoin-core`: `cmake --build build --target bitcoind bitcoin-cli`, then
`BITCOIND=../../bitcoin-core/build/bin/bitcoind BITCOIN_CLI=../../bitcoin-core/build/bin/bitcoin-cli`).

Override output path:

```bash
./scripts/gen-core-regtest-fixture.sh /path/to/fixture
```

Or point tests at an existing Core datadir:

```bash
export BLVM_CORE_REGTEST_FIXTURE=/path/to/regtest/datadir
cargo test -p blvm-node --features rocksdb core_drop_in
```

After generation, `fixture.json` records expected height and tip hash from `bitcoin-cli`.
