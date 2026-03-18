# Database Backend Parity

All storage backends (redb, rocksdb, sled) must behave identically for the Database abstraction.

## Tree Names

**Known trees** are defined in `database::KNOWN_TREE_NAMES`. All backends support exactly this set.

**Module storage** has been removed from the node DB. Modules use their own DB at `{data_dir}/db/` via `blvm_sdk::module::open_module_db`. Requests for `module_*` or `modules` trees return an error.

## Implementation Summary

| Aspect | redb | RocksDB | Sled |
|--------|------|---------|------|
| Known trees | Pre-defined TableDefinitions | Pre-created column families at open | `open_tree` creates on demand |
| Module trees | Rejected (error) | Rejected (error) | Rejected (error) |
| Unknown names | Rejected (get_table_def returns None) | Create CF on demand (tests, WAL) | Create tree on demand |
| Reopen | N/A | list_cf + merge for existing DBs with extra CFs | N/A |

## Adding a New Backend (e.g. TidesDB)

1. Use `KNOWN_TREE_NAMES` for the tree/CF list.
2. Reject `module_*` and `modules` tree names with an error.
3. Implement `Database` and `Tree` traits following the same semantics.
