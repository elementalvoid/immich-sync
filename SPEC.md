# immich-sync — Specification

Status: build-ready draft.
Date: 2026-10-12.
Companion documents: [PLAN.md](PLAN.md), [GLOSSARY.md](GLOSSARY.md), [docs/adr/](docs/adr/), [docs/research/](docs/research/).

## 1. Purpose

Define the v1 requirements for Immich Sync: a desktop tray application that uploads files from user-selected local folders to one Immich server.

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
- Per-folder overrides of the default skip rules.
- Copying faces, descriptions, or custom metadata to a new version.

## 3. Requirements

Acceptance criteria are testable. "Default" values are configurable.

### R1 — Connection and authentication

- R1.1 The config window accepts a server URL, an email address, and a password.
- R1.2 The client stores the password in the OS credential store. It never writes the password to the config file or the logs.
- R1.3 The client logs in at startup with `POST /api/auth/login` (Immich's global API prefix). The session token stays in memory.
- R1.4 After a 401, the client re-logs in once with the stored password. Once a run of consecutive failed requests reaches the attention threshold (default 3, reset by a success), the connection enters a needs-attention state and the tray shows it. One refused batch adds at most two failures to the run (the 401 and the retry after the re-login), so a single transient batch stays below the alarm. The run is separate from the circuit breaker's run (R6.6).
- R1.5 The config window also accepts an API key. The key is validated at startup and stored in the OS credential store, like the password.
- R1.6 The client shows a clear message when the server disables password login.

Acceptance: password login works. API-key login works on a server with password login disabled. A restart re-logs in without user input. A wrong password shows an error and does not crash.

### R2 — Source folders and watch rules

- R2.1 The user adds and removes source folders, typing the path or choosing it in the OS folder picker. A new folder's settings (recursive, album mapping, and album scope) are confirmed in a setup dialog before the client records it or starts its scan; Add records them together and Cancel writes nothing. Each folder has a recursive toggle.
- R2.2 The client skips hidden files and folders, symlinks and junctions, temporary and partial files, and system files by default.
- R2.3 The watcher reports changes while the client runs. A reconciliation scan runs at startup and every 30 minutes by default.
- R2.4 The client compares paths case-insensitively on Windows and on macOS, where APFS and HFSX are case-insensitive by default. A case-sensitive volume can hold two files that differ only in case; they share one path key, so one of them may not sync. It supports long paths.
- R2.5 A source folder that disappears (for example, an unplugged drive) shows a folder status, not an error per file.

Acceptance: a file added while the client is stopped is found at startup. A file added while the client runs is found within 10 seconds. A file skipped by the watcher is found by the next scan. Hidden and temporary files never upload.

### R3 — Upload pipeline

- R3.1 New files are hashed with SHA-1, checked with `POST /api/assets/bulk-upload-check` in batches, and uploaded with `POST /api/assets` and the `x-immich-checksum` header.
- R3.2 `created` and `duplicate` both count as success.
- R3.3 A sidecar file uploads as `sidecarData`. A live photo video uploads first, and the photo links it with `livePhotoVideoId`.
- R3.4 Album mapping is opt-in per folder. The album scope is per folder: leaf folders or the whole source folder. A new folder's mapping, scope, and album title are confirmed with its other settings before it syncs. The title is the user's name for the album the folder itself holds (the whole-folder album, or the album for files directly in the source folder); a leaf subfolder's album keeps the subfolder's name. A blank title means the folder's name. The title is stored with the folder's album row even while mapping is off, and changing it renames the album with `PATCH /api/albums/{id}` (`UpdateAlbumDto.albumName`, Stable, `album.update`). A rename that cannot reach the server keeps the title and its reason, and is retried on the next scan, restart, or reconnect; an album deleted in Immich is created again with the title and its new id recorded. The title is never polled from Immich, and Immich does not require titles to be unique.
- R3.5 Only Immich operations with `x-immich-state: Stable` are used for sync correctness.
- R3.6 Upload success means the server returned the asset id. Server-side processing continues asynchronously.
- R3.7 The sync tag is on by default in Settings; the user can turn it off. When it is on, the client upserts the fixed tag `immich-sync` with `PUT /api/tags` (`x-immich-state: Stable`) and keeps the returned id for the run, so a tag deleted in Immich is created again on the next run. Every asset an upload settles with (`created` or `duplicate`, including a changed file's new version) gets the tag with `PUT /api/tags/assets`. Tagging runs after the upload is recorded, so a tag failure leaves the item uploaded and is reported in Settings instead. With an API key, tagging needs the `tag.create`, `tag.asset`, and `asset.update` permissions; Settings names a missing one. Turning the setting off stops new tagging and deletes no tag.

  Turning the tag on also backfills it onto every asset the client has ever uploaded: each item's current asset, every old version in the version chain, and every asset the server answered as a duplicate, each once. The ids go out in batches of a few hundred per `PUT /api/tags/assets` call, which is idempotent, so a repeated batch is safe and an asset the server no longer holds is filtered out instead of failing its batch. Settings shows the progress, for example "Tagging existing assets: 12,340 of 48,000", and a completed state. The state database records the cursor and count in one write per batch, and the completion once at the end, so a restart resumes from the last completed batch, a finished backfill is not repeated, and a restart after an interruption does not start the library over. "Tag existing assets now" clears the record and runs the whole library again. The backfill runs beside the upload pipeline, never on the UI thread. A transient batch failure is retried with the retry policy (R6.1); a permanent one records its reason, stops the backfill, and keeps the progress so a later attempt continues from it. Turning the setting off pauses the backfill and turning it on resumes it; the completion record survives the toggle.

Acceptance: a 100-file folder syncs. Duplicate content is not uploaded twice. A mapped folder creates the expected album. A sidecar lands with its asset. Unsupported formats produce a permanent error, not a retry loop. With the sync tag on, a new upload and a duplicate both carry it, and a tag failure leaves both uploaded. Turning the tag on in a library that already has uploads tags every current asset, every old version, and every duplicate; the progress reaches a completed state; a restart does not repeat the finished backfill; "Tag existing assets now" re-runs it safely; an interrupted backfill resumes from its last completed batch; a permanent refusal shows its reason and keeps the progress that landed.

### R4 — Changed files

- R4.1 A tracked file that changes in place keeps its sync item identity.
- R4.2 The client uploads the new bytes as a new asset. It calls `PUT /api/assets/copy` with the old asset as source and the new asset as target, with albums, shared links, favorite, sidecar, and stack enabled. It applies the old asset's tags to the new asset with the tag endpoints.
- R4.3 If the old asset is in no stack, the client calls `POST /api/stacks` with the new asset first and the old asset second.
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
- R6.6 A run of connection failures trips a circuit breaker. The client probes with the credential validation endpoint before it resumes. The breaker's run (default 5) is separate from and longer than the connection's attention threshold (R1.4), so the tray changes before the queue does.
- R6.7 The upload queue survives a restart. No completed item uploads twice.

Acceptance: stopping the server mid-sync produces backoff retries, not a hot loop. A 415 response produces exactly one permanent failure. A restart resumes the queue. A full disk error does not corrupt the database.

### R7 — Limits

- R7.1 The bandwidth limit is user-settable in bytes per second. 0 means unlimited. Changes apply without a restart.
- R7.2 The concurrency limit is user-settable. The default is 4. Changes apply without a restart.
- R7.3 Both limits persist across restarts.
- R7.4 R7 governs the sync engine. The manual one-file upload in the Upload view, whether its path is typed or chosen in the OS file picker, is deliberately exempt from both limits: it is an explicit one-off request, not sync work. This is by design, not a defect.

Acceptance: a sync at a 1 MB/s limit produces near-limit observed throughput. Raising concurrency during a sync starts new workers without a restart.

### R8 — Shell and user experience

- R8.1 The config window is a dashboard with status cards: one global card and one per source folder. The global card shows the activity state, the progress and counts across every folder, the upload rate and the uploads on the wire, the connection state, the last error, and the last reconciliation scan, which is the newest of the folders' scans. While a reconciliation scan runs, **Sync now** reads "Checking…". A source folder's card opens its detail: the full path, its live state, progress, and counts, its own upload activity, its settings, its full last error, the last successful upload and last reconciliation scan, the list of the assets it has uploaded, an Open folder action, links into the Failed and Review views filtered to that folder, and Remove behind a confirmation.
- R8.2 The tray icon shows the activity state (idle, syncing, paused, needs attention), the review count, upload progress, and the last error in its tooltip.
- R8.3 The tray menu offers Show, Pause, Resume, Sync now, and Quit.
- R8.4 Closing the window hides it. Quit exits the client. A second launch focuses the first instance.
- R8.5 Autostart at login is a setting. The default is off. Windows uses its Run key, Linux an XDG autostart entry, and macOS a Login Item (`SMAppService`), with a `~/Library/LaunchAgents` fallback when an unsigned bundle is refused.
- R8.6 The theme follows the system theme.
- R8.7 Pause stops new uploads. In-flight uploads finish or stop at a safe point.

Acceptance: every tray action works from a cold start. A second launch does not start a second process. Pause stops new uploads within 5 seconds.

### R9 — Settings and paths

- R9.1 Settings live in a TOML file in the OS config directory.
- R9.2 The state database lives in the local data directory. It never lives on a network share or a cloud-sync folder.
- R9.3 All paths come from a platform-directory abstraction. No hardcoded Windows path. Linux uses XDG paths, and macOS resolves the config, the state database, and the logs under `~/Library/Application Support/immich-sync/`.

Acceptance: on Windows, config resolves under `%APPDATA%` and state under `%LOCALAPPDATA%`. A test build resolves XDG paths on Linux. A repository check finds no hardcoded `C:\` paths.

### R10 — Diagnostics and notifications

- R10.1 The client notifies the user about a permanent error or a lost connection, and about a new deletion review item. A permanent error already parked at startup is not re-announced.
- R10.2 A diagnostics view shows the client version, the server version, the granted permissions, the last error per class, and current throughput, and offers one action that collects the logs, versions, database information, configuration, and sizing into a single archive, without blocking the UI.
- R10.3 The client writes a rotating log file. The log contains no password, API key, or session token.
- R10.4 The diagnostics view offers a state rebuild. The rebuild rescans folders and re-checks the server.

Acceptance: a permanent error raises one notification. Grep of the log finds no credential. A rebuild recovers a state database deleted by hand.

### R11 — Packaging and distribution

- R11.1 `cargo-packager` produces an NSIS installer for Windows, deb and AppImage packages for Linux, and one DMG per architecture for macOS: `Immich Sync_<version>_aarch64.dmg` and `Immich Sync_<version>_x64.dmg`. The Windows installer offers an install-mode choice and defaults to a system-wide install under `C:\Program Files`, with a current-user install as the alternative; either mode ships the app icon on the installed executable.
- R11.2 Binaries ship unsigned in v1 on every platform. The Windows installer warns under SmartScreen. macOS blocks the first launch until the user allows it in **System Settings › Privacy & Security › Open Anyway**, because there is no Apple Developer Program account, no Developer ID certificate, and no notarization ([ADR 0008](docs/adr/0008-macos-unsigned-distribution.md)). Windows code signing belongs to the public-release ticket.
- R11.3 CI builds every platform, publishes a SHA256SUMS file, and contains provenance attestation steps guarded by a repository variable. The steps stay skipped while the repository is private. `gh attestation verify` succeeds when they are enabled.
- R11.4 The README documents install and verification steps for every platform.

Acceptance: a clean Windows machine installs from the NSIS installer. A clean Linux desktop installs from the deb or the AppImage. A clean Mac drags the DMG's app to Applications and launches it through Open Anyway. The provenance attestation verifies for every artifact once the repository is public.

## 4. Non-functional requirements

- N1 Platform: Windows 10 and 11 (x64, arm64 if the toolchain permits), Linux (deb and AppImage), and macOS 13 (Ventura) or later on arm64 and x86_64. The macOS packages are unsigned and unnotarized (R11.2).
- N2 Responsiveness: database reads never block the UI. The common views load in under 16 ms at 300k items. Measure this in M2.
- N3 Security: TLS verification stays on. Credentials live in the OS credential store only. Logs are redacted.
- N4 Data safety: WAL with `synchronous=FULL`, `PRAGMA quick_check` at startup, and a rebuild path. Server state changes only from user actions or the version flow in R4.
- N5 Accessibility: every control has an accessible name. Keyboard navigation covers the config window. Test with Windows UI Automation where the framework permits.
- N6 Quality gates: `cargo fmt`, `cargo clippy` with no warnings, and tests for the engine. The engine has no UI imports.

## 5. Data model summary

- `source_folder`: path, recursive flag, album mapping flag, album scope, last reconciliation scan, last successful upload.
- `album`: mapped folder path key, album title, album id, rename pending flag and reason.
- `sync_item`: path, size, mtime, SHA-1, current asset id, stack id, state, last error class.
- `version`: item id, asset id, SHA-1, created at, current flag.
- `attempt`: item id, start time, outcome, error class.
- `tag_backfill`: completion time, the last completed batch's last asset id, tagged count.

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
- [ADR 0008](docs/adr/0008-macos-unsigned-distribution.md) — macOS ships unsigned, without an Apple Developer Program account.
- [API research](docs/research/immich-sync-client-research.md), [embedded store research](docs/research/embedded-store-options.md), [Rust engine crates](docs/research/rust-sync-engine-crates.md).
