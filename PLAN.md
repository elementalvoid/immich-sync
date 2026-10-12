# immich-sync — Project Plan

Status: planning. No code yet.
Date: 2026-10-12
Research: [API research](docs/research/immich-sync-client-research.md), [embedded store options](docs/research/embedded-store-options.md), [Rust engine crates](docs/research/rust-sync-engine-crates.md)
Spec: [SPEC.md](SPEC.md)
Tickets: [GitHub](https://github.com/elementalvoid/immich-sync/issues)

## Goal

A background sync client for Immich on Windows. The user selects local folders in a config window. The client uploads new and changed files to one Immich server. It shows a tray icon and runs at login. The user can set bandwidth and concurrency limits. The client retries retriable errors and marks permanent errors for the user.

## Non-goals for v1

- Download or two-way sync
- Automatic server-side deletion
- Multiple Immich servers or accounts
- OAuth login
- macOS and Linux releases. The code stays portable, but v1 ships on Windows.

## Decisions

### Confirmed

| Decision | Answer | Record |
|---|---|---|
| Shell stack | Rust with GPUI-Kit, `tray-icon`, `muda`, `cargo-packager` | [ADR 0001](docs/adr/0001-rust-gpui-kit-shell.md) |
| Sync direction | Upload-only. The state model stays open to later directions. | [ADR 0002](docs/adr/0002-upload-only-with-deletion-review.md) |
| Local delete | No automatic server change. A deletion review with a per-file choice. | [ADR 0002](docs/adr/0002-upload-only-with-deletion-review.md) |
| Auth methods | Password login plus API-key fallback. | [ADR 0003](docs/adr/0003-credential-strategy.md) |
| Credential store | The password only. The session token stays in memory. | [ADR 0003](docs/adr/0003-credential-strategy.md) |
| Immich versions | Immich v3.0 and later. | this plan |
| Accounts | One server, one account. | this plan |
| Deletion review actions | Keep on server or move to trash. No permanent delete. | [ADR 0002](docs/adr/0002-upload-only-with-deletion-review.md) |
| Rename detection | By content hash and size. No review item for a rename. | [ADR 0002](docs/adr/0002-upload-only-with-deletion-review.md) |
| Changed files | New version: upload, copy associations, stack with the old asset. | [ADR 0004](docs/adr/0004-file-changes-create-a-new-version.md) |
| Old versions | Kept in the stack. | [ADR 0004](docs/adr/0004-file-changes-create-a-new-version.md) |
| Carried state | Tags move to the new version. Faces, descriptions, and custom metadata stay on the old version. | this plan |
| Settings store | TOML in the OS config directory. XDG-ready, no hardcoded Windows paths. | this plan |
| Path layout | Config in the OS config directory. State in the local data directory. | this plan |
| Sync-state store | SQLite via `rusqlite` (bundled), WAL, `synchronous=FULL`. | [ADR 0005](docs/adr/0005-sqlite-via-rusqlite-for-state.md) |
| Album scope | User choice per mapped folder: leaf folders or the whole source folder. | this plan |
| Tray display | Activity state, review count, upload progress, last error. | this plan |
| Visual direction | Dashboard with status cards. Per-folder and global state. | this plan |
| Theme | Follow the system theme. | this plan |
| Product name | Immich Sync. | this plan |
| Build order | UI and tray first. Packaging is normal work, not a spike. | this plan |
| Notifications | A permanent error, a lost connection, or a new deletion review item. | this plan |
| Default skips | Hidden files, symlinks, temporary files, system files. | this plan |

Album mapping is opt-in per source folder. The user chooses the album scope per mapped folder.

## Architecture

Cargo workspace with three crates:

| Crate | Role |
|---|---|
| `immich-client` | Typed client for the Immich API: login, credential validation, bulk upload check, upload. |
| `sync-core` | Engine: discovery, hashing, queue, retry, limits, state DB, deletion detection. No UI imports. |
| `app` | GPUI-Kit shell: config window, tray icon, deletion review, notifications, autostart. |

Dependency rule: `app` calls `sync-core` and reads its events. `sync-core` never imports the shell. This rule keeps the shell replaceable.

Engine components are hand-written on adopted crates: `notify` + `notify-debouncer-full` (watching), `ignore` (scan and skip rules), `sha1` + `base64` (checksums), `reqwest` (streaming multipart), `backon` (backoff), `tokio::sync::Semaphore` (concurrency), and a SQLite table plus worker loop for the queue. `governor` is available for request pacing and, with weighted cells, for bandwidth. See [Rust engine crates](docs/research/rust-sync-engine-crates.md).

### Paths

All paths come from a platform-directory crate (for example `directories`). The config lives in the OS config directory. The state database lives in the local data directory. Windows is the first target. Linux and macOS use the XDG and platform paths from the same crate.

### Sync pipeline

1. Discover: the watcher and the reconciliation scan report local files.
2. Hash: the engine computes the SHA-1 of each new or changed file.
3. Check: `POST /assets/bulk-upload-check` returns `accept` or `reject` for each checksum.
4. Upload: the engine sends each accepted file with `POST /assets` and the `x-immich-checksum` header.
5. Record: the engine writes the result to the state DB. `created` and `duplicate` both count as success.

Immich has no resumable upload. A network drop mid-file restarts that file from byte 0.

### Missing files

The reconciliation scan compares the state DB against the filesystem. A tracked path that no longer exists becomes a missing file. One exception exists. A new file with the same content and size is a rename. The engine keeps the asset mapping and raises no review item. For a real missing file, the engine raises an event. The config window shows the deletion review. The tray icon shows a status indicator. The server changes only after the user confirms.

### Changed files

A tracked file that changes in place is the same sync item with a new SHA-1. The client uploads the new bytes as a new asset. It then calls `PUT /assets/copy` with the old asset as source and the new asset as target. That call moves albums, shared links, favorite state, sidecar, and stack membership to the new version. If the old asset is in no stack, the client calls `POST /stacks` with the new asset first. The old version stays in the stack. The server state changes here without a review step, because the user changed the file on purpose. The client also applies the old asset's tags to the new version.

### State DB (SQLite)

- `source_folder`: path, recursive flag, album mapping flag, album scope.
- `sync_item`: path, size, mtime, SHA-1, current asset id, stack id, state, last error class.
- `version`: item id, asset id, SHA-1, created at, current flag.
- `attempt`: item id, start time, outcome, error class.

Store settings: bundled SQLite, WAL, `synchronous=FULL`, one writer connection with `BEGIN IMMEDIATE` and a busy timeout, reader connections for the UI, and `PRAGMA quick_check` at startup. The database lives in the local data directory. See [ADR 0005](docs/adr/0005-sqlite-via-rusqlite-for-state.md).

### Retry policy

- Retriable: timeouts, transport errors, 408, 425, 429, 500, 502, 503, 504. Honour `Retry-After`.
- Permanent: 400, 403, 404, 409, 413, 415, 422, and 401 after one re-login.
- Backoff: exponential with full jitter, capped. After the cap, the item moves to the dead letter state.
- A run of connection failures trips the circuit breaker.

### Limits

- Concurrency: a semaphore with the user-set size.
- Bandwidth: a token bucket over the request body.
- Both limits are live-adjustable and persisted.

## Milestones

| Milestone | Result | Exit test |
|---|---|---|
| M0 Shell | GPUI-Kit config window and working tray icon | The tray menu opens and hides the window. The visual feel is approved. |
| M1 Engine | Headless `sync-core` with tests against a mock server | A 10k-file tree syncs. Restart resumes. The dead letter works. Missing files appear as events. |
| M2 Integration | Config, keyring, deletion review, tray status | A user configures and syncs without a terminal. |
| M3 Reliability | Failure visibility, diagnostics, autostart | The user sees and retries a permanent failure. |
| M4 Release | Signed NSIS installer and docs | A stranger installs the app and syncs a folder. |

The tray and GPUI integration is the main technical risk. M0 tests it first, by design.

## Tickets

The tickets live on GitHub: https://github.com/elementalvoid/immich-sync/issues.
Each ticket declares its blockers as a native GitHub issue dependency. The frontier is #1 and #15.

| # | Ticket | Blocked by |
|---|---|---|
| [#1](https://github.com/elementalvoid/immich-sync/issues/1) | Tray shell walking skeleton | — |
| [#15](https://github.com/elementalvoid/immich-sync/issues/15) | Mock Immich server for engine tests | — |
| [#2](https://github.com/elementalvoid/immich-sync/issues/2) | Dashboard with fake data | #1 |
| [#3](https://github.com/elementalvoid/immich-sync/issues/3) | Settings and paths | #2 |
| [#4](https://github.com/elementalvoid/immich-sync/issues/4) | Login end-to-end | #3 |
| [#5](https://github.com/elementalvoid/immich-sync/issues/5) | One-file upload tracer bullet | #3, #4, #15 |
| [#6](https://github.com/elementalvoid/immich-sync/issues/6) | Folder sync engine | #5 |
| [#7](https://github.com/elementalvoid/immich-sync/issues/7) | Retry and failed items | #6 |
| [#8](https://github.com/elementalvoid/immich-sync/issues/8) | Limits | #6 |
| [#9](https://github.com/elementalvoid/immich-sync/issues/9) | Album mapping | #6 |
| [#10](https://github.com/elementalvoid/immich-sync/issues/10) | Sidecars and live photos | #6 |
| [#11](https://github.com/elementalvoid/immich-sync/issues/11) | Versioned changes | #6 |
| [#12](https://github.com/elementalvoid/immich-sync/issues/12) | Deletion review | #6 |
| [#13](https://github.com/elementalvoid/immich-sync/issues/13) | Diagnostics | #6 |
| [#14](https://github.com/elementalvoid/immich-sync/issues/14) | Packaged release | #7–#12 |

## Risks

| Risk | Impact | Mitigation |
|---|---|---|
| `tray-icon` does not coexist with GPUI's event loop | Shell change to Tauri 2 or GPUI direct | T0.4 fails fast, before engine work starts. |
| `gpui-pre` API churn | Upgrade cost | Pin exactly. Upgrade only at milestone boundaries. |
| GPUI-Kit Windows shell bugs | Ugly tray or window behaviour | Test early. Track the open issues. Report bugs upstream. |
| Renames appear as deletions | Review noise | Rename detection by content hash (open question). |
| Hashing large libraries is slow | First run takes hours | Persist SHA-1. Hash only new or changed files. |
| Password in the credential store | Credential exposure | OS store only. Never write it to the config file or the logs. |

## Open questions

None. Build-ready requirements live in [SPEC.md](SPEC.md). The next step is the ticket split.
