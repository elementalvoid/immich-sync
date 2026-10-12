# Embedded store options for Immich Sync

**Date:** 2026-10-12
**Decision this informs:** the `sync-core` state DB choice for Immich Sync (rusqlite is the current lean in the [project plan](../../PLAN.md)).
**Status:** research complete; recommendation at the end. **rusqlite should stand.**

## Scope and method

This report compares embedded stores for one workload: a single-process, Windows-first Rust sync client with **one writer** (`sync-core`) and **one or more readers** (the GPUI config window and tray status). The store holds the [project plan](../../PLAN.md) tables — `source_folder`, `sync_item`, `version`, `attempt` — at 10k–1M rows over the app's life, with these access patterns:

| Pattern | Shape |
|---|---|
| Get by path | point lookup on a UTF-8 path (Windows long paths) |
| Scan by state | all pending / retry-wait / dead-letter / missing rows |
| Batch upsert | after a reconciliation scan |
| Update state | after one upload attempt |
| Counts by state | dashboard and tray badges |
| Ordered listing | deletion review UI |

Durability required: crash-safe for kill/power-loss, corruption detectable, schema migrations across app versions with no data loss. A rare rebuild path exists when the engine can rescan local folders and re-check the server. Windows MSVC first; Linux/macOS later; no cross-compilation.

**Method.** Every version, date, license, download count, maintenance and API claim below comes from a primary source retrieved on **2026-10-12**: the crates.io API, docs.rs, upstream repositories/headers/design docs, and the official SQLite/LMDB documentation. No benchmark was run for this report. The only performance numbers cited are redb's own self-reported table, and they are labelled as such.

## Version and maintenance snapshot (retrieved 2026-10-12)

| Option | Current release (date) | License | Engine language | crates.io downloads | GitHub stars | Last repo commit |
|---|---|---|---|---|---|---|
| **rusqlite** | **0.40.2** (2026-08-08) | MIT | C (SQLite amalgamation, bundled) | 123.0 M | 4,433 | 2026-10-11 |
| **sqlx** (SQLite) | **0.9.0** (crates.io 2026-05-21; changelog 2026-05-06) | MIT OR Apache-2.0 | C driver on `libsqlite3-sys` | 162.6 M | 17,562 | 2026-10-04 |
| **redb** | **4.3.0** (2026-09-15); 2.6.4 maintenance (2026-10-02) | MIT OR Apache-2.0 | pure Rust | 13.1 M | 4,837 | 2026-10-05 |
| **fjall** | **3.1.12** (2026-10-03) | MIT OR Apache-2.0 | pure Rust | 1.8 M | 2,375 | 2026-10-05 |
| **sled** | **1.0.0-alpha.124** (2024-10-11); last stable 0.34.7 (2021-09-12) | MIT OR Apache-2.0 | pure Rust | 16.9 M | 9,098 | 2026-04-04 |
| **heed / LMDB** | heed **0.22.1** (2026-04-07); LMDB **1.0.2** tag (2026-09-08) | MIT (heed) + OpenLDAP Public License 2.8 (LMDB) | C (LMDB) | 6.6 M | 921 | heed 2026-09-30; LMDB 2026-10-09 |
| **rocksdb** | **0.25.0** (2026-08-16), wraps RocksDB **11.8.1** | Apache-2.0 (crate); RocksDB files include GPLv2 `COPYING` | C++ (RocksDB) | 56.9 M | 2,183 | 2026-08-16 |
| **Turso** (added candidate) | **0.8.2** (2026-10-06) | MIT | pure Rust | 1.1 M | 24,724 | 2026-10-11 |

Download/reverse-dependency counts and release dates: [rusqlite](https://crates.io/crates/rusqlite), [libsqlite3-sys](https://crates.io/crates/libsqlite3-sys), [sqlx](https://crates.io/crates/sqlx), [redb](https://crates.io/crates/redb), [fjall](https://crates.io/crates/fjall), [sled](https://crates.io/crates/sled), [heed](https://crates.io/crates/heed), [lmdb-master-sys](https://crates.io/crates/lmdb-master-sys), [rocksdb](https://crates.io/crates/rocksdb), [librocksdb-sys](https://crates.io/crates/librocksdb-sys), [turso](https://crates.io/crates/turso). Repository stats: [rusqlite](https://github.com/rusqlite/rusqlite), [sqlx](https://github.com/transact-rs/sqlx), [redb](https://github.com/cberner/redb), [fjall](https://github.com/fjall-rs/fjall), [sled](https://github.com/spacejam/sled), [heed](https://github.com/meilisearch/heed), [rust-rocksdb](https://github.com/rust-rocksdb/rust-rocksdb), [turso](https://github.com/tursodatabase/turso), [LMDB](https://github.com/LMDB/lmdb).

## Comparison

| Store | ACID / durability out of the box | Concurrency model | Migrations | SQL? | Windows | Verdict for Immich Sync |
|---|---|---|---|---|---|---|
| **rusqlite** | ACID in WAL mode with `synchronous=FULL` (the default); crash/power-loss suites upstream | WAL: many readers + **one writer**, same-host | `rusqlite_migration` / `refinery`; plain `user_version` | yes | bundled C build via `cc`; win32 VFS path cap 1040 B | **Recommended.** Best fit for the query patterns and lowest new code |
| **sqlx** (SQLite) | same SQLite engine; `synchronous` defaults to FULL; WAL must be requested explicitly | pool of connections, each on a dedicated thread; SQLite still one writer | built-in `sqlx::migrate` | yes | bundled C build; macros need a dev DB or offline data | Works, but async/pool adds cost with no benefit for one writer |
| **redb** | fully ACID, MVCC, "crash-safe by default"; `Durability::Immediate` is the default | MVCC: many readers + **one writer** at a time (multi-process is experimental) | none built in; file-format versioning only | no | Windows CI; byte-range lock fixes landed Aug–Sep 2026 | Credible pure-Rust fallback, but manual indexes + recent data-loss-class fixes |
| **fjall** | crash-safe, but writes reach the OS buffer, not disk, until `persist(SyncAll)` | MVCC snapshots; single-writer or optimistic transactions | none built in; major-version migration path promised | no | Windows CI restored Nov 2025 | Watch; recent fsync bug and mostly yanked 2026 releases disqualify it for primary state |
| **sled** | 500 ms auto-fsync by default; alpha | lock-free, one process only | none | no | untested in years | **No.** Alpha since 2021, rewrite in progress, no releases since 2024 |
| **heed / LMDB** | full ACID, durable by default (unless `MDB_NOSYNC`) | MVCC: many readers + **one writer**; multi-process capable | none built in; engine format stable | no | C; Windows-specific fixes Aug 2026; **511-byte default key limit** | Solid engine, poor fit for path-keyed rows and manual indexes |
| **rocksdb** | atomic batches; durability only with `sync=true` (default false) | one read-write DB instance; snapshots | none built in | no | C++ build needs Clang/LLVM; MSVC supported | Overkill for a tray app |
| **Turso** | SQLite-compatible, pre-1.0; vendors advise independent backups | MVCC (`BEGIN CONCURRENT`) | inherits SQLite-style schema tooling | yes | Windows supported | Watch; pre-1.0 risk for the app's only durable state |

---

## 1. rusqlite (SQLite via bundled C) — recommended

**Version and status.** 0.40.2 released 2026-08-08 ([crates.io](https://crates.io/crates/rusqlite); [release notes](https://github.com/rusqlite/rusqlite/releases/tag/v0.40.2), which lowered MSRV to 1.88). The repo had commits on 2026-10-11 and 172 open issues at retrieval — an actively maintained project, not abandoned ([repo](https://github.com/rusqlite/rusqlite)). License MIT ([crates.io](https://crates.io/crates/rusqlite)).

**SQLite version.** With `features = ["bundled"]`, `libsqlite3-sys` compiles SQLite from source with the `cc` crate ([README](https://github.com/rusqlite/rusqlite/blob/master/README.md)); the vendored header at tag `v0.40.2` is **SQLite 3.53.2** ([sqlite3.h](https://github.com/rusqlite/rusqlite/blob/v0.40.2/libsqlite3-sys/sqlite3/sqlite3.h)), matching the 0.40.1 release note. Upstream SQLite is **3.54.0** ([download page](https://sqlite.org/download.html)), so bundled users are one minor release behind at worst. SQLite itself is public domain ([copyright page](https://sqlite.org/copyright.html)).

**Dependency footprint.** C, but a single self-contained amalgamation with pre-generated bindings; `bundled` "is a good option for cases where linking to SQLite is complicated, such as Windows" ([README](https://github.com/rusqlite/rusqlite/blob/master/README.md)). Without `bundled`, it looks for a system/vcpkg SQLite.

**ACID and crash safety (quoted).**
- "FULL is atomic, consistent, isolated, and durable (ACID) in WAL mode" and "Transactions are durable across application crashes regardless of the synchronous setting or journal mode" ([PRAGMA synchronous](https://sqlite.org/pragma.html#pragma_synchronous)).
- The compile-time default is `synchronous=2` (FULL) ([compile options](https://sqlite.org/compile.html#default_synchronous)).
- The lower `synchronous=NORMAL` setting trades durability, not integrity: "WAL mode is always consistent with synchronous=NORMAL, but WAL mode does lose durability. A transaction committed in WAL mode with synchronous=NORMAL might roll back following a power loss" ([PRAGMA synchronous](https://sqlite.org/pragma.html#pragma_synchronous)).
- Upstream testing is the strongest evidence in this comparison: 100% branch coverage in an as-deployed configuration and "590 times as much test code and test scripts — 92053.1 KSLOC", including crash and power-loss tests ([How SQLite Is Tested](https://sqlite.org/testing.html)).
- Corruption is detectable with `PRAGMA integrity_check` / `quick_check` ([PRAGMA](https://sqlite.org/pragma.html)).

**Concurrency model (quoted).** In WAL mode "writers and readers can run at the same time. However, since there is only one WAL file, there can only be one writer at a time." Checkpoints can run with readers but stop at an active reader's end mark; SQLite auto-checkpoints at 1000 pages by default ([WAL](https://sqlite.org/wal.html)). WAL requires all processes on the same host and "does not work over a network filesystem" ([WAL](https://sqlite.org/wal.html)); filesystems with broken locking can corrupt databases, "especially … network filesystems and NFS" ([How To Corrupt](https://sqlite.org/howtocorrupt.html)). A deferred transaction upgrades to a write transaction "if possible, or return SQLITE_BUSY" ([transactions](https://sqlite.org/lang_transaction.html)), so the writer should use `BEGIN IMMEDIATE` plus a `busy_timeout` ([`Connection::busy_timeout`](https://docs.rs/rusqlite/latest/rusqlite/struct.Connection.html)).

**Fit for the query patterns.** Direct: `path TEXT PRIMARY KEY` gives get-by-path; an index on `state` covers state scans; `INSERT … ON CONFLICT DO UPDATE` covers batch upsert ([UPSERT](https://sqlite.org/lang_upsert.html)); `UPDATE … WHERE path=?` covers post-upload state changes; `SELECT state, COUNT(*) … GROUP BY state` covers counts (index-only); an index on `(state, path)` covers ordered review listing. Sizes are a non-issue: the default page-count limit already allows ~17.5 TB and 2^64 rows theoretical ([limits](https://sqlite.org/limits.html)).

**Migrations.** Plain `PRAGMA user_version` plus `ALTER TABLE` SQL works with no extra crate. Two maintained helpers: **rusqlite_migration 2.6.0** (2026-05-28, Apache-2.0), which deliberately uses `user_version` instead of a metadata table ([crates.io](https://crates.io/crates/rusqlite_migration), [docs](https://docs.rs/rusqlite_migration/latest/rusqlite_migration/)), and **refinery 0.10.0** (2026-10-02, MIT), whose documented example runs embedded migrations against a `rusqlite::Connection` ([crates.io](https://crates.io/crates/refinery), [docs](https://docs.rs/refinery/latest/refinery/)). Both give ordered, versioned, no-data-loss upgrades.

**Windows support and caveats.**
- The bundled build compiles with the MSVC toolchain; no bindgen step for normal builds (pre-generated bindings in `libsqlite3-sys`).
- File names are UTF-8 ([sqlite3_open](https://sqlite.org/c3ref/open.html)).
- Path length: the default `win32` VFS caps the database path at **1040 bytes**; `win32-longpath` raises it to 65534 bytes ([VFS](https://sqlite.org/vfs.html)). This limit applies to the DB file's own path, not to paths stored as TEXT. If the DB ever lives under a long path, open it with the long-path VFS via [`Connection::open_with_flags_and_vfs`](https://docs.rs/rusqlite/latest/rusqlite/struct.Connection.html).
- Keep the DB in the local data directory, never on OneDrive/network shares — WAL is unsupported on network filesystems ([WAL](https://sqlite.org/wal.html)).

**Adoption.** 123.0 M downloads and 5,308 reverse dependencies on crates.io ([crates.io](https://crates.io/crates/rusqlite)); `libsqlite3-sys` alone has 262.8 M downloads, which reflects how much of the Rust ecosystem reaches SQLite through this binding ([crates.io](https://crates.io/crates/libsqlite3-sys)).

**Verdict.** Stands. It is the only candidate that matches every query pattern with built-in machinery and the only one whose crash-safety story is backed by decades of adversarial testing; the cost is a C dependency that is already accepted.

---

## 2. sqlx with the SQLite driver

**Version and status.** 0.9.0, published to crates.io 2026-05-21; the [CHANGELOG](https://github.com/transact-rs/sqlx/blob/main/CHANGELOG.md) heading is dated 2026-05-06 and announces the move from `launchbadge/sqlx` to the `transact-rs` org. MSRV for this cycle is **Rust 1.94** ([CHANGELOG](https://github.com/transact-rs/sqlx/blob/main/CHANGELOG.md)). License MIT OR Apache-2.0 ([crates.io](https://crates.io/crates/sqlx)). Very active: last commit 2026-10-04.

**What the SQLite driver is.** Not a different engine: "The SQLite driver uses the libsqlite3 C library" and the `sqlite` feature statically links a bundled SQLite via `libsqlite3-sys` ([sqlx-sqlite source](https://github.com/transact-rs/sqlx/blob/main/sqlx-sqlite/src/lib.rs)). As of 0.9.0 the `libsqlite3-sys` version is a range; only one such version may appear in the dependency tree, which matters if this app later mixes sqlx and rusqlite ([sqlx-sqlite source](https://github.com/transact-rs/sqlx/blob/main/sqlx-sqlite/src/lib.rs)).

**Durability and concurrency.** sqlx does **not** set WAL for you — "Don't set `journal_mode` unless the user requested it" — and it leaves `synchronous` at SQLite's FULL default ([sqlx-sqlite options](https://github.com/transact-rs/sqlx/blob/main/sqlx-sqlite/src/options/mod.rs)). So the durability reasoning in §1 applies unchanged. Its concurrency advantage over rusqlite is only an async facade: "Each SQLite connection has a dedicated thread" ([worker source](https://github.com/transact-rs/sqlx/blob/main/sqlx-sqlite/src/connection/worker.rs)). SQLite's own one-writer rule still governs.

**Migrations.** Built-in `migrate` feature: migration files named `<VERSION>_<DESCRIPTION>.sql`, a `Migrator`, and `Migrate` trait ([docs.rs](https://docs.rs/sqlx/latest/sqlx/migrate/index.html)). Compile-time checked queries need either a development database at build time or the offline query data; "Offline mode is now always enabled" per the [README](https://github.com/transact-rs/sqlx/blob/main/README.md).

**Verdict.** Correct if the project already commits to tokio and wants compile-time-checked SQL; otherwise it adds an async runtime, a connection pool, and a build-time metadata step for no gain with one writer and a handful of readers. It is not a separate storage engine, so choosing it does not remove the SQLite risk that §1 addresses.

---

## 3. redb (pure-Rust B+tree KV)

**Version and status.** Stable line **4.3.0** (2026-09-15); **2.6.4** (2026-10-02) is a maintenance release on the older 2.x line — its tag points at a commit on top of 2.6.3 (2025-08-23) ([releases](https://github.com/cberner/redb/releases), [crates.io](https://crates.io/crates/redb)). MSRV 1.90 ([Cargo.toml](https://github.com/cberner/redb/blob/master/Cargo.toml)). License MIT OR Apache-2.0. Pure Rust, no C. Active: commits through 2026-10-05; only 2 open issues at retrieval.

**ACID and crash safety (quoted).** "A simple, portable, high-performance, ACID, embedded key-value store … Fully ACID-compliant transactions … MVCC support for concurrent readers & writer, without blocking … Crash-safe by default" ([README](https://github.com/cberner/redb/blob/master/README.md)). The default durability is `Durability::Immediate`, guaranteed persistent when `commit` returns ([source](https://github.com/cberner/redb/blob/master/src/transactions.rs), [Durability](https://docs.rs/redb/latest/redb/enum.Durability.html)). The [design document](https://github.com/cberner/redb/blob/master/docs/design.md) describes the default 1-phase + checksum commit, a 2-phase option, XXH3_128 non-cryptographic Merkle checksums, and repair after unclean shutdown. Its integrity check is `Database::check_integrity()` ([docs.rs](https://docs.rs/redb/latest/redb/struct.Database.html)).

**Honest risk note.** The 2026 releases fixed data-loss-class bugs: 4.0.0 fixed a "critical bug where the accessor could outlive the `Table` … This could cause data loss"; 3.1.3 fixed a data-loss bug from `Table::get_mut()` guards; 4.1.0 shipped "a large number of bug fixes discovered by AI coding agents", several of them savepoint/table-corruption cases ([releases](https://github.com/cberner/redb/releases)). The current 4.3.0 line is the hardened one, but the track record is far shorter than SQLite's.

**Concurrency (quoted).** "Multiple reads may be performed concurrently, with each other, and with writes. Only a single write may be in progress at a time" ([Database](https://docs.rs/redb/latest/redb/struct.Database.html)); "Only a single WriteTransaction may exist at a time" ([WriteTransaction](https://docs.rs/redb/latest/redb/struct.WriteTransaction.html)). Cross-process read-write access is **experimental** behind `experimental-multiprocess` in 4.3.0 ([release notes](https://github.com/cberner/redb/releases/tag/v4.3.0)) — irrelevant for this single-process app, and the default exclusive-writer lock is fine.

**No-SQL cost.** Tables are `TableDefinition<K,V>` maps with `BTreeMap`-like semantics and range reads ([crates.io docs](https://docs.rs/redb/latest/redb/)); multimap tables exist for 1-to-many relations. There is no query language, planner, aggregate, or DDL. The app must maintain a `(state, path)` index table, keep counts (or scan the index), and version its own schema. No migration crate exists: a crates.io search on 2026-10-12 surfaced only third-party projects, not a migration framework; the engine guarantees file-format stability and "a reasonable effort … upgrade path", not schema migration ([README](https://github.com/cberner/redb/blob/master/README.md), [design doc version changes](https://github.com/cberner/redb/blob/master/docs/design.md)).

**Windows.** CI runs `windows-latest` ([workflow](https://github.com/cberner/redb/blob/master/.github/workflows/ci.yml)), and Windows byte-range locking was reworked in issues closed 2026-08-29 and 2026-09-10 ([#1423](https://github.com/cberner/redb/issues/1423), [#1479](https://github.com/cberner/redb/issues/1479)) — reassuring for a single-process app, and evidence Windows is actively tested.

**Adoption.** 13.1 M downloads and 507 reverse dependencies ([crates.io](https://crates.io/crates/redb)) — real but an order of magnitude below SQLite bindings.

**Verdict.** The only credible pure-Rust primary-store candidate; choose it only if "no C dependency" is a hard requirement, and budget for hand-built secondary indexes, counters, migration logic, and integrity checks. Otherwise it loses to SQLite on correctness history and query ergonomics, not on speed.

---

## 4. fjall (pure-Rust LSM KV)

**Version and status.** **3.1.12** (2026-10-03), MIT OR Apache-2.0, MSRV 1.90 ([crates.io](https://crates.io/crates/fjall), [README](https://github.com/fjall-rs/fjall/blob/main/README.md)). Active: 2026-10-05 commits, 40 open issues; releases land within days of fixes.

**Durability — the critical caveat (quoted).** "By default, any operation will flush to OS buffers, but **not** to disk. This matches RocksDB's default durability." Durability requires an explicit `Database::persist(PersistMode::SyncAll)` (fsync data+metadata); `PersistMode::SyncData` uses `fdatasync`; `PersistMode::Buffer` says data is not guaranteed persisted on power loss or OS crash ([README](https://github.com/fjall-rs/fjall/blob/main/README.md), [PersistMode](https://docs.rs/fjall/latest/fjall/enum.PersistMode.html)). The single-writer transactional API documents: "Persisting only affects durability, NOT consistency! Even without flushing data is crash-safe" ([SingleWriterTxDatabase](https://docs.rs/fjall/latest/fjall/struct.SingleWriterTxDatabase.html)).

**Recent fsync bug.** Release 3.1.11 (2026-10-02) fixed "[Missing fsync before memtable flush](https://github.com/fjall-rs/fjall/issues/329)"; the issue describes a power loss leaving a write batch partially applied and `get` disagreeing with `range` across keyspaces — an atomicity violation that contradicts the documented cross-keyspace atomic `WriteBatch`. It is fixed, but it was reported 2026-10-01 and shipped the next day. That is very fresh.

**Concurrency and query model.** Internally synchronized for threads; one database per process (multi-process loading "may **not**" happen). MVCC snapshots give repeatable reads; transactional modes are `OptimisticTxDatabase` (serializable, conflicts rerun) and `SingleWriterTxDatabase` (one write txn at a time) ([README](https://github.com/fjall-rs/fjall/blob/main/README.md), [docs](https://docs.rs/fjall/latest/fjall/)). Cross-keyspace atomic batches are built in ([Database](https://docs.rs/fjall/latest/fjall/struct.Database.html)). It is explicitly "not … a relational or wide-column database: it has no notion of columns or query language"; keys are limited to 65536 bytes and values to 2^32 bytes ([docs.rs](https://docs.rs/fjall/latest/fjall/)). Same manual-index, no-migration-framework story as redb; the README promises "a major version bump and a migration path" for future disk-format changes ([README](https://github.com/fjall-rs/fjall/blob/main/README.md)). Rust 3.x is young (3.0.0 in 2025-08 per [releases](https://github.com/fjall-rs/fjall/releases)).

**Version churn — read this before adopting.** fjall 3.0.0 (2026-01-02) and every release from 3.0.1 through 3.1.5 are **yanked** on crates.io; the oldest non-yanked 3.x release is 3.1.6 (2026-07-05) ([crates.io versions](https://crates.io/crates/fjall/versions)). A GitHub search of the repo found no issue or release note explaining the yanks. Whatever the cause, nine yanks in one year means a new install cannot resolve to most of the 2026 release history.

**Windows.** `windows-latest` is in CI now ([workflow](https://github.com/fjall-rs/fjall/blob/main/.github/workflows/test.yml)); Windows CI had to be restored in Nov 2025 ([#214](https://github.com/fjall-rs/fjall/issues/214), [#215](https://github.com/fjall-rs/fjall/issues/215)).

**Adoption.** 1.8 M downloads, 96 reverse dependencies ([crates.io](https://crates.io/crates/fjall)).

**Verdict.** Watch, don't adopt as primary state. A store that recently lost an fsync before a memtable flush, and whose 2026 releases are mostly yanked, is the wrong place for the app's only durable record; revisit after a year of quiet releases.

---

## 5. sled — not viable

**Status.** Latest published version is **1.0.0-alpha.124 (2024-10-11)**; the last stable release was **0.34.7 (2021-09-12)** ([crates.io](https://crates.io/crates/sled), [releases](https://github.com/spacejam/sled/releases)). The default branch has a large in-progress rewrite: the README itself is marked "out of sync with the main branch which contains a large in-progress rewrite" ([README](https://github.com/spacejam/sled/blob/main/README.md)). Commits continue (last 2026-04-04) but produce no releases ([commits](https://github.com/spacejam/sled/commits/main)).

**Behavior notes if it were used.** Durability is time-based by default: "sled automatically fsyncs every 500ms" unless `flush` is called; transactions are optimistic and must not perform IO; "sled does not support multiple open instances for the time being" ([README](https://github.com/spacejam/sled/blob/main/README.md)).

**Adoption.** 16.9 M downloads and 533 reverse dependencies ([crates.io](https://crates.io/crates/sled)) — high historic usage, but that does not offset five years without a stable release.

**Verdict.** No. An alpha database whose maintainer is rewriting the storage engine is disqualified for the app's only durable state.

---

## 6. heed / LMDB

**Version and status.** heed **0.22.1** (2026-04-07), MIT, maintained under the `meilisearch` org, wrapping LMDB from the `mdb.master` branch via `lmdb-master-sys` 0.2.6 (2026-04-07) ([crates.io](https://crates.io/crates/heed), [docs.rs](https://docs.rs/heed/latest/heed/), [lmdb-master-sys](https://crates.io/crates/lmdb-master-sys)). LMDB itself reached **1.0.0** (tag 2026-06-30) and **1.0.2** (tag 2026-09-08) ([tags](https://github.com/LMDB/lmdb/tags)); active development continues (last commit 2026-10-09, including a Windows `write_meta` fix on 2026-08-19) ([commits](https://github.com/LMDB/lmdb/commits/mdb.master3)).

**License.** heed is MIT; LMDB is **OpenLDAP Public License 2.8**, a permissive BSD-style license ([COPYRIGHT](https://github.com/LMDB/lmdb/blob/mdb.master/libraries/liblmdb/COPYRIGHT), [LICENSE](https://github.com/LMDB/lmdb/blob/mdb.master/libraries/liblmdb/LICENSE)). Bundled C.

**ACID and crash safety (quoted).** "fully transactional with full ACID semantics… Data pages use a copy-on-write strategy so no active data pages are ever overwritten, which also provides resistance to corruption and eliminates the need of any special recovery procedures after a system crash" ([LMDB introduction](http://www.lmdb.tech/doc/index.html)). Writes are durable unless `MDB_NOSYNC`/`MDB_MAPASYNC` are set; the header documents those modes' tradeoff: without sync "a system crash can corrupt the database or lose the last transactions", while with ordered writes and no `MDB_WRITEMAP` transactions "exhibit ACI … and only lose D" ([lmdb.h](https://github.com/LMDB/lmdb/blob/mdb.master/libraries/liblmdb/lmdb.h)). There is **no checksumming** in the plain `heed`/`mdb.master` build: the crate docs say `heed3` (tracking `mdb.master3`) adds "encryption-at-rest and checksumming features that the heed crate doesn't" and "will be stable once the LMDB version on the mdb.master3 branch will be officially released" ([docs.rs](https://docs.rs/heed/latest/heed/)). Since the app requires detectable corruption, plain heed is a weaker fit unless heed3 stabilizes.

**Concurrency (quoted).** "Writes are fully serialized; only one write transaction may be active at a time, which guarantees that writers can never deadlock. The database structure is multi-versioned so readers run with no locks; writers cannot block readers, and readers don't block writers" ([LMDB introduction](http://www.lmdb.tech/doc/index.html)). Multi-process access is supported — more than this app needs.

**The fit problems.**
1. **Key size.** LMDB's maximum key size "Depends on the compile-time constant `MDB_MAXKEYSIZE`. Default 511" ([lmdb.h](https://github.com/LMDB/lmdb/blob/mdb.master/libraries/liblmdb/lmdb.h)). A full Windows long path can exceed 511 UTF-8 bytes, so the natural key (path) is not safe; you would key by hash and lose path-ordered scans. SQLite stores the same path as an unlimited TEXT primary key.
2. **Map size.** "The size of the memory map is also the maximum size of the database. The value should be chosen as large as possible, to accommodate future growth" ([mdb_env_set_mapsize](http://www.lmdb.tech/doc/group__mdb.html)); running out means `MDB_MAP_FULL` errors. heed exposes `EnvOpenOptions::map_size` with the note that it "must be a multiple of the OS page size" ([source](https://github.com/meilisearch/heed/blob/main/heed/src/envs/env_open_options.rs)).
3. **No query language or migration framework.** Same manual-index/counter burden as redb. The open heed issue "[Improve the documentation by bringing the LMDB caveats into it](https://github.com/meilisearch/heed/issues/150)" (open since 2023) is a hint that these caveats bite users.

**Windows.** LMDB has Windows-specific behavior throughout the header: `MDB_TLS_FULL` is "Windows only", `MDB_NORDAHEAD` has no Windows effect, and the `mdb_filehandle_t` is an opaque pointer ([lmdb.h](https://github.com/LMDB/lmdb/blob/mdb.master/libraries/liblmdb/lmdb.h)). Windows fixes landed in Aug 2026 ([commits](https://github.com/LMDB/lmdb/commits/mdb.master3)).

**Adoption.** heed has 6.6 M downloads and 80 reverse dependencies ([crates.io](https://crates.io/crates/heed)); Meilisearch's own build depends on it ([Cargo.toml](https://github.com/meilisearch/meilisearch/blob/main/Cargo.toml)), which is a strong production signal for the binding.

**Verdict.** A superb engine whose key-size cap and map-size management make it a poor fit for path-keyed rows; it wins over redb only where mmap reads demonstrably matter, which is not this workload.

---

## 7. rocksdb (rust-rocksdb)

**Version and status.** **0.25.0** (2026-08-16), Apache-2.0, MSRV 1.88; it statically links **RocksDB 11.8.1** through `librocksdb-sys` 0.19.0+11.8.1 ([crates.io](https://crates.io/crates/rocksdb), [librocksdb-sys](https://crates.io/crates/librocksdb-sys)). Release cadence is roughly annual (0.24.0 was 2025-08-10). License nuance worth flagging: the RocksDB repository root contains `COPYING` (GPLv2), `LICENSE.Apache`, and `LICENSE.leveldb` ([repo](https://github.com/facebook/rocksdb)), and GitHub classifies the repo as GPL-2.0, while the crates declare Apache-2.0 / MIT+Apache-2.0+BSD-3-Clause. Treat licensing as a counsel question if it ever becomes a finalist.

**Build weight.** "Requirements: Clang and LLVM"; the binding is "statically linked with a specific version of RocksDB" built from C++ source, with `bindgen` and extra features for `/MT` vs `/MD` on Windows ([README](https://github.com/rust-rocksdb/rust-rocksdb/blob/master/README.md)). CI does run `windows-latest` ([workflow](https://github.com/rust-rocksdb/rust-rocksdb/blob/master/.github/workflows/rust.yml)).

**Durability and concurrency.** `WriteOptions::sync` defaults to **false**: "If this flag is false, and the machine crashes, some recent writes may be lost" ([options.h](https://github.com/facebook/rocksdb/blob/main/include/rocksdb/options.h)). Opening read-write fails "if the DB is already open (read-write) by another DB object", enforced via `Env::LockFile` ([db.h](https://github.com/facebook/rocksdb/blob/main/include/rocksdb/db.h)) — a single-writer-process model, same as this app needs.

**Verdict.** Overkill. The workload is a few hundred thousand narrow rows; RocksDB buys LSM write throughput the app does not need at the cost of C++ build tooling and another durability knob to get right.

---

## 8. Turso (added candidate: pure-Rust SQLite rewrite)

The task asked for other credible, actively maintained options. **Turso Database** (formerly limbo) is the strongest addition: **0.8.2** (2026-10-06), MIT, pure Rust, SQLite-compatible, 24,724 stars, commits on 2026-10-11 ([crates.io](https://crates.io/crates/turso), [repo](https://github.com/tursodatabase/turso)).

The README says "compatible with SQLite" at the SQL dialect, file-format, and C-API levels while tracking SQLite 3.50.4, supports Windows, and adds MVCC `BEGIN CONCURRENT`. But it also states: "we have not yet reached 1.0 … until we declare 1.0, we recommend the same discipline you would apply to any database: keep independent backups" ([README](https://github.com/tursodatabase/turso/blob/main/README.md)). For an app whose only durable record of upload state is this file, a pre-1.0 engine is the wrong risk. **Verdict: watch.** Re-evaluate at 1.0, especially if dropping the C toolchain becomes a goal — it would then replace both rusqlite and redb.

---

## Migration tooling per option

| Option | Tooling | Notes |
|---|---|---|
| rusqlite | [rusqlite_migration](https://crates.io/crates/rusqlite_migration) 2.6.0 (2026-05-28) or [refinery](https://crates.io/crates/refinery) 0.10.0 (2026-10-02); or plain `PRAGMA user_version` + `ALTER TABLE` | rusqlite_migration intentionally avoids a metadata table and uses `user_version` ([docs](https://docs.rs/rusqlite_migration/latest/rusqlite_migration/)); refinery runs embedded SQL/Rust migrations against `rusqlite::Connection` ([docs](https://docs.rs/refinery/latest/refinery/)) |
| sqlx | built-in `migrate` feature + `sqlx-cli` | files named `<VERSION>_<DESCRIPTION>.sql` ([docs](https://docs.rs/sqlx/latest/sqlx/migrate/index.html)) |
| redb | none found | engine file-format versions v1–v3 are documented ([design doc](https://github.com/cberner/redb/blob/master/docs/design.md)); app schema evolution is DIY |
| fjall | none found | "Future breaking changes will result in a major version bump and a migration path" ([README](https://github.com/fjall-rs/fjall/blob/main/README.md)) |
| sled | none | disqualified |
| heed/LMDB | none | engine format stable; LMDB ships upgrade notes (`upgrading.doc` in the source tree); app schema evolution is DIY |
| rocksdb | none | format compatibility is an engine guarantee; app schema evolution is DIY |
| Turso | SQLite-style SQL | inherits SQL DDL/migration patterns |

For the KV options, "migration" also means **serialization changes**: adding a field to a value type breaks decoding unless the app versions its own records. SQLite's `ALTER TABLE ADD COLUMN` and typed columns remove that class of work.

## The "no SQL" cost for the four KV options

The app's six query patterns are exactly the ones SQL databases give away. On a KV store the app owns all of these:

| App need | SQL (rusqlite) | KV (redb/fjall/heed/rocksdb) |
|---|---|---|
| Get by path | `WHERE path = ?` on the primary key | direct `get(path)` — the one free pattern |
| Scan by state | index on `state` | a second table/keyspace keyed `(state, path)`, written in the same transaction as the item row |
| Batch upsert | one `INSERT … ON CONFLICT DO UPDATE` prepared statement | per-row `insert` inside one write txn/batch (redb transaction, fjall `WriteBatch`, LMDB txn) |
| Update state | `UPDATE … WHERE path = ?` | update the item row **and** every index entry that contains `state`, atomically, or let indexes drift |
| Counts by state | `GROUP BY state` (index-only) | maintain counters in a meta table with each state change, or scan the whole state index |
| Ordered review listing | `ORDER BY state, path` with a composite index | range over the `(state, path)` index — order is free only if the key encoding preserves it |

Cross-table atomicity exists in all four (redb transactions span tables, fjall batches span keyspaces, LMDB transactions span named DBs, RocksDB write batches), so correctness is achievable — but every invariant is now the app's code, and every schema change is a manual backfill. The KV options also leave corruption detection to the engine: redb has `check_integrity()` and XXH3 checksums (non-cryptographic; the design doc details a theoretical attack that 2-phase commit mitigates) ([design doc](https://github.com/cberner/redb/blob/master/docs/design.md)); plain heed/LMDB has no checksumming (see §6); SQLite has `PRAGMA integrity_check` ([PRAGMA](https://sqlite.org/pragma.html)) and the upstream test suite behind it ([testing](https://sqlite.org/testing.html)).

## Recommendation: rusqlite stands

**rusqlite should stand as the state store.** Reasons, in order:

1. **Query fit.** Four of the six access patterns are one-line SQL; the other two are index design, which KV options also require — except there the app must hand-roll and then maintain them.
2. **Crash-safety evidence.** SQLite is the only candidate with documented 100% branch coverage in its deployed configuration plus crash/power-loss test suites ([testing](https://sqlite.org/testing.html)); WAL + `synchronous=FULL` is ACID by the vendor's own statement ([pragma](https://sqlite.org/pragma.html#pragma_synchronous)).
3. **Concurrency matches the design.** WAL gives many readers + one writer ([WAL](https://sqlite.org/wal.html)), exactly the engine/UI split in the plan.
4. **Migrations are a solved problem** with two maintained crates ([rusqlite_migration](https://docs.rs/rusqlite_migration/latest/rusqlite_migration/), [refinery](https://docs.rs/refinery/latest/refinery/)).
5. **Costs are bounded.** One C amalgamation compiled by `cc`; the README recommends `bundled` specifically for Windows ([README](https://github.com/rusqlite/rusqlite/blob/master/README.md)); build/binary-size impact was not measured here, and should be measured once in CI rather than assumed.
6. **The alternatives lose on the app's own criteria**, not on hype: sled is alpha; fjall recently lost an fsync; LMDB caps keys at 511 bytes; redb requires hand-built indexes and has a 2026 history of data-loss-class fixes; sqlx is the same engine with extra machinery; RocksDB/Turso are category mismatches (heavyweight, pre-1.0).

**Settings recipe** (each item has a cited basis above): open the DB with `features=["bundled"]`; `PRAGMA journal_mode=WAL`; keep `synchronous=FULL` unless losing the last few commits on power loss is acceptable; one dedicated writer connection using `BEGIN IMMEDIATE` + `busy_timeout`; readers on their own connections; prepared statements, batches inside explicit transactions; the default 1000-page auto-checkpoint is fine; run `PRAGMA quick_check`/`integrity_check` on startup (cheap `quick_check` first) and expose a rebuild path; store the DB in the local data directory, never a network/sync-folder path; index `(state, path)` for the review UI and `state` for counts; normalize path case or define a Windows case-insensitive collation so `C:\Foo` and `c:\foo` do not become two sync items.

**When rusqlite would fall:** if "pure Rust, no C toolchain" becomes a hard product constraint (then redb is the only candidate, with the index/migration work accepted), or if profiling ever shows SQLite read latency blocking the UI after the reader-snapshot design above (it should not).

## Uncertainty and limitations

- **No benchmarks were run.** Nothing here measures this workload. The only performance table cited is redb's own README, which compares redb, LMDB, RocksDB, fjall, and SQLite on a synthetic key-value benchmark (`crates/redb-bench-compare/benches/lmdb_benchmark.rs`, Ryzen 9950X3D + Samsung 9100 PRO NVMe) ([README](https://github.com/cberner/redb/blob/master/README.md)). It is self-reported, tests a different workload (large values, bulk/random KV operations), and **must not be used to predict this app's performance**.
- **Corruption/failure claims are documented, not independently verified here.** SQLite's crash tests, redb's checksums, and fjall's fsync bug are all taken from upstream documentation and issue trackers. A soak test with simulated power cuts is the only way to trust them for this app.
- **Maintenance snapshots are point-in-time.** "Last commit 2026-10-11", open-issue counts, and download counts were retrieved on 2026-10-12 and decay quickly.
- **fjall's yanks are unexplained.** crates.io marks 3.0.0 and 3.0.1–3.1.5 as yanked ([versions](https://crates.io/crates/fjall/versions)), and no public issue or release note explaining that was found on 2026-10-12. Do not read a cause into it; treat 3.1.6+ as the usable minimum if fjall is ever revisited.
- **redb's two release lines.** 4.3.0 is the current stable line while 2.6.4 (2026-10-02) is a later-dated maintenance release on the 2.x line. Confirm upgrade expectations for any pinned version before adopting; the 4.0.0 notes require a migration step off the `Legacy` type ([releases](https://github.com/cberner/redb/releases/tag/v4.0.0)).
- **Bundled SQLite lags upstream** (3.53.2 vs 3.54.0) ([header](https://github.com/rusqlite/rusqlite/blob/v0.40.2/libsqlite3-sys/sqlite3/sqlite3.h), [download](https://sqlite.org/download.html)). Upstream SQLite releases are regularly folded into `libsqlite3-sys` point releases (0.40.1 bumped the bundled version to 3.53.2), but the app owns the update cadence; verify the bundled version on each `cargo update`.
- **Windows behavior was not exercised.** Path-length VFS limits, antivirus interaction with SQLite locking, and real MSVC build times need a spike, not a document.
- **LMDB's branch layout is confusing.** `heed` wraps `mdb.master` while `heed3` wraps `mdb.master3` (encryption/checksums) and is described as not yet stable ([docs.rs](https://docs.rs/heed/latest/heed/)); LMDB 1.0 tags exist (June–September 2026) but the exact LMDB commit vendored by `lmdb-master-sys` 0.2.6 (2026-04-07) is not established here.
- **License review is out of scope.** The RocksDB repository mixes GPLv2 (`COPYING`) with Apache/BSD files ([repo](https://github.com/facebook/rocksdb)); the Rust crates declare Apache-2.0-family licenses. If RocksDB ever becomes a finalist, have counsel confirm.
- **Adoption metrics are proxies.** Download counts include CI and transitive use; stars and reverse dependencies indicate mindshare, not suitability.

## Sources

**SQLite / rusqlite**
- [SQLite write-ahead log](https://sqlite.org/wal.html) — one writer, concurrent readers, network-FS limitation, checkpoint behavior
- [PRAGMA synchronous](https://sqlite.org/pragma.html#pragma_synchronous) — FULL is ACID in WAL; NORMAL durability tradeoff
- [Compile-time options: SQLITE_DEFAULT_SYNCHRONOUS](https://sqlite.org/compile.html#default_synchronous) — default is FULL
- [SQLite download page](https://sqlite.org/download.html) — current 3.54.0
- [SQLite VFS](https://sqlite.org/vfs.html) — `win32` vs `win32-longpath` path limits
- [sqlite3_open](https://sqlite.org/c3ref/open.html) — UTF-8 filenames
- [How SQLite Is Tested](https://sqlite.org/testing.html) — crash/power-loss tests, coverage
- [How To Corrupt An SQLite Database](https://sqlite.org/howtocorrupt.html) — network filesystem locking
- [SQLite limits](https://sqlite.org/limits.html) — database size and row limits
- [SQLite transactions](https://sqlite.org/lang_transaction.html) — DEFERRED/IMMEDIATE, SQLITE_BUSY
- [SQLite UPSERT](https://sqlite.org/lang_upsert.html) — `ON CONFLICT DO UPDATE`
- [SQLite copyright/public domain](https://sqlite.org/copyright.html)
- [rusqlite crates.io](https://crates.io/crates/rusqlite) · [libsqlite3-sys](https://crates.io/crates/libsqlite3-sys)
- [rusqlite repo](https://github.com/rusqlite/rusqlite) · [v0.40.2 release](https://github.com/rusqlite/rusqlite/releases/tag/v0.40.2) · [v0.40.1 release](https://github.com/rusqlite/rusqlite/releases/tag/v0.40.1)
- [rusqlite README](https://github.com/rusqlite/rusqlite/blob/master/README.md) — bundled builds, cc, Windows, SQLite 3.53.2
- [vendored sqlite3.h at v0.40.2](https://github.com/rusqlite/rusqlite/blob/v0.40.2/libsqlite3-sys/sqlite3/sqlite3.h)
- [rusqlite Connection API](https://docs.rs/rusqlite/latest/rusqlite/struct.Connection.html) — `open_with_flags_and_vfs`, `busy_timeout`
- [rusqlite_migration](https://crates.io/crates/rusqlite_migration) · [docs](https://docs.rs/rusqlite_migration/latest/rusqlite_migration/)
- [refinery](https://crates.io/crates/refinery) · [docs](https://docs.rs/refinery/latest/refinery/)

**sqlx**
- [sqlx crates.io](https://crates.io/crates/sqlx) · [repo](https://github.com/transact-rs/sqlx) · [CHANGELOG](https://github.com/transact-rs/sqlx/blob/main/CHANGELOG.md)
- [sqlx-sqlite crate docs](https://github.com/transact-rs/sqlx/blob/main/sqlx-sqlite/src/lib.rs)
- [sqlx-sqlite worker](https://github.com/transact-rs/sqlx/blob/main/sqlx-sqlite/src/connection/worker.rs) — one thread per connection
- [sqlx-sqlite options](https://github.com/transact-rs/sqlx/blob/main/sqlx-sqlite/src/options/mod.rs) — no default WAL; synchronous default
- [sqlx migrate docs](https://docs.rs/sqlx/latest/sqlx/migrate/index.html)

**redb**
- [crates.io](https://crates.io/crates/redb) · [repo](https://github.com/cberner/redb) · [releases](https://github.com/cberner/redb/releases)
- [README](https://github.com/cberner/redb/blob/master/README.md) — ACID/MVCC claims, status, self-reported benchmark
- [design document](https://github.com/cberner/redb/blob/master/docs/design.md) — commit strategies, checksums, MVCC, multiprocess, format versions
- [docs.rs Database](https://docs.rs/redb/latest/redb/struct.Database.html) · [WriteTransaction](https://docs.rs/redb/latest/redb/struct.WriteTransaction.html) · [Durability](https://docs.rs/redb/latest/redb/enum.Durability.html)
- [Cargo.toml (MSRV)](https://github.com/cberner/redb/blob/master/Cargo.toml) · [CI workflow](https://github.com/cberner/redb/blob/master/.github/workflows/ci.yml)
- Windows locking issues: [#1423](https://github.com/cberner/redb/issues/1423), [#1479](https://github.com/cberner/redb/issues/1479)

**fjall**
- [crates.io](https://crates.io/crates/fjall) · [crates.io version list](https://crates.io/crates/fjall/versions) · [README](https://github.com/fjall-rs/fjall/blob/main/README.md) · [releases](https://github.com/fjall-rs/fjall/releases)
- [docs.rs crate page](https://docs.rs/fjall/latest/fjall/) — key/value size limits
- [Database](https://docs.rs/fjall/latest/fjall/struct.Database.html) · [SingleWriterTxDatabase](https://docs.rs/fjall/latest/fjall/struct.SingleWriterTxDatabase.html) · [PersistMode](https://docs.rs/fjall/latest/fjall/enum.PersistMode.html)
- [issue #329, missing fsync](https://github.com/fjall-rs/fjall/issues/329) · [release 3.1.11](https://github.com/fjall-rs/fjall/releases/tag/3.1.11)
- [CI workflow](https://github.com/fjall-rs/fjall/blob/main/.github/workflows/test.yml) · Windows CI issues [#214](https://github.com/fjall-rs/fjall/issues/214), [#215](https://github.com/fjall-rs/fjall/issues/215)

**sled**
- [crates.io](https://crates.io/crates/sled) · [repo](https://github.com/spacejam/sled) · [README](https://github.com/spacejam/sled/blob/main/README.md) · [releases](https://github.com/spacejam/sled/releases) · [commits](https://github.com/spacejam/sled/commits/main)

**heed / LMDB**
- [heed crates.io](https://crates.io/crates/heed) · [docs.rs](https://docs.rs/heed/latest/heed/) · [repo](https://github.com/meilisearch/heed) · [README](https://github.com/meilisearch/heed/blob/main/README.md)
- [lmdb-master-sys](https://crates.io/crates/lmdb-master-sys) · [heed EnvOpenOptions source](https://github.com/meilisearch/heed/blob/main/heed/src/envs/env_open_options.rs)
- [LMDB introduction](http://www.lmdb.tech/doc/index.html) — ACID, MVCC, single writer
- [LMDB API](http://www.lmdb.tech/doc/group__mdb.html) — `mdb_env_set_mapsize`
- [lmdb.h](https://github.com/LMDB/lmdb/blob/mdb.master/libraries/liblmdb/lmdb.h) — MDB_NOSYNC, MDB_MAXKEYSIZE 511, Windows notes
- [LMDB COPYRIGHT](https://github.com/LMDB/lmdb/blob/mdb.master/libraries/liblmdb/COPYRIGHT) · [LICENSE](https://github.com/LMDB/lmdb/blob/mdb.master/libraries/liblmdb/LICENSE) · [tags](https://github.com/LMDB/lmdb/tags) · [commits](https://github.com/LMDB/lmdb/commits/mdb.master3)
- [Meilisearch Cargo.toml (heed dependency)](https://github.com/meilisearch/meilisearch/blob/main/Cargo.toml) · [heed issue #150](https://github.com/meilisearch/heed/issues/150)

**rocksdb**
- [crates.io](https://crates.io/crates/rocksdb) · [librocksdb-sys](https://crates.io/crates/librocksdb-sys) · [repo](https://github.com/rust-rocksdb/rust-rocksdb) · [README](https://github.com/rust-rocksdb/rust-rocksdb/blob/master/README.md) · [CI](https://github.com/rust-rocksdb/rust-rocksdb/blob/master/.github/workflows/rust.yml)
- [RocksDB options.h (WriteOptions.sync)](https://github.com/facebook/rocksdb/blob/main/include/rocksdb/options.h) · [db.h (single read-write open)](https://github.com/facebook/rocksdb/blob/main/include/rocksdb/db.h) · [RocksDB repo/license files](https://github.com/facebook/rocksdb)

**Turso**
- [crates.io](https://crates.io/crates/turso) · [repo](https://github.com/tursodatabase/turso) · [README](https://github.com/tursodatabase/turso/blob/main/README.md)
