# Rust crates and prior art for a desktop folder-sync engine (Immich upload-only)

**Date:** 2026-10-12
**Scope:** Decide whether the `sync-core` engine for a Windows tray app (watch local folders → upload to one Immich server, upload-only v1) should be built from scratch or embedded from an existing engine, and which crates each component should use.
**Method:** crates.io API (`/api/v1/crates/<name>`, `/dependencies`, `/versions`) for version, release date, license and dependency facts; GitHub/Gitea REST APIs for repository activity and archived state; raw primary sources (Immich repo at tag `v3.3.1`, OpenAPI JSON specs at `v3.3.1` and `v1.137.3`, immich-go `main`, `@immich/cli` `v3.3.1`, rclone `master` docs, RustCrypto/progenitor/openapi-generator sources). One empirical test was run: `cargo progenitor 0.15.1` against the Immich `v3.3.1` spec. All version/date facts are as observed on 2026-10-12. Claims that are inference rather than primary source are flagged in **Uncertainty**.

---

## Build-vs-adopt summary

| Option | Verdict | One-line reason |
|---|---|---|
| Embed a whole sync engine (rustic_core, syncthing-rs, copia, duet, ensync, rclone/librclone) | **No** | They are backup-repository, delta/rsync, or peer-to-peer engines. None can target a third-party HTTP JSON API, and rclone has no Immich backend at all. |
| Embed a job-queue framework (apalis-sqlite, sqlxmq, background-jobs) | **No** | apalis-sqlite pulls a second SQLite stack (`sqlx`) against the `rusqlite` decision and is still `1.0.0-rc`; sqlxmq is PostgreSQL-only; background-jobs is AGPL-3.0 and has no SQLite backend. |
| Generate a full Rust Immich client from the spec | **No (v1)** | `progenitor` provably fails on this spec (`multipart/form-data`), and every JSON endpoint plus upload is 8 hand-written operations. Use the generator only if the API surface grows. |
| Reuse the official/prior-art *semantics* (from `@immich/cli`, immich-go, rclone) | **Yes** | Batching shape, checksum correlation, watch debouncing, two-tier retry, deletion safety rails — all documented behavior worth copying as design, not code. |
| Build `sync-core` from scratch on ~12 crates | **Yes** | The hard part — the upload state machine, Immich semantics, version chains, deletion review — is not supplied by any crate. |

---

## 1. Whole sync engines

Searched crates.io for `immich`, `sync engine`, `sync daemon`, `file sync engine`, `file synchronization`, `folder synchronization`, `rsync`, `two-way sync`, `unison file sync`, `backup library`, `content addressed storage`, `rclone`, plus the specific names below.

| Candidate | What it actually is | Version / released | License | Maintenance | Could it sync local folders to Immich? |
|---|---|---|---|---|---|
| [`rustic_core`](https://crates.io/crates/rustic_core) ([repo](https://github.com/rustic-rs/rustic_core)) | Library for **restic-format encrypted, deduplicated backup repositories** (chunking, packs, index, snapshots) | 0.13.0 / 2026-08-16 | Apache-2.0 OR MIT | Active (106,976 downloads) | **No.** It writes to storage backends using the restic repository format; Immich is an application HTTP API with an asset model. Its chunker/dedup solves a problem the Immich API already solves server-side by SHA-1. |
| [`syncthing`](https://crates.io/crates/syncthing) ([repo](https://github.com/jaycefayne/syncthing-rs)) | Rust **REST client for a running Syncthing daemon** ("controlling syncthing file synchronization through its REST interface") | 0.6.0 / 2025-09-21 | MIT | Low-to-moderate (8,925 downloads, 21 stars, last push 2026-01-29) | **No.** It is a control plane for an external syncthing *binary*, and syncthing is peer-to-peer between Syncthing instances, not an HTTP-API uploader. |
| [`copia`](https://crates.io/crates/copia) | "Pure Rust rsync-style **delta synchronization library**" | 0.3.0 / 2026-08-23 | MIT | New (created 2026-01, 10,844 downloads) | **No.** rsync delta-transfer against a cooperating peer; Immich has no delta/rolling-checksum endpoint. |
| [`duet`](https://crates.io/crates/duet) | "bi-directional synchronization" | 0.9.2 / 2026-08-09 | MIT | Small (2,492 downloads) | **No.** Two-way local/mirror semantics; v1 is upload-only and the remote is an API, not a filesystem. |
| [`ensync`](https://crates.io/crates/ensync) | Encrypted file synchroniser for untrusted central storage | 1.0.2 / 2024-08-25 | **GPL-3.0** | Quiet (21k downloads, last release 2024) | **No.** Own wire protocol + own storage layout; GPL-3.0 is also a licensing problem for a permissive desktop app. |
| [`ubiquity`](https://crates.io/crates/ubiquity) | "n-directional file synchronization utility" | 0.2.0 / 2020-04-05 | — | Stale | **No.** |
| `synchi` (0.1.4, 2026-03-21), `synx` (0.1.6, 2026-10-10), `msy` (0.6.10, 2026-10-09) | SSH/rsync-style sync CLIs | — | — | New, tiny (29–451 downloads) | **No.** SSH remotes. |
| [`cloud-storage-sync`](https://crates.io/crates/cloud-storage-sync) | "Embedded rsync-like local files and **Google Cloud Storage** buckets" | 0.6.0 / 2021-05-21 | — | Stale | **No.** GCS object-store specific. |
| `restic` ([crates.io](https://crates.io/crates/restic)) | Thin **wrapper for the restic CLI** | 0.1.0 / 2025-05-27 | MPL-2.0 | Single release, 608 downloads | **No.** Shells out to `restic`; restic has no Immich backend. |
| `duplicacy*` | — | — | — | Not found on crates.io | n/a |

**rclone.** The primary source is rclone's own [storage-systems overview](https://rclone.org/overview/): the backend list (A–Z) contains no Immich remote, and a full-text scan of that page for "immich" returns no backend. Immich is not a WebDAV/S3 drop-in for rclone's purposes, and rclone's generic [`HTTP` remote is documented as read-only](https://github.com/rclone/rclone/blob/master/docs/content/http.md): *"This remote is read only - you can't upload files to an HTTP server."* A GitHub issue search on `rclone/rclone` for `immich` returns 6 unrelated results (mostly Google Photos metadata issues), i.e. no backend work in flight.

**Rust bindings to rclone.** Two exist, neither useful here:

- [`librclone`](https://crates.io/crates/librclone) ([repo](https://github.com/trevyn/librclone)) 0.9.0 / 2025-02-12 — Rust bindings over the `librclone` C API, i.e. you can embed rclone's engine and call `rclone.RPC`/`rclone.RC`. Even with an embedded engine, there is no Immich backend to call, so this buys nothing.
- [`rclone-sdk`](https://crates.io/crates/rclone-sdk) 1.75.0 / 2026-08-19 — a typed client for the **rclone RC HTTP API**; requires a separately running `rclone rcd` process. Wrong shape for a tray app, and again no Immich backend.
- `rclone` on crates.io (0.1.1, 2019) is an unrelated "tools for git clone" crate — a name collision, not rclone.

**Conclusion for Q1:** no crate in the ecosystem provides an embeddable, target-agnostic file-sync *engine* that can be pointed at a third-party HTTP JSON API. The closest architectural relatives (rclone, syncthing, rustic/restic, duplicacy) all define their own remote protocol or repository format; adopting one would mean either reimplementing an Immich backend inside someone else's abstraction or syncing to a local intermediate directory and running a second process — strictly more moving parts than the hand-rolled engine.

---

## 2. Immich clients and code generation

### 2.1 Existing Rust Immich crates

| Crate | Version / released | License | Maintenance evidence | Fit |
|---|---|---|---|---|
| [`immich_api_rs`](https://crates.io/crates/immich_api_rs) | 1.137.3 / 2025-08-11 | Unlicense | **One version ever**, 450 downloads, **no `repository` field**. Version tracks Immich server `v1.137.3` ([released 2025-08-01](https://api.github.com/repos/immich-app/immich/releases/tags/v1.137.3)); the current server is [v3.3.1 (2026-10-08)](https://api.github.com/repos/immich-app/immich/releases?per_page=1) | Stale ~14 months and ~2 major server versions behind. Not adoptable. |
| [`immich`](https://crates.io/crates/immich) ([repo](https://github.com/anergictcell/immich)) | 0.2.0 / 2025-02-09 | MIT (repo shows no license) | 0 GitHub stars, last push 2025-05-25, two releases total, 1,598 downloads | Abandoned toy. |
| [`immich-lib`](https://crates.io/crates/immich-lib) | 1.2.0 / 2025-12-29 | MIT | 32 downloads; the crates.io `repository` URL (`github.com/richardjlyon/immich-lib`) returns **404 Not Found** | Scope is duplicate management, not upload. Source is gone. |
| [`immich-tools`](https://crates.io/crates/immich-tools) ([repo](https://git.enoent.fr/kernald/immich-tools)) | 0.5.0 / 2026-08-06 | MIT | Active; CI, changelog, release notes, 2,319 downloads | **The only credible Rust Immich codebase.** It is a CLI, not a library, and — importantly — it **deletes the upload operation** (see §2.3). |
| [`immich-analyze`](https://crates.io/crates/immich-analyze) | 0.6.1 / 2026-10-10 | — | New | AI description generator; not a client library. |

There is therefore **no maintained, reusable Rust Immich client library**. Any Rust path is "generate or hand-write".

Facts about the API surface that matter to any client, from the published spec at [`v3.3.1/open-api/immich-openapi-specs.json`](https://raw.githubusercontent.com/immich-app/immich/v3.3.1/open-api/immich-openapi-specs.json) (measured locally):

- 953,386 bytes, OpenAPI **3.0.0**, 193 paths, 279 operations, 438 schemas.
- `POST /assets` — `multipart/form-data` body (`AssetMediaCreateDto`), query params `key`/`slug`, header `x-immich-checksum` documented as *"sha1 checksum that can be used for duplicate detection before the file is uploaded"*; **200 = "Asset is a duplicate"**, 201 = uploaded.
- `POST /assets/bulk-upload-check` — `AssetBulkUploadCheckItem.checksum` is *"Base64 or hex encoded SHA1 hash"*, with a client-supplied `id` echoed back; `AssetUploadAction` = `accept` | `reject`; `AssetRejectReason` = `duplicate` | `unsupported-format`; results carry `assetId` and `isTrashed`.
- **No chunked or resumable upload exists.** A regex sweep of all 193 paths for `chunk|resum|tus|part|multipart` matches only unrelated paths (`/auth/status`, `/partners`, `/admin/maintenance/status`). There is no upload-session endpoint, no offset parameter and no `Content-Range` parameter anywhere. A multipart `POST /assets` is all-or-nothing per request.
- **`PUT /assets/{id}/original` (`replaceAsset`) is gone in v3.3.1.** The same path exists in [`v1.137.3`](https://raw.githubusercontent.com/immich-app/immich/v1.137.3/open-api/immich-openapi-specs.json) with `get: downloadAsset` + `put: replaceAsset`; in v3.3.1 the path has `get` only, and no operationId in the whole v3.3.1 spec contains "replace". Consequences: (a) immich-go's in-place replace path targets an endpoint the current spec does not document, and (b) the documented way to "replace" an asset today is upload-new + [`PUT /assets/copy`](https://raw.githubusercontent.com/immich-app/immich/v3.3.1/open-api/immich-openapi-specs.json) (`AssetCopyDto{sourceId,targetId,albums,favorite,sharedLinks,sidecar,stack}`) + optional `POST /stacks` (`StackCreateDto{assetIds}`) + `DELETE /assets`. See Uncertainty.

### 2.2 How Immich generates its own SDKs

Primary source, [`docs/docs/api.md` at v3.3.1](https://github.com/immich-app/immich/blob/v3.3.1/docs/docs/api.md): *"OpenAPI is used to generate the client (Typescript, Dart) SDK… The generated SDK is based on the `immich-openapi-specs.json` file, which is autogenerated by the server when running in development mode… You can find the generated client SDK in the `packages/sdk/client` for Typescript SDK and `mobile/generated/openapi` for Dart SDK"*, regenerated by `mise open-api`.

The build files are more specific than the docs page:

- TypeScript is generated with **oazapfts 7.5.0**, not openapi-generator — [`mise.toml`](https://github.com/immich-app/immich/blob/v3.3.1/mise.toml) task `open-api-typescript`: `oazapfts --optimistic --argumentStyle=object --useEnumType --allSchemas open-api/immich-openapi-specs.json packages/sdk/src/fetch-client.ts`, and [`packages/sdk/package.json`](https://github.com/immich-app/immich/blob/v3.3.1/packages/sdk/package.json) depends on `@oazapfts/runtime`.
- Dart is generated with **openapi-generator-cli** — [`open-api/openapitools.json`](https://github.com/immich-app/immich/blob/v3.3.1/open-api/openapitools.json) pins generator version **7.25.0**, and [`open-api/bin/generate-dart-sdk.sh`](https://github.com/immich-app/immich/blob/v3.3.1/open-api/bin/generate-dart-sdk.sh) generates `-g dart` then applies **5 hand-written patches** (including `api_client.dart.patch` and model patches) to the generated output.

**Lesson:** even Immich's first-party Dart SDK is generated with template patches on top of the generator. Generated ≠ zero-maintenance, even for the vendor.

### 2.3 Generating a Rust client from this spec

**`progenitor`** ([crates.io](https://crates.io/crates/progenitor), 0.15.1 released 2026-10-09, MPL-2.0, [repo](https://github.com/oxidecomputer/progenitor), 1,023 stars, last push 2026-10-11). This was **tested, not assumed**:

```
$ cargo install cargo-progenitor --locked   # installs 0.15.1
$ cargo progenitor -i immich-openapi-specs.json -o out -n immich_api -v 0.1.0
gen fail: UnexpectedFormat("unexpected content type: multipart/form-data")
Error: generation experienced errors
```

This matches the generator's source: [`progenitor-impl/src/method.rs`](https://github.com/oxidecomputer/progenitor/blob/main/progenitor-impl/src/method.rs) parses body content types with only `application/octet-stream`, `application/json`, `application/x-www-form-urlencoded` and `text/plain` in the accepted set, returning `Error::UnexpectedFormat` for anything else. `POST /assets` therefore cannot be generated at all, and generation aborts the whole spec.

This is independently corroborated by the only maintained Rust Immich codebase: [`immich-tools`](https://git.enoent.fr/kernald/immich-tools) generates its client in `build.rs` with `progenitor::Generator::default().generate_tokens(spec)`, and its [`scripts/update-immich-spec.sh`](https://git.enoent.fr/kernald/immich-tools/src/branch/main/scripts/update-immich-spec.sh) **deletes six operations** from the vendored spec, with this comment:

> ```
> # Operations progenitor cannot generate. None are used by this CLI, so they are
> # dropped rather than worked around.
> #   multipart/form-data request bodies, unsupported by progenitor:
> #     uploadDatabaseBackup, uploadAsset, createProfileImage
> #   $ref-typed query/path parameters, unsupported by progenitor:
> #     emptyQueue, getQueueJobs, searchLargeAssets
> ```

So progenitor also cannot handle `$ref`-typed query/path parameters — a second, independent limitation against this spec. `immich-tools` proves progenitor *works* for the rest of the Immich API, but it proves the upload operation must be excluded and hand-written.

**`paperclip`** ([crates.io](https://crates.io/crates/paperclip), 0.9.7 released 2026-04-20, MIT OR Apache-2.0, 970 stars, 91 open issues, last push 2026-04-20). Its own [`README`](https://github.com/paperclip-rs/paperclip/blob/master/README.md) says: *"It's currently under active development and may not be ready for production use just yet"*, and it is primarily a *server*-side framework derive suite. [`paperclip-ng`](https://crates.io/crates/paperclip-ng) 0.1.3 (2026-04-20) self-describes as *"Experimental OpenAPI V3.0.3 Code Generator"*. **Avoid.**

**`openapi-generator` (rust, `--library reqwest`)** — the same tool Immich uses for Dart. Its [rust generator docs](https://openapi-generator.tech/docs/generators/rust/) list it as generator stability **STABLE**, generator type CLIENT, `helpTxt` *"Generates a Rust client library (beta)"*, with `library` options `reqwest`, `reqwest-trait`, `hyper`, plus `supportMiddleware` (reqwest-middleware) and `supportMultipleResponses`. Its rust/reqwest template **does** handle multipart at the pinned generator version: [`rust/reqwest/api.mustache` at tag `v7.25.0`](https://github.com/OpenAPITools/openapi-generator/blob/v7.25.0/modules/openapi-generator/src/main/resources/rust/reqwest/api.mustache) contains `{{#isMultipart}}` → `reqwest::multipart::Form::new()`, `multipart_form.file(...)` and `Part::stream(reqwest::Body::wrap_stream(stream))` for file params. **However**, I could not run it: it requires a JVM and none is installed in this environment, and `@openapitools/openapi-generator-cli` is only a wrapper around the Java jar. Note also that this generator maps `binary` → `crate::models::File` and `ByteArray` → `String` in [`RustClientCodegen.java`](https://github.com/OpenAPITools/openapi-generator/blob/master/modules/openapi-generator/src/main/java/org/openapitools/codegen/languages/RustClientCodegen.java), which is exactly the kind of type mapping that behaves differently for a `$ref`-typed form schema like `AssetMediaCreateDto`. So: **plausible but unverified**, and it adds a JVM + a ~950 KB vendored spec + a regenerated multi-thousand-line client to maintain for 8 operations.

(Also present: [`openapi-to-rust`](https://crates.io/crates/openapi-to-rust) 0.22.0 / 2026-10-06, 53,395 downloads, a pure-Rust generator with client+server output. Not evaluated for this report — see Uncertainty.)

**Verdict: hand-write the client.** The engine needs 8 operations, all small. `reqwest` + `serde` gives ~300–500 lines with full control over multipart streaming, the `x-immich-checksum` header and the 200-vs-201 duplicate semantics — precisely the parts every generator gets wrong or drops. If the API surface later grows past ~20 operations, the proven pattern is `immich-tools`'s: vendor a pruned spec, generate everything except the multipart upload with progenitor, hand-write the upload.

---

## 3. Component crates

All version/date/license facts below come from the crates.io API on 2026-10-12; repository activity from the GitHub REST API on the same date.

### Filesystem watching

| Crate | Version / released | License | Evidence | Verdict |
|---|---|---|---|---|
| [`notify`](https://crates.io/crates/notify) | **8.2.0 / 2025-08-03** (9.0.0-rc.5 is a pre-release, 2026-08-30) | CC0-1.0 | 169M downloads; repo [notify-rs/notify](https://github.com/notify-rs/notify) pushed 2026-10-12, 3,474 stars; platform table: inotify / FSEvents / **ReadDirectoryChangesW** / kqueue | **Adopt 8.2.0.** Stable line is 14 months old but the repo is active; do not ship the 9.0 RC. |
| [`notify-debouncer-full`](https://crates.io/crates/notify-debouncer-full) | **0.7.0 / 2026-01-23** (0.8.0-rc.2 pending) | MIT OR Apache-2.0 | Depends `notify ^8.2.0` (verified via the crates.io dependencies API); [README](https://github.com/notify-rs/notify/blob/main/notify-debouncer-full/README.md) says it merges renames, *"Optionally keeps track of the file system IDs … (FSEvents, Windows)"* and *"Emits only one `Remove` event when deleting a directory (inotify)"*; MSRV 1.88 | **Adopt.** Rename coalescing and file-ID tracking are exactly what a sync engine needs and what raw `notify` does not give you. |

### Directory scanning

| Crate | Version / released | License | Evidence | Verdict |
|---|---|---|---|---|
| [`walkdir`](https://crates.io/crates/walkdir) | 2.5.0 / 2024-03-01 | Unlicense/MIT | 678M downloads; repo pushed 2026-06-23 (maintenance) | **Consider.** Fine for the reconciliation scan, but `ignore` gives you the same walk plus filtering for one dependency. |
| [`jwalk`](https://crates.io/crates/jwalk) | 0.9.0 / 2026-08-05 | MIT | **Repository is archived** ([Byron/jwalk](https://github.com/Byron/jwalk), `archived: true`); crates.io description now reads *"Use `dua-core` instead"* | **Avoid.** Archived by its author with a migration pointer. |
| [`ignore`](https://crates.io/crates/ignore) | 0.4.33 / 2026-08-04 | Unlicense OR MIT | 188M downloads; part of the actively maintained [ripgrep repo](https://github.com/BurntSushi/ripgrep) (pushed 2026-08-04) | **Adopt.** One crate for the parallel directory walk *plus* hidden-file and gitignore-style exclusion rules, which the app needs anyway (hidden folders, `.DS_Store`, Synology/`@eaDir`-class junk that immich-go also excludes). |
| [`globset`](https://crates.io/crates/globset) | 0.4.20 / 2026-08-04 | Unlicense OR MIT | Same repo | **Consider, probably skip.** Only earns its place if the app exposes raw glob syntax to users; `ignore` already covers `.gitignore`-style matching. |

### Hashing

| Crate | Version / released | License | Evidence | Verdict |
|---|---|---|---|---|
| [`sha1`](https://crates.io/crates/sha1) | **0.11.0 / 2026-03-27**, with `0.10.7` backported 2026-07-10 | MIT OR Apache-2.0 | [RustCrypto/hashes](https://github.com/RustCrypto/hashes) active (pushed 2026-10-09); MSRV 1.85; the [sha1 README](https://github.com/RustCrypto/hashes/blob/master/sha1/README.md) warns: *"The SHA-1 hash function should be considered cryptographically broken… We provide this crate for legacy interoperability purposes only."* | **Adopt.** The warning is the correct context: SHA-1 here is a server-mandated duplicate key, not a security primitive. Note the dual maintenance lines (0.10.7 and 0.11.0) when picking a version for the app's MSRV. |
| [`sha2`](https://crates.io/crates/sha2) | 0.11.0 / 2026-03-25 | MIT OR Apache-2.0 | Same repo | **Consider/skip.** Immich needs SHA-1 only. A second hash in the local DB buys nothing the server can verify. |
| [`hex`](https://crates.io/crates/hex) | 0.4.3 / 2021-03-03 | MIT OR Apache-2.0 | 704M downloads; no release in 5 years but the API is 2 functions | **Consider.** Stable, but for this app prefer base64 (next row). |
| [`base64`](https://crates.io/crates/base64) | 0.23.1 / 2026-08-04 | MIT OR Apache-2.0 | 1.71B downloads, active | **Adopt (or `hex`).** Immich's checksum field accepts *"Base64 or hex encoded SHA1"*; **immich-go sends base64** ([`internal/fshelper/hash/base64.go`](https://github.com/simulot/immich-go/blob/main/internal/fshelper/hash/base64.go), `StdEncoding`), while `@immich/cli` sends hex ([`utils.ts`](https://github.com/immich-app/immich/blob/v3.3.1/packages/cli/src/utils.ts)). Base64 is ~33% shorter in the bulk-check batch. Pick one and freeze it in the DB. If hex is preferred: `const-hex` 1.19.3 (2026-09-16) or `faster-hex` 1.0.0 (2026-09-23) are the fast replacements. |
| [`memmap2`](https://crates.io/crates/memmap2) | 0.9.11 / 2026-06-22 | MIT OR Apache-2.0 | 372M downloads, active | **Avoid for hashing.** The pipeline is one sequential pass per file; a buffered reader is equivalent in throughput and has no page-fault failure modes on files that change or live on network shares mid-read. Keep it out unless a future feature needs random access. |

**On "streaming/multipart hashing":** there is no crate needed, and the Immich API forbids the obvious optimization. `x-immich-checksum` is a **request header**, so the SHA-1 must be known *before* the body is sent; you cannot compute it by tee-ing the upload body and expect the server to dedupe in the same request. Either hash first and then upload (two passes), or rely on `POST /assets/bulk-upload-check` first (immich-go does both: it hashes during its scan and also sets the checksum header, per [`immich/upload.go`](https://github.com/simulot/immich-go/blob/main/immich/upload.go) + [`immich/call.go`](https://github.com/simulot/immich-go/blob/main/immich/call.go)).

### HTTP client

| Crate | Version / released | License | Evidence | Verdict |
|---|---|---|---|---|
| [`reqwest`](https://crates.io/crates/reqwest) | 0.13.5 / 2026-09-08 | MIT OR Apache-2.0 | Feature `multipart` exposes [`Form`/`Part`](https://docs.rs/reqwest/0.13.5/reqwest/multipart/index.html); [`Part::stream_with_length(Body, u64)`](https://docs.rs/reqwest/0.13.5/reqwest/multipart/struct.Part.html) streams a file part with a known length; [`Body::wrap_stream`](https://docs.rs/reqwest/0.13.5/reqwest/struct.Body.html) (feature `stream`) wraps any `TryStream` | **Adopt.** Matches the existing decision. `Part::stream_with_length` is the right primitive for a file part. |
| [`tower`](https://crates.io/crates/tower) | 0.5.3 / 2026-01-12 | MIT | [`tower::retry`](https://docs.rs/tower/0.5.3/tower/retry/index.html) (feature `retry`) provides `Retry`, `Policy`, a `backoff` module and a retry `budget` | **Consider.** Only worth it if the app adopts a tower stack end-to-end. |
| [`tower-http`](https://crates.io/crates/tower-http) | 0.7.1 / 2026-08-31 | MIT | Active (515M downloads) | **Consider.** Client middleware (tracing, timeouts) — useful but not load-bearing. |
| [`reqwest-middleware`](https://crates.io/crates/reqwest-middleware) + [`reqwest-retry`](https://crates.io/crates/reqwest-retry) | 0.5.2 / 2026-05-19 · 0.9.1 / 2026-02-05 | MIT OR Apache-2.0 | TrueLayer repos, 382 stars, pushed 2026-05-19; 47 open issues | **Consider.** This is the cheapest way to get retry/backoff middleware *inside* reqwest without restructuring the client around tower. If the retry policy needs to consult a durable queue (it does — see below), a plain `backon` loop is more transparent. |

### Retry / backoff

| Crate | Version / released | License | Evidence | Verdict |
|---|---|---|---|---|
| [`backon`](https://crates.io/crates/backon) | 1.6.0 / 2025-10-18 | Apache-2.0 | [README](https://github.com/Xuanwo/backon/blob/main/README.md): exponential/constant strategies, *"dynamic backoff, such as using the HTTP `Retry-After` header"*, `.sleep(tokio::time::sleep)`; features include `tokio-sleep`, `std-blocking-sleep`, `gloo-timers-sleep`, `embassy-sleep`; [`ExponentialBuilder`](https://docs.rs/backon/1.6.0/backon/struct.ExponentialBuilder.html) has `with_jitter`; repo pushed 2026-06-19 | **Adopt.** Jitter + `Retry-After` support + pluggable sleep is exactly what "retry classification with backoff" needs, with no framework. |
| [`tokio-retry`](https://crates.io/crates/tokio-retry) | 0.3.2 / 2026-06-09 | MIT | Revived after a 2021 gap; 144 stars; simpler `Action`-based API | **Consider.** Fine alternative, less featureful than `backon`. |
| [`reqwest-retry`](https://crates.io/crates/reqwest-retry) | 0.9.1 / 2026-02-05 | MIT OR Apache-2.0 | Middleware, couples policy to the client | **Consider** (see above). |
| [`tower-retry`](https://crates.io/crates/tower-retry) | **0.3.0 / 2019-12-19** | MIT | Abandoned; functionality lives in `tower::retry` now | **Avoid.** Dead package. |

The shape to copy from rclone: it separates **low-level retries** (a single failing HTTP request, e.g. one chunk) from **high-level retries** (re-running the whole transfer pass), with `--low-level-retries`, `--retries` (default 3) and `--retries-sleep` (default 0) documented in [`docs/content/docs.md`](https://github.com/rclone/rclone/blob/master/docs/content/docs.md). For an upload-only engine this maps to: retry the HTTP request N times with backoff *inside* the worker, and let a failed item fall back to the durable queue as a separate, longer-horizon retry — not one merged counter.

### Rate limiting and concurrency

| Crate | Version / released | License | Evidence | Verdict |
|---|---|---|---|---|
| [`governor`](https://crates.io/crates/governor) | 0.10.4 / 2025-12-16 | MIT | [README](https://github.com/boinkor-net/governor/blob/master/governor/README.md): *"an implementation of the Generic Cell Rate Algorithm for rate limiting in Rust programs"*, GCRA ≡ leaky bucket for **requests/events**, state in one `AtomicU64`, direct + keyed limiters | **Consider.** GCRA counts cells, and the limiter has a weighted API (`check_n` / `until_n_ready(n)`), so passing n = byte count expresses a byte/sec budget directly. The hand-rolled stream bucket remains the primary recommendation because it couples the budget to the body stream, but governor is a valid option for the same job. |
| Byte-rate limiting alternatives | [`async-speed-limit`](https://crates.io/crates/async-speed-limit) 0.4.2 / 2024-10-05 (MIT OR Apache-2.0, tokio-adjacent, 592k downloads) is *"Asynchronously speed-limiting multiple byte streams"*; `leaky-bucket` 1.1.2 / 2024-05-22 | — | Both quiet but small and stable | **Consider.** Either gives a real bytes/second throttle on the upload body stream; a hand-rolled token bucket over the multipart stream is also viable and is ~50 lines. |
| [`tower_governor`](https://crates.io/crates/tower_governor) | 0.8.0 / 2025-08-14 | MIT OR Apache-2.0 | `tower` middleware that rate-limits **incoming** requests, keyed (e.g. by peer IP) | **Avoid.** Server-side middleware; there is a tray app, not a server. |
| `tokio::sync::Semaphore` ([tokio](https://crates.io/crates/tokio) 1.53.2 / 2026-10-03) | — | MIT | Standard library of the async runtime already in use | **Adopt.** Upload concurrency cap. rclone's equivalent knob is `--transfers` (default 4) and `--checkers` (default 8) — separate budgets for transfer vs metadata-check work, which is a good default shape for this engine too. |

### Durable job queue / worker framework

| Candidate | Version / released | License | Evidence | Verdict |
|---|---|---|---|---|
| [`apalis`](https://crates.io/crates/apalis) + [`apalis-sqlite`](https://crates.io/crates/apalis-sqlite) | apalis 0.7.4 stable / 1.0.0-rc.12 (2026-10-09); **apalis-sqlite 1.0.0-rc.11 / 2026-10-07** | MIT | [apalis-sqlite Cargo.toml](https://github.com/apalis-dev/apalis-sqlite/blob/main/Cargo.toml) depends on **`sqlx 0.9` with `features=["sqlite"]`**; [README](https://github.com/apalis-dev/apalis-sqlite/blob/main/README.md) advertises durable execution, `max_attempts`, backoff re-enqueue, heartbeat/orphan recovery, scheduled jobs, priorities | **Avoid (for now).** It *does* support SQLite — via `sqlx`, i.e. a second SQLite driver alongside the chosen `rusqlite`, plus the sqlx dependency tree and its own migrations. It is also still a release candidate. |
| [`sqlxmq`](https://crates.io/crates/sqlxmq) | 0.6.0 / 2025-05-25 | MIT OR Apache-2.0 | [README](https://github.com/Diggsey/sqlxmq/blob/master/README.md): *"A job queue built on `sqlx` and **PostgreSQL**"*, *"The only runtime dependency is `PostgreSQL`"* | **Avoid.** Wrong database. |
| [`background-jobs`](https://crates.io/crates/background-jobs) | 0.20.0 / 2026-01-13 | **AGPL-3.0** | Backends are `background-jobs-sled` and `background-jobs-postgres` (both 0.20.0) — **no SQLite backend** | **Avoid.** Wrong backend *and* a strong-copyleft license for a dependency of a desktop app. |
| Queue-as-a-SQLite-table + worker loop | — | — | The app already owns a `rusqlite` (bundled, WAL) store | **Adopt.** For this engine the "queue" is not a generic job table: an upload item's state *is* the file/version record's state (pending → hashing → checked → uploading → uploaded/dead-letter), and transitions must be transactional with the version-chain rows. A framework's separate job table would duplicate or shadow that state. |

Evaluation detail supporting that last point: neither prior-art uploader has a durable queue at all. `@immich/cli` uses a purely in-memory [`fastq`-backed `Queue` class](https://github.com/immich-app/immich/blob/v3.3.1/packages/cli/src/queue.ts) with `{concurrency, retry}` and no persistence; immich-go's tracking is an in-memory `map[string]*AssetRecord` ([`internal/assettracker/tracker.go`](https://github.com/simulot/immich-go/blob/main/internal/assettracker/tracker.go)) with no persistence mechanism in the file. Both therefore re-hash and re-check everything on every run. The tray app's requirement is the opposite (survive restarts, dead letter, version chains), which is precisely the part where no crate helps.

### State machines

| Candidate | Version / released | License | Evidence | Verdict |
|---|---|---|---|---|
| Plain Rust enums (`enum UploadState`) + `match` | — | — | — | **Adopt.** The engine's states are I/O-bound and each transition is a DB write plus a network call; the persisted state is a `status` column, so a compile-time state machine buys little and adds a layer between the queue table and the code. |
| [`statig`](https://crates.io/crates/statig) | 0.4.1 / 2025-07-05 | MIT | [README](https://github.com/mdeloof/statig/blob/main/README.md): hierarchical state machines, state-local storage, `no_std`, *"Support for async actions and handlers"*; 4.6M downloads | **Consider.** The best-in-class option if the retry/version logic ever grows nested sub-states (e.g. per-item retry policy inside per-file upload). Not needed for v1. |
| [`rust-fsm`](https://crates.io/crates/rust-fsm) | 0.8.0 / 2025-07-21 | MIT | Maintained, DSL-based | **Consider** (same conclusion). |
| [`sm`](https://crates.io/crates/sm) | 0.9.0 / **2019-11-09** | MIT OR Apache-2.0 | No release in 7 years | **Avoid.** |
| [`machine`](https://crates.io/crates/machine) | 0.3.0 / **2019-06-14** | MIT/Apache-2.0 | No release in 7 years, 39k downloads | **Avoid.** |

### File identity and timing

| Crate | Version / released | License | Evidence | Verdict |
|---|---|---|---|---|
| [`same-file`](https://crates.io/crates/same-file) | 1.0.6 / **2020-01-11** | Unlicense/MIT | 657M downloads; repo [BurntSushi/same-file](https://github.com/BurntSushi/same-file) pushed 2025-10-17 (124 stars); used by `walkdir`/`ignore` | **Consider.** No release in 6 years, but it is a two-function stability crate with a live upstream. Only needed if the engine must resolve hardlink/symlink aliasing (e.g. two watched folders containing the same file). |
| [`filetime`](https://crates.io/crates/filetime) | 0.2.29 / 2026-05-12 | MIT/Apache-2.0 | 408M downloads, repo pushed 2026-05-12 | **Consider.** Reading mtime/ctime is already in `std::fs::Metadata`; this is only needed to *set* timestamps (round-trip fidelity), which v1 does not require. |

**Identity for change detection:** the practical key is `(device, inode)` on Unix and the file index (`volume serial + file ID`) on Windows, which is what `notify-debouncer-full` tracks internally and what `same-file` exposes. Path alone is not sufficient when a file is replaced by rename or the same content appears at two paths. Storing size + mtime alongside a content hash in the SQLite row is the cheap 90% solution; the (device, inode)/file-ID pair is what makes rename detection correct on Windows.

**Where a candidate conflicts with earlier decisions:** only `apalis-sqlite` (adds `sqlx` next to `rusqlite`). Everything else above is compatible with rusqlite + WAL, tokio and reqwest. Note that `governor` does *not* conflict; its weighted-cell API can express bytes/sec, so it is a valid bandwidth-limiter option rather than the wrong tool.

---

## 4. Prior art: architecture lessons

Sources: [`@immich/cli` at v3.3.1](https://github.com/immich-app/immich/tree/v3.3.1/packages/cli/src) (TypeScript, in-tree, official), [immich-go `main`](https://github.com/simulot/immich-go) (Go, AGPL-3.0, 7,019 stars, last push 2026-06-25), and rclone `master` docs. No code was copied; these are behavior observations.

### Resumable/chunked uploads

- **The Immich API forbids them.** The v3.3.1 spec has no upload-session, offset, part or `Content-Range` concept anywhere among its 193 paths; `POST /assets` is a single multipart request. So a dropped upload must be **restarted from byte 0**, which is why the engine's retry classification must treat "connection dropped mid-file" as a normal, cheap-to-repeat event rather than a resume opportunity.
- immich-go accepts this by streaming the multipart body through an `io.Pipe` while writing the file part ([`immich/upload.go`](https://github.com/simulot/immich-go/blob/main/immich/upload.go)) — constant memory, no temp files, restart on failure. `@immich/cli` instead subclasses `File` to override `stream()` with `createReadStream(filepath)` and hands it to `FormData` ([`commands/asset.ts`](https://github.com/immich-app/immich/blob/v3.3.1/packages/cli/src/commands/asset.ts)) — same idea in Node.
- rclone *does* resume/chunk where backends allow it (its `chunker` overlay splits large files; `--multi-thread-streams` controls per-file parallelism [docs](https://github.com/rclone/rclone/blob/master/docs/content/docs.md)), but that machinery is backend-protocol-specific and inapplicable here.
- **Lesson:** stream the file once from disk, never buffer it; make the unit of retry the whole `POST /assets`; keep the size of each item small enough that restart cost is acceptable.

### Queueing

- `@immich/cli`: a **single in-memory queue** with a `concurrency` option, drained before exit; tasks are reported as failed at the end and otherwise forgotten. The queue class itself carries TODOs for progress timing and per-task durations ([`queue.ts`](https://github.com/immich-app/immich/blob/v3.3.1/packages/cli/src/queue.ts)). No persistence.
- immich-go: a fixed goroutine pool with an **unbuffered** task channel ([`internal/worker/worker.go`](https://github.com/simulot/immich-go/blob/main/internal/worker/worker.go)) and an in-memory asset tracker; no persistence.
- **Lesson (the gap this app must fill):** prior art is batch-importer shaped — it assumes one process lifetime. A tray app that survives reboots and sleep cycles needs the queue in SQLite, with a lease/heartbeat so a crashed worker's "in-flight" row is re-claimable. Nothing in the ecosystem supplies that for this combination of requirements, which is the strongest argument for the hand-rolled table + worker loop.
- rclone's shape is still worth stealing for the *budgets*: separate `--transfers` (4) and `--checkers` (8) pools, so metadata/dedup work cannot starve or be starved by byte-moving work.

### Retry classification

- `@immich/cli` passes `{concurrency, retry: 3}` and its `work()` simply re-runs the task up to `retry` times with **no delay, no backoff, no jitter, no classification** ([`queue.ts`](https://github.com/immich-app/immich/blob/v3.3.1/packages/cli/src/queue.ts)).
- immich-go declares `Retries int // Number of attempts on 500 errors` and `RetriesDelay time.Duration`, defaults them to `1` and `1s` ([`immich/client.go`](https://github.com/simulot/immich-go/blob/main/immich/client.go)) — and **never uses either field**: a full-text scan of its `immich/` and `app/` Go sources finds only the declaration and the defaults, and its `serverCall.do()` returns on the first non-2xx after decoding the error body ([`immich/call.go`](https://github.com/simulot/immich-go/blob/main/immich/call.go)). So neither prior-art uploader has a real retry policy.
- One genuinely valuable, non-obvious handling exists in immich-go: when the `x-immich-checksum` header reveals a duplicate, **Immich closes the connection mid-upload**, and immich-go treats `io.ErrClosedPipe` as success when the response status is `duplicate` ([`immich/upload.go`](https://github.com/simulot/immich-go/blob/main/immich/upload.go)). Any Rust client must handle "server closed the connection because it already has this file" as a *success*, not a transport error.
- rclone's documented two-tier model (`--low-level-retries` per HTTP request vs `--retries` for the whole transfer pass, with `--retries-sleep`) is the right structure to copy, and it is free to implement with `backon` + `Retry-After` handling.
- **Lesson:** classify by *reason*, not by "did it error": duplicate (success), 4xx semantic rejection (dead letter — `unsupported-format` will never succeed on retry), 401/token expiration (re-authenticate once and retry), 429/503 with `Retry-After` (queue delay), 5xx/transport (backoff), disk/IO error on the source file (dead letter).

### Hashing

- Both prior-art clients compute SHA-1 as a **separate streaming pass over the file**, then use it for the dedup check: `@immich/cli` streams `createReadStream` through `createHash('sha1')` ([`utils.ts`](https://github.com/immich-app/immich/blob/v3.3.1/packages/cli/src/utils.ts)); immich-go has `FileSHA1Hash` over an `fs.FS` ([`internal/fshelper/hash/sha1.go`](https://github.com/simulot/immich-go/blob/main/internal/fshelper/hash/sha1.go)).
- Encoding differs: **immich-go base64-encodes, `@immich/cli` sends hex** — both are accepted by the API's *"Base64 or hex encoded SHA1 hash"* field, so the wire format is a free choice.
- immich-go avoids re-reading by wrapping sources in a `CacheReader` that spools non-file readers to a temp file so the same bytes can be read twice (hash, then upload), hashing on the fly ([`internal/fshelper/cachereader/cachereader.go`](https://github.com/simulot/immich-go/blob/main/internal/fshelper/cachereader/cachereader.go)).
- **Lesson:** hash in the scan phase and store it in SQLite; that turns "hash every file every run" into "hash when (size, mtime) changed". immich-go's temp-file spooling is an artifact of streaming archives and is unnecessary for a folder watcher, where the file is on disk twice.

### Conflict / duplicate detection

- immich-go has the richest model: an explicit decision enum in [`app/upload/advice.go`](https://github.com/simulot/immich-go/blob/main/app/upload/advice.go) — `NotOnServer`, `SameOnServer`, `BetterOnServer`, `SmallerOnServer`, `AlreadyProcessed` (hash already seen in this run), `ForceUpload` — consumed by `handleAsset` in [`app/upload/run.go`](https://github.com/simulot/immich-go/blob/main/app/upload/run.go). Note that "which copy wins" is a *policy* function (it uses EXIF/file size judgment) sitting on top of an API that only reports duplicate-of/accept.
- For a changed file it uploads a new asset, then `CopyAsset(oldID, newID)` (metadata/albums/`PUT /assets/copy`), then deletes the old one ([`run.go` ~L490-522](https://github.com/simulot/immich-go/blob/main/app/upload/run.go)). With `replaceAsset` gone from the v3.3.1 spec, this is now the documented route to a "version chain".
- `@immich/cli` is simpler: batch up to **5,000** checksums into one `checkBulkUpload` request, and treat every `reject` as a duplicate, correlating results back via the echoed client `id` (the file path) ([`commands/asset.ts`](https://github.com/immich-app/immich/blob/v3.3.1/packages/cli/src/commands/asset.ts)). Batching is what makes a first-run scan of 100k files tractable.
- rclone's contribution is the **safety rail, not the mechanism**: `bisync --max-delete 50%` aborts a run that would delete more than half of the files, plus lock files and `--resilient`/`--recover` for interrupted runs ([bisync docs](https://github.com/rclone/rclone/blob/master/docs/content/bisync.md)). For an upload-only engine the analogous rule is a cap on server-side deletions or trashed-asset counts before the deletion pass executes.
- **Lesson:** the API answers "do you have this SHA-1?" and nothing else — no ETag, no per-file version, no remote listing of *unmatched* local files. Duplicate/version/deletion policy must live entirely in the app's own SQLite state, reconciled against `POST /assets/bulk-upload-check` batches.

### Progress reporting

- `@immich/cli` uses `cli-progress` `MultiBar` bars keyed on **bytes**, with separate bars for hashing vs duplicate-checking vs uploading, and an `etaBuffer` of 100 samples ([`commands/asset.ts`](https://github.com/immich-app/immich/blob/v3.3.1/packages/cli/src/commands/asset.ts)).
- immich-go renders a TUI and, in `--no-ui` mode, prints a one-line counter from a **500 ms ticker** ([`app/upload/noui.go`](https://github.com/simulot/immich-go/blob/main/app/upload/noui.go)); its counters come from a central event logger with a server-error count surfaced alongside the upload count.
- **Lesson:** report bytes (hashing and uploading progress) plus counts of terminal states including errors, and throttle rendering to ~2 Hz. For a tray app this becomes "notify only on state change, plus a periodic tick".

---

## 5. Recommendation

**Build `sync-core` from scratch on the crates below. Do not embed a sync engine, and do not generate a full Immich client for v1.**

### Build vs adopt

| Component | Adopt | Consider | Avoid | Why |
|---|---|---|---|---|
| Folder watching | `notify` 8.2.0 + `notify-debouncer-full` 0.7.0 | `notify` 9.0 RC | raw `notify` alone | Debouncer gives rename coalescing + file-ID tracking; 9.0 is a pre-release and the debouncer still pins notify 8.2.0 |
| Reconciliation scan | `ignore` 0.4.33 | `walkdir` 2.5.0, `globset` 0.4.20 | `jwalk` (archived) | `ignore` = parallel walk + hidden/ignore rules in one crate |
| Hashing | `sha1` (0.11.0 or 0.10.7) + `base64` 0.23.1 | `hex` 0.4.3, `const-hex` | `memmap2` | SHA-1 is server-mandated; hash in the scan phase, cache in SQLite |
| HTTP | `reqwest` 0.13.5 (`multipart`, `stream`) | `reqwest-middleware` + `reqwest-retry` | hand-rolled HTTP | `Part::stream_with_length` streams the file part with a known length |
| Retry/backoff | `backon` 1.6.0 (jitter + `Retry-After`) | `tokio-retry` 0.3.2, `reqwest-retry` 0.9.1 | `tower-retry` (dead) | Two-tier: in-worker HTTP retry vs queue-level dead letter |
| Concurrency | `tokio::sync::Semaphore` | `tower` `limit` | — | Separate transfer and check budgets, rclone-style |
| Request pacing | — | `governor` 0.10.4 for requests/sec caps, or weighted cells for bytes/sec | `tower_governor` | GCRA counts cells; pass n = byte count to `check_n` / `until_n_ready` to shape bytes/sec |
| Bandwidth limiting | hand-rolled byte token bucket over the body stream | `async-speed-limit` 0.4.2, `governor` 0.10.4 with weighted cells | — | Needs bytes/sec, measured as the body is streamed |
| Durable queue | hand-rolled SQLite table + worker loop (`rusqlite` 0.40.2, WAL) | `apalis-sqlite` (RC + sqlx conflict) | `sqlxmq` (Postgres), `background-jobs` (AGPL, no SQLite) | The queue state *is* the file/version state and must be transactional with it |
| State machine | plain enum + `match` | `statig` 0.4.1 | `sm`, `machine` (2019) | Transitions are I/O + DB writes; persistence is a `status` column |
| File identity/timing | `std::fs::Metadata` + size/mtime | `same-file` 1.0.6, `filetime` 0.2.29 | — | Only add `same-file` if hardlink/symlink aliasing matters |
| Immich client | ~8 hand-written `reqwest` + `serde` operations | progenitor on a pruned spec for non-upload ops, if the surface grows | progenitor over the full spec (fails), paperclip ("not ready for production"), stale Immich crates | `progenitor` provably cannot generate `POST /assets` |

### Where prior art or a crate saves real work

1. **Watching, debouncing and rename coalescing** — `notify` + `notify-debouncer-full`. Getting Windows `ReadDirectoryChangesW` semantics (double events, renames arriving as remove+create, directories reported before their contents settle) right by hand is weeks of work.
2. **The directory walk with exclusion rules** — `ignore` already implements the hidden-file and junk-pattern rules that both prior-art tools needed, including the Synology/macOS `.DS_Store` class of excludes.
3. **Streaming multipart upload** — `reqwest` `Part::stream_with_length`; the multipart framing, boundary handling and `Content-Length` computation are solved.
4. **Backoff with jitter and `Retry-After`** — `backon`.
5. **Checksum batching strategy** — copy `@immich/cli`: hash concurrently, batch up to ~5,000 `{id, checksum}` items per `bulk-upload-check`, correlate results by echoed id.
6. **The duplicate-mid-upload quirk** — copy immich-go: a connection abort with a `duplicate` 200 is a success.
7. **Deletion safety rails** — copy rclone's `--max-delete` idea as a configurable cap on the number of server-side deletions per run.

### Genuinely this app's own logic (no crate will help)

- **The persistable upload state machine** — queued → hashing → checked → uploading → uploaded / retry-with-backoff / dead-letter, with crash recovery (leases) and per-reason classification.
- **Immich semantics** — the 200-vs-201 duplicate response, `x-immich-checksum` short-circuit, `bulk-upload-check` batching and `unsupported-format` rejection, token validation and re-auth.
- **Version chains** — upload-new + `PUT /assets/copy` (+ `POST /stacks`) + optional delete, all recorded in the local DB, now that `replaceAsset` is unavailable.
- **Deletion detection and review** — neither prior-art tool does this at all; `@immich/cli`'s watcher listens only to `add` and `change` ([`commands/asset.ts`](https://github.com/immich-app/immich/blob/v3.3.1/packages/cli/src/commands/asset.ts)), immich-go is a one-shot importer. Local deletion (including renames, which arrive as delete+create) has to be distinguished from "unmounted drive / moved folder", and the user must be able to review before anything is trashed on the server.

---

## Uncertainty

- **`PUT /assets/{id}/original` removal.** I verified that the path has `get` + `put` (`replaceAsset`) in the `v1.137.3` spec and only `get` in the `v3.3.1` spec, and that no `v3.3.1` operationId contains "replace". I did **not** read the Immich server source to confirm the route was deleted rather than merely undocumented, and I did not bisect the release in which it changed. Before relying on this, grep the Immich server controllers.
- **openapi-generator was not executed.** No JVM exists in this environment, and the npm CLI is only a wrapper for the Java jar. Its multipart support is inferred from the `v7.25.0` tag's `rust/reqwest/api.mustache` template and from Immich generating its own Dart SDK with generator 7.25.0 — not from a successful Rust run. Its `binary` → `models::File` / `ByteArray` → `String` mappings are a specific risk for the `$ref`-typed `AssetMediaCreateDto` form schema.
- **`openapi-to-rust` 0.22.0 was not evaluated** at all; it is listed only as an alternative that exists.
- **progenitor's `$ref`-typed query/path parameter limitation** is taken from `immich-tools`'s script comment, not from my own run (my run failed earlier, on multipart).
- **Crates.io `updated_at` vs release date.** Where a crate has pre-releases, `updated_at` reflects the newest version of any kind; the tables above use the newest *stable* version's `created_at`, verified per crate.
- **`sha1` 0.10.7 vs 0.11.0.** Both are current and maintained in parallel. The choice depends on the app's MSRV and on which `digest` version the rest of the tree uses; the crates.io metadata alone does not settle it.
- **`memmap2` avoidance is a judgment**, not a measured benchmark on this app's file mix (large photos/videos, possibly on network volumes).
- **`async-speed-limit` (2024) and `leaky-bucket` (2024)** are quiet projects; the hand-rolled alternative is small enough that "quiet dependency" cuts against adopting them.
- **Download counts and star counts** are weak proxies for maintenance and are reported as context only; the stronger signals used were release recency, archived flags, repository push dates and self-describing README statements.
- **No crate was compiled or benchmarked**, apart from installing and running `cargo-progenitor` 0.15.1.

---

## Sources

Immich API and SDK generation:
- [Immich OpenAPI spec, v3.3.1](https://raw.githubusercontent.com/immich-app/immich/v3.3.1/open-api/immich-openapi-specs.json)
- [Immich OpenAPI spec, v1.137.3](https://raw.githubusercontent.com/immich-app/immich/v1.137.3/open-api/immich-openapi-specs.json)
- [Immich docs `docs/docs/api.md` @ v3.3.1](https://github.com/immich-app/immich/blob/v3.3.1/docs/docs/api.md)
- [Immich `mise.toml` @ v3.3.1 (oazapfts + openapi-generator tasks)](https://github.com/immich-app/immich/blob/v3.3.1/mise.toml)
- [`packages/sdk/package.json` @ v3.3.1](https://github.com/immich-app/immich/blob/v3.3.1/packages/sdk/package.json)
- [`open-api/openapitools.json` @ v3.3.1](https://github.com/immich-app/immich/blob/v3.3.1/open-api/openapitools.json)
- [`open-api/bin/generate-dart-sdk.sh` @ v3.3.1](https://github.com/immich-app/immich/blob/v3.3.1/open-api/bin/generate-dart-sdk.sh)
- [Immich release v1.137.3](https://api.github.com/repos/immich-app/immich/releases/tags/v1.137.3) · [latest release](https://api.github.com/repos/immich-app/immich/releases?per_page=1)

Prior art:
- [`@immich/cli` `queue.ts` @ v3.3.1](https://github.com/immich-app/immich/blob/v3.3.1/packages/cli/src/queue.ts)
- [`@immich/cli` `commands/asset.ts` @ v3.3.1](https://github.com/immich-app/immich/blob/v3.3.1/packages/cli/src/commands/asset.ts)
- [`@immich/cli` `utils.ts` @ v3.3.1](https://github.com/immich-app/immich/blob/v3.3.1/packages/cli/src/utils.ts)
- [immich-go `immich/upload.go`](https://github.com/simulot/immich-go/blob/main/immich/upload.go) · [`immich/call.go`](https://github.com/simulot/immich-go/blob/main/immich/call.go) · [`immich/client.go`](https://github.com/simulot/immich-go/blob/main/immich/client.go)
- [immich-go `app/upload/run.go`](https://github.com/simulot/immich-go/blob/main/app/upload/run.go) · [`app/upload/noui.go`](https://github.com/simulot/immich-go/blob/main/app/upload/noui.go) · [`app/upload/advice.go`](https://github.com/simulot/immich-go/blob/main/app/upload/advice.go)
- [immich-go `internal/worker/worker.go`](https://github.com/simulot/immich-go/blob/main/internal/worker/worker.go) · [`internal/assettracker/tracker.go`](https://github.com/simulot/immich-go/blob/main/internal/assettracker/tracker.go) · [`internal/fshelper/cachereader/cachereader.go`](https://github.com/simulot/immich-go/blob/main/internal/fshelper/cachereader/cachereader.go)
- [immich-go `internal/fshelper/hash/sha1.go`](https://github.com/simulot/immich-go/blob/main/internal/fshelper/hash/sha1.go) · [`.../hash/base64.go`](https://github.com/simulot/immich-go/blob/main/internal/fshelper/hash/base64.go)
- [rclone storage-systems overview (backend A–Z)](https://rclone.org/overview/)
- [rclone `docs/content/http.md` (read-only HTTP remote)](https://github.com/rclone/rclone/blob/master/docs/content/http.md)
- [rclone `docs/content/docs.md` (--transfers, --checkers, --retries, --low-level-retries, --bwlimit)](https://github.com/rclone/rclone/blob/master/docs/content/docs.md)
- [rclone `docs/content/bisync.md` (--max-delete, --resilient, locks)](https://github.com/rclone/rclone/blob/master/docs/content/bisync.md)

Whole-engine candidates:
- [rustic_core](https://crates.io/crates/rustic_core) · [rustic-rs/rustic_core](https://github.com/rustic-rs/rustic_core)
- [syncthing](https://crates.io/crates/syncthing) · [jaycefayne/syncthing-rs](https://github.com/jaycefayne/syncthing-rs)
- [copia](https://crates.io/crates/copia) · [duet](https://crates.io/crates/duet) · [ensync](https://crates.io/crates/ensync) · [ubiquity](https://crates.io/crates/ubiquity)
- [restic](https://crates.io/crates/restic) · [cloud-storage-sync](https://crates.io/crates/cloud-storage-sync)
- [librclone](https://crates.io/crates/librclone) · [trevyn/librclone](https://github.com/trevyn/librclone) · [rclone-sdk](https://crates.io/crates/rclone-sdk)

Immich clients and generators:
- [immich_api_rs](https://crates.io/crates/immich_api_rs) · [immich](https://crates.io/crates/immich) · [anergictcell/immich](https://github.com/anergictcell/immich) · [immich-lib](https://crates.io/crates/immich-lib) · [immich-analyze](https://crates.io/crates/immich-analyze)
- [immich-tools](https://crates.io/crates/immich-tools) · [`build.rs`](https://git.enoent.fr/kernald/immich-tools/src/branch/main/build.rs) · [`scripts/update-immich-spec.sh`](https://git.enoent.fr/kernald/immich-tools/src/branch/main/scripts/update-immich-spec.sh)
- [progenitor](https://crates.io/crates/progenitor) · [`progenitor-impl/src/method.rs`](https://github.com/oxidecomputer/progenitor/blob/main/progenitor-impl/src/method.rs)
- [paperclip](https://crates.io/crates/paperclip) · [paperclip README](https://github.com/paperclip-rs/paperclip/blob/master/README.md) · [paperclip-ng](https://crates.io/crates/paperclip-ng)
- [openapi-generator rust generator docs](https://openapi-generator.tech/docs/generators/rust/) · [`rust/reqwest/api.mustache` @ v7.25.0](https://github.com/OpenAPITools/openapi-generator/blob/v7.25.0/modules/openapi-generator/src/main/resources/rust/reqwest/api.mustache) · [`RustClientCodegen.java`](https://github.com/OpenAPITools/openapi-generator/blob/master/modules/openapi-generator/src/main/java/org/openapitools/codegen/languages/RustClientCodegen.java) · [openapi-to-rust](https://crates.io/crates/openapi-to-rust)

Component crates:
- [notify](https://crates.io/crates/notify) · [notify-rs/notify README](https://github.com/notify-rs/notify/blob/main/README.md) · [notify-debouncer-full](https://crates.io/crates/notify-debouncer-full) · [debouncer README](https://github.com/notify-rs/notify/blob/main/notify-debouncer-full/README.md)
- [walkdir](https://crates.io/crates/walkdir) · [jwalk](https://crates.io/crates/jwalk) · [Byron/jwalk (archived)](https://github.com/Byron/jwalk) · [ignore](https://crates.io/crates/ignore) · [globset](https://crates.io/crates/globset)
- [sha1](https://crates.io/crates/sha1) · [sha1 README (cryptographically broken warning)](https://github.com/RustCrypto/hashes/blob/master/sha1/README.md) · [sha2](https://crates.io/crates/sha2) · [hex](https://crates.io/crates/hex) · [base64](https://crates.io/crates/base64) · [const-hex](https://crates.io/crates/const-hex) · [faster-hex](https://crates.io/crates/faster-hex) · [memmap2](https://crates.io/crates/memmap2)
- [reqwest](https://crates.io/crates/reqwest) · [reqwest::multipart](https://docs.rs/reqwest/0.13.5/reqwest/multipart/index.html) · [Part::stream_with_length](https://docs.rs/reqwest/0.13.5/reqwest/multipart/struct.Part.html) · [Body::wrap_stream](https://docs.rs/reqwest/0.13.5/reqwest/struct.Body.html)
- [tower](https://crates.io/crates/tower) · [tower::retry](https://docs.rs/tower/0.5.3/tower/retry/index.html) · [tower-http](https://crates.io/crates/tower-http) · [reqwest-middleware](https://crates.io/crates/reqwest-middleware) · [reqwest-retry](https://crates.io/crates/reqwest-retry) · [tower-retry (dead)](https://crates.io/crates/tower-retry)
- [backon](https://crates.io/crates/backon) · [backon README](https://github.com/Xuanwo/backon/blob/main/README.md) · [ExponentialBuilder](https://docs.rs/backon/1.6.0/backon/struct.ExponentialBuilder.html) · [tokio-retry](https://crates.io/crates/tokio-retry)
- [governor](https://crates.io/crates/governor) · [governor README](https://github.com/boinkor-net/governor/blob/master/governor/README.md) · [tower_governor](https://crates.io/crates/tower_governor) · [async-speed-limit](https://crates.io/crates/async-speed-limit) · [leaky-bucket](https://crates.io/crates/leaky-bucket) · [tokio](https://crates.io/crates/tokio)
- [apalis](https://crates.io/crates/apalis) · [apalis-sqlite](https://crates.io/crates/apalis-sqlite) · [apalis-sqlite Cargo.toml](https://github.com/apalis-dev/apalis-sqlite/blob/main/Cargo.toml) · [apalis-sqlite README](https://github.com/apalis-dev/apalis-sqlite/blob/main/README.md) · [sqlxmq](https://crates.io/crates/sqlxmq) · [sqlxmq README](https://github.com/Diggsey/sqlxmq/blob/master/README.md) · [background-jobs](https://crates.io/crates/background-jobs)
- [statig](https://crates.io/crates/statig) · [statig README](https://github.com/mdeloof/statig/blob/main/README.md) · [rust-fsm](https://crates.io/crates/rust-fsm) · [sm](https://crates.io/crates/sm) · [machine](https://crates.io/crates/machine)
- [same-file](https://crates.io/crates/same-file) · [filetime](https://crates.io/crates/filetime) · [rusqlite](https://crates.io/crates/rusqlite)
