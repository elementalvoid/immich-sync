# SQLite via rusqlite for the sync-state store

Status: accepted
Date: 2026-10-12

The sync engine needs a durable local store for one row per sync item, a version chain, attempts, and per-folder settings. We chose SQLite through `rusqlite` with the `bundled` feature, in WAL mode with `synchronous=FULL`, one writer connection, and reader connections for the UI. WAL gives many readers and one writer, which matches the engine and UI split. SQL covers the query patterns directly, and `rusqlite_migration` covers schema changes. The alternatives lose on this app's own criteria: redb needs hand-built indexes and migrations, fjall yanked its 3.0–3.1.5 releases and recently fixed a missing fsync, sled remains alpha, LMDB caps keys at 511 bytes, sqlx is the same engine with an async facade, and RocksDB is build-heavy. The cost is one bundled C amalgamation and an update cadence for the bundled SQLite version. Full comparison: `docs/research/embedded-store-options.md`.

## Considered Options

- **sqlx with SQLite.** Same engine, async machinery that one writer does not need.
- **redb.** The only credible pure-Rust fallback. The cost is manual secondary indexes, counters, and migrations, plus Windows locking issues in its tracker.
- **fjall, sled, heed/LMDB, rocksdb, Turso.** Each rejected for a specific documented reason in the research report.

## Consequences

- The database lives in the local data directory. It never lives on a network or sync folder, because WAL does not work there.
- One writer connection uses `BEGIN IMMEDIATE` and a busy timeout. Readers use their own connections.
- Startup runs `PRAGMA quick_check`. The UI offers a rebuild path that rescans folders and pre-checks the server.
- The bundled SQLite version lags upstream. Check it on each dependency update.
- Paths need a case-insensitive comparison on Windows, so `C:\Foo` and `c:\foo` do not become two sync items.
