# immich-sync — Specification

Status: build-ready draft.
Date: 2026-10-12.
Companion documents: [PLAN.md](PLAN.md), [GLOSSARY.md](GLOSSARY.md), [docs/adr/](docs/adr/), [docs/research/](docs/research/).

## 1. Purpose

Define the v1 requirements for Immich Sync: a Windows tray application that uploads files from user-selected local folders to one Immich server.

## 2. Scope

### In scope (v1)

- One Immich server and one account per install.
- Upload-only sync from local folders.
- Versioned uploads for changed files, with association and tag carryover.
- A deletion review for missing local files. No automatic server change.
- Bandwidth and concurrency limits.
- Retry classification with a visible failed-items list.
- Tray icon, dashboard window, notifications, autostart.

### Out of scope (v1)

- Download or two-way sync.
- Multiple servers, accounts, or users per install.
- OAuth login.
- macOS releases. The code stays portable. Revisit after v1 ships.
- Per-folder overrides of the default skip rules.
- Copying faces, descriptions, or custom metadata to a new version.

## 3. Requirements

Acceptance criteria are testable. "Default" values are configurable.

### R1 — Connection and authentication

- R1.1 The config window accepts a server URL, an email address, and a password.
- R1.2 The client stores the password in the OS credential store. It never writes the password to the config file or the logs.
- R1.3 The client logs in at startup with `POST /auth/login`. The session token stays in memory.
- R1.4 After a 401, the client re-logs in once with the stored password. After a second 401, the connection enters a needs-attention state and the tray shows it.
- R1.5 The config window also accepts an API key. The key is validated at startup and stored in the OS credential store, like the password.
- R1.6 The client shows a clear message when the server disables password login.

Acceptance: password login works. API-key login works on a server with password login disabled. A restart re-logs in without user input. A wrong password shows an error and does not crash.

### R2 — Source folders and watch rules

- R2.1 The user adds and removes source folders. Each folder has a recursive toggle.
- R2.2 The client skips hidden files and folders, symlinks and junctions, temporary and partial files, and system files by default.
- R2.3 The watcher reports changes while the client runs. A reconciliation scan runs at startup and every 30 minutes by default.
- R2.4 The client compares paths case-insensitively on Windows. It supports long paths.
- R2.5 A source folder that disappears (for example, an unplugged drive) shows a folder status, not an error per file.

Acceptance: a file added while the client is stopped is found at startup. A file added while the client runs is found within 10 seconds. A file skipped by the watcher is found by the next scan. Hidden and temporary files never upload.

### R3 — Upload pipeline

- R3.1 New files are hashed with SHA-1, checked with `POST /assets/bulk-upload-check` in batches, and uploaded with `POST /assets` and the `x-immich-checksum` header.
- R3.2 `created` and `duplicate` both count as success.
- R3.3 A sidecar file uploads as `sidecarData`. A live photo video uploads first, and the photo links it with `livePhotoVideoId`.
- R3.4 Album mapping is opt-in per folder. The album scope is per folder: leaf folders or the whole source folder.
- R3.5 Only Immich operations with `x-immich-state: Stable` are used for sync correctness.
- R3.6 Upload success means the server returned the asset id. Server-side processing continues asynchronously.

Acceptance: a 100-file folder syncs. Duplicate content is not uploaded twice. A mapped folder creates the expected album. A sidecar lands with its asset. Unsupported formats produce a permanent error, not a retry loop.

### R4 — Changed files

- R4.1 A tracked file that changes in place keeps its sync item identity.
- R4.2 The client uploads the new bytes as a new asset. It calls `PUT /assets/copy` with the old asset as source and the new asset as target, with albums, shared links, favorite, sidecar, and stack enabled. It applies the old asset's tags to the new asset with the tag endpoints.
- R4.3 If the old asset is in no stack, the client calls `POST /stacks` with the new asset first and the old asset second.
- R4.4 The old version stays. The state database records the version chain and the stack id.
- R4.5 An unchanged SHA-1 means no upload.

Acceptance: an edited file produces a new asset. Albums, shared links, favorite state, tags, and the sidecar survive. The timeline shows one stack entry. The old asset remains retrievable.

### R5 — Missing files and the deletion review

- R5.1 A tracked path that disappears becomes a missing file. The client changes nothing on the server before a decision.
- R5.2 A rename is detected by content hash and size. The item keeps its asset mapping, and no review item appears.
- R5.3 The deletion review lists missing files with two actions per file: keep on server, or move the asset to trash. There is no permanent delete in v1.
- R5.4 The tray icon shows the review count. A new review item raises a notification.
- R5.5 A trash action without the required permission shows an actionable message. The item stays in the review.

Acceptance: deleting a file creates one review item. "Keep" clears the item and leaves the asset. "Trash" moves the asset to trash. A rename creates no review item. A second instance of the same content in another folder creates no review item.

### R6 — Failure handling and retry

- R6.1 The client retries transport errors and HTTP 408, 425, 429, 500, 502, 503, and 504 with exponential backoff and full jitter, capped at 8 attempts by default.
- R6.2 The client honours a `Retry-After` header when the server sends one.
- R6.3 The client does not retry HTTP 400, 403, 404, 409, 413, 415, and 422. It marks those permanent.
- R6.4 The client persists the failure class: auth_failed, permission_denied, quota_exceeded, unsupported_format, network_transient, server_transient, local_read_error, too_large.
- R6.5 After the attempt cap, the item enters the dead letter state and appears in the failed-items view with a manual retry action.
- R6.6 A run of connection failures trips a circuit breaker. The client probes with the credential validation endpoint before it resumes.
- R6.7 The upload queue survives a restart. No completed item uploads twice.

Acceptance: stopping the server mid-sync produces backoff retries, not a hot loop. A 415 response produces exactly one permanent failure. A restart resumes the queue. A full disk error does not corrupt the database.

### R7 — Limits

- R7.1 The bandwidth limit is user-settable in bytes per second. 0 means unlimited. Changes apply without a restart.
- R7.2 The concurrency limit is user-settable. The default is 4. Changes apply without a restart.
- R7.3 Both limits persist across restarts.

Acceptance: a 1 MB/s limit produces near-limit observed throughput. Raising concurrency during a sync starts new workers without a restart.

### R8 — Shell and user experience

- R8.1 The config window is a dashboard with status cards: one global card and one per source folder.
- R8.2 The tray icon shows the activity state (idle, syncing, paused, needs attention), the review count, upload progress, and the last error in its tooltip.
- R8.3 The tray menu offers Show, Pause, Resume, Sync now, and Quit.
- R8.4 Closing the window hides it. Quit exits the client. A second launch focuses the first instance.
- R8.5 Autostart at login is a setting. The default is off.
- R8.6 The theme follows the system theme.
- R8.7 Pause stops new uploads. In-flight uploads finish or stop at a safe point.

Acceptance: every tray action works from a cold start. A second launch does not start a second process. Pause stops new uploads within 5 seconds.

### R9 — Settings and paths

- R9.1 Settings live in a TOML file in the OS config directory.
- R9.2 The state database lives in the local data directory. It never lives on a network share or a cloud-sync folder.
- R9.3 All paths come from a platform-directory abstraction. No hardcoded Windows path. Linux uses XDG paths.

Acceptance: on Windows, config resolves under `%APPDATA%` and state under `%LOCALAPPDATA%`. A test build resolves XDG paths on Linux. A repository check finds no hardcoded `C:\` paths.

### R10 — Diagnostics and notifications

- R10.1 The client notifies the user about a permanent error or a lost connection, and about a new deletion review item.
- R10.2 A diagnostics view shows the server version, the granted permissions, the last error per class, and current throughput.
- R10.3 The client writes a rotating log file. The log contains no password, API key, or session token.
- R10.4 The diagnostics view offers a state rebuild. The rebuild rescans folders and re-checks the server.

Acceptance: a permanent error raises one notification. Grep of the log finds no credential. A rebuild recovers a state database deleted by hand.

### R11 — Packaging and distribution

- R11.1 `cargo-packager` produces an NSIS installer for Windows and deb and AppImage packages for Linux.
- R11.2 Windows binaries ship unsigned in v1. The installer warns under SmartScreen. Code signing belongs to the public-release ticket.
- R11.3 CI builds both platforms, publishes a SHA256SUMS file, and contains provenance attestation steps guarded by a repository variable. The steps stay skipped while the repository is private. `gh attestation verify` succeeds when they are enabled.
- R11.4 The README documents install and verification steps for both platforms.

Acceptance: a clean Windows machine installs from the NSIS installer. A clean Linux desktop installs from the deb or the AppImage. The provenance attestation verifies for both artifacts once the repository is public.

## 4. Non-functional requirements

- N1 Platform: Windows 10 and 11 (x64, arm64 if the toolchain permits) and Linux (deb and AppImage) ship in v1. macOS stays portable but deferred.
- N2 Responsiveness: database reads never block the UI. The common views load in under 16 ms at 300k items. Measure this in M2.
- N3 Security: TLS verification stays on. Credentials live in the OS credential store only. Logs are redacted.
- N4 Data safety: WAL with `synchronous=FULL`, `PRAGMA quick_check` at startup, and a rebuild path. Server state changes only from user actions or the version flow in R4.
- N5 Accessibility: every control has an accessible name. Keyboard navigation covers the config window. Test with Windows UI Automation where the framework permits.
- N6 Quality gates: `cargo fmt`, `cargo clippy` with no warnings, and tests for the engine. The engine has no UI imports.

## 5. Data model summary

- `source_folder`: path, recursive flag, album mapping flag, album scope.
- `sync_item`: path, size, mtime, SHA-1, current asset id, stack id, state, last error class.
- `version`: item id, asset id, SHA-1, created at, current flag.
- `attempt`: item id, start time, outcome, error class.

Item states: discovered, hashing, checking, pending, uploading, uploaded, duplicate, retry_wait, missing, review_keep, review_trash, failed_permanent, dead_letter.

## 6. Open issues

- The GPUI and tray integration is unproven. M0 is the gate. A failure changes the shell, not the requirements.
- The bundled SQLite version lags upstream. Re-check it on each dependency update.
- Default scan interval (30 minutes) and attempt cap (8) are engineering defaults. Adjust after measurement.

## 7. References

- [PLAN.md](PLAN.md) — milestones and ticket graph.
- [GLOSSARY.md](GLOSSARY.md) — canonical terms.
- [ADR 0001](docs/adr/0001-rust-gpui-kit-shell.md) — Rust and GPUI-Kit.
- [ADR 0002](docs/adr/0002-upload-only-with-deletion-review.md) — upload-only and deletion review.
- [ADR 0003](docs/adr/0003-credential-strategy.md) — credential strategy.
- [ADR 0004](docs/adr/0004-file-changes-create-a-new-version.md) — versioned changes.
- [ADR 0005](docs/adr/0005-sqlite-via-rusqlite-for-state.md) — state store.
- [ADR 0006](docs/adr/0006-hand-written-client-and-engine.md) — hand-written client and engine.
- [API research](docs/research/immich-sync-client-research.md), [embedded store research](docs/research/embedded-store-options.md), [Rust engine crates](docs/research/rust-sync-engine-crates.md).
