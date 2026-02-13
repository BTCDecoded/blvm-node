# Database Backend Parity

All storage backends (redb, rocksdb, sled) must behave identically for the Database abstraction.

## Tree Names

**Known trees** are defined in `database::KNOWN_TREE_NAMES`. All backends support exactly this set for non-module storage.

**Module trees** (`module_{id}_{name}`) use a shared "modules" table/CF with key prefix `module_{id}_{name}_` for isolation. Redb, RocksDB, and Sled all implement this via a `ModuleTree` wrapper; data is stored in the shared "modules" storage with prefixed keys.

## Implementation Summary

| Aspect | redb | RocksDB | Sled |
|--------|------|---------|------|
| Known trees | Pre-defined TableDefinitions | Pre-created column families at open | `open_tree` creates on demand |
| Module trees | MODULES_TABLE + ModuleTree prefix | "modules" CF + RocksDBModuleTree prefix | "modules" tree + SledModuleTree prefix |
| Unknown names | Rejected (get_table_def returns None) | Create CF on demand (tests, WAL) | Create tree on demand |
| Reopen | N/A | list_cf + merge for existing DBs with extra CFs | N/A |

## Adding a New Backend (e.g. TidesDB)

1. Use `KNOWN_TREE_NAMES` for the tree/CF list.
2. Implement `ModuleTree` wrapper for `module_*` names—use shared "modules" storage with key prefix `module_{id}_{name}_`.
3. Implement `Database` and `Tree` traits following the same semantics.
