# Cargo audit suppressions (`blvm-node`)

Single source of **`ignore`** IDs is **`.cargo/audit.toml`**. CI runs plain **`cargo audit`** from this repo root (no duplicate **`--ignore`** flags in workflows).

This page records **why** each advisory remains suppressed and how to **re-verify** after dependency bumps. Update the **Last reviewed** column when you shrink the ignore list or confirm an ID still applies.

## Review procedure (B2 / B′)

1. Temporarily rename **`.cargo/audit.toml`** (or empty the `[advisories].ignore` array).
2. Run **`cargo audit`** and capture full output.
3. For each remaining **`RUSTSEC-*`**, run **`cargo tree -i <crate>`** (crate name from the advisory output) and paste the path into this table or the bump PR.
4. Restore **`audit.toml`** only after narratives match reality.

**Note:** `cargo audit` may also print **warnings** (unmaintained / unsound) that are **not** in **`audit.toml`**; treat those separately.

## Suppression table

| Advisory ID | Subject (short) | Notes / typical dependency path | Exit criterion | Last reviewed |
|---------------|-----------------|-----------------------------------|----------------|---------------|
| RUSTSEC-2024-0421 | **`idna`** | Narrative: transitively tied to DNS / **`iroh`** migration (see **`Cargo.toml`** security comment). | Clean **`cargo audit`** without ignore, or upstream removes vulnerable edge. | 2026-05-17 |
| RUSTSEC-2025-0009 | **`ring`** | Narrative: **`iroh`** upgrade path (see **`Cargo.toml`**). | Same | 2026-05-17 |
| RUSTSEC-2023-0071 | **`rsa`** | Narrative: **`iroh`** upgrade path (see **`Cargo.toml`**). | Same | 2026-05-17 |
| RUSTSEC-2026-0007 | **`bytes`** | Direct pin **`bytes = "=1.11.1"`** addresses overflow class; ID may still appear if another edge pulls an older **`bytes`**. Verify with **`cargo tree -i bytes`**. | Single **`bytes`** version in graph at patched line | 2026-05-17 |
| RUSTSEC-2026-0037 | **`quinn-proto`** | Git **`[patch.crates-io]`** to tagged **`quinn-proto`** (see **`Cargo.toml`**). | crates.io release absorbs patch; drop **`patch`** + ignore | 2026-05-17 |
| RUSTSEC-2026-0009 | **`time`** | Git **`[patch.crates-io]`** to tagged **`time`** (see **`Cargo.toml`**). | crates.io release absorbs patch; drop **`patch`** + ignore | 2026-05-17 |
| RUSTSEC-2026-0118 | **`hickory-proto`** — NSEC3 closest-encloser validation unbounded loop (OOM / debug panic) | [`RUSTSEC-2026-0118`](https://rustsec.org/advisories/RUSTSEC-2026-0118); verify edge with **`cargo tree -i hickory-proto`** when present (graph may omit **`hickory`** until a transitive path lands). Typical exit: bump **`iroh`** / resolver stack to lines **`>=0.26.0-beta.1`** / patched **`hickory-net`** per advisory. | Clean audit without ignore, or pinned patched **`hickory-*`** | 2026-05-17 |
| RUSTSEC-2026-0119 | **`hickory-proto`** — O(n²) name compression CPU DoS during encode | [`RUSTSEC-2026-0119`](https://rustsec.org/advisories/RUSTSEC-2026-0119); verify with **`cargo tree -i hickory-proto`**. Patched **`>=0.26.1`**. | Same | 2026-05-17 |

**Owner:** Bitcoin Commons maintainers / release sheriffs for **`blvm-node`**.

Track **B** / **B′** (temporary **`cargo audit`** policy and suppressions narrative) stays with security maintainers; this repo keeps reasons in **`Cargo.toml`** comments and this table — use **`cargo tree`** and upstream advisories when re-verifying suppressions.
