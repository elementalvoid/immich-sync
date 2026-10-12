# Immich Desktop Folder-Sync Client — Stack Research (Rust+GPUI-Kit vs Go+Wails 3)

**Date of research:** 2026-10-12
**Scope:** Feasibility of a Windows-first desktop "Google-Drive-like" folder-sync client for Immich. Evaluates Immich's HTTP API surface as it actually exists today, sync-engine architecture options, and a head-to-head between (A) Rust + GPUI-Kit and (B) Go + Wails 3 (beta).

**Method note.** Every version number, date and API claim below was read from a primary source during this pass: Immich's published OpenAPI spec (both `main` and the `v3.3.1` tag), Immich server TypeScript source at tag `v3.3.1` (sparse checkout), Immich docs markdown from the `immich-app/immich` repo, GitHub release/tag APIs, crates.io API, pkg.go.dev, and the vendors' own documentation sites. Where a fact could not be verified from a primary source it is explicitly labelled **unverified**. Where a conclusion is my inference from primary source code rather than a documented statement, it is labelled **inferred from source**. Nothing here is copied from another sync client's implementation.

**Versions observed at time of writing:** Immich `v3.3.1` (released 2026-10-08); GPUI-Kit `0.7.1` (released 2026-10-05); Wails `v3.0.0-beta.28` (released 2026-10-05).

**Revision note (2026-10-12, after review).** The first revision compared *GPUI-Kit alone* against *Wails v3*. That is not a like-for-like comparison: Wails is an application framework (tray, single-instance, installers, signing bundled), while GPUI-Kit is a component/UI toolkit that expects the app to assemble those pieces from the Rust ecosystem. §3.4 and §5 were reframed to compare **assembled stacks**. Corrections made: the tokio/reqwest bridge *is* documented by GPUI Kit (a `LazyLock<tokio::runtime::Runtime>` worker runtime plus `cx.set_http_client`); Rust has packaging tooling (`cargo-packager` generates NSIS + WiX MSI, `cargo-wix` MSI); pre-release pinning is symmetric between the stacks; and GPUI core is production on Windows via Zed. The earlier "decisive" and "weeks of work" language was not supported by these facts.

---

## 1. Immich API reality check

### 1.1 Authentication

Immich exposes four authentication mechanisms. The API's `securitySchemes` are declared as `bearer` (`Bearer`, `bearerFormat: JWT`), `cookie` (`immich_access_token`) and `api_key` (`x-api-key` header) — see the v3.3.1 OpenAPI [`components.securitySchemes`](https://raw.githubusercontent.com/immich-app/immich/v3.3.1/open-api/immich-openapi-specs.json).

**Username/password login is viable and marked Stable.**

- `POST /auth/login` — "Login with username and password and receive a session token." Request `LoginCredentialDto`, response `LoginResponseDto` (`201`). See [`POST /auth/login`](https://api.immich.app/endpoints/authentication/login) and the spec's `/auth/login` node.
- The "username" is an **email address**: `LoginCredentialDto` requires `email` (with an `email` format pattern) and `password`. There is no separate username field.
- Response contains `accessToken`, `userId`, `userEmail`, `name`, `isAdmin`, `isOnboarded`, `profileImagePath`, `shouldChangePassword` — all required.
- The spec records stability per endpoint via `x-immich-history`, e.g. `/auth/login` is `Added v1 → Beta v1 → Stable v2` with `x-immich-state: Stable`. This gives a machine-readable stability signal per operation, which is unusually good for a self-hosted project.

**Token characteristics (read from server source, not just the spec).**

- The token is an **opaque 32-byte random value**, not a JWT: `AuthService.createLoginResponse()` calls `this.cryptoRepository.randomBytesAsText(32)` and stores only `hashSha256(token)` in the `session` table ([`server/src/services/auth.service.ts`](https://github.com/immich-app/immich/blob/v3.3.1/server/src/services/auth.service.ts)).
- Because it is opaque and looked up by hash, the OpenAPI `bearerFormat: JWT` declaration is **not accurate for password login** — this is inferred from source; no documentation states it either way.
- **No refresh token mechanism exists.** There is no `/auth/refresh`. A client either re-logs in or falls back to an API key.
- `POST /auth/validateToken` (Stable) exists to check whether the current credential is still valid: "Validate the current authorization method is still valid." This is the correct startup probe for a background client.
- `POST /auth/logout` deletes the session.

**Expiry.** For interactive login, `createLoginResponse()` does not set `expiresAt`, so the column is `NULL` and `SessionRepository.getByToken()` treats `NULL` as non-expiring. A separate `POST /sessions` endpoint can mint a child session with `expiresAt = now + dto.duration` when a duration is supplied — but its own docs describe it as "used for casting", not for third-party clients. `SessionCleanup` deletes sessions where `expiresAt <= now` **or** `updatedAt <= now - 90 days` ([`server/src/repositories/session.repository.ts`](https://github.com/immich-app/immich/blob/v3.3.1/server/src/repositories/session.repository.ts)). I found no per-request refresh of `session.updatedAt` in the session repository, which suggests the 90-day clock is *last write*, not *last use* (**inferred from source**, not documented). Practically: a sync client must treat a long-lived session token as revocable at any moment by an admin ("logout all devices"), a password change (`onAuthChangePassword` invalidates all other sessions), or the cleanup job, and must handle `401` by re-authenticating.

**Password login can be switched off server-side — the single biggest auth risk for this product.**

```ts
async login(dto: LoginCredentialDto, details: LoginDetails) {
  const config = await this.getConfig({ withCache: false });
  if (!config.passwordLogin.enabled) {
    throw new UnauthorizedException('Password login has been disabled');
  }
```
([`auth.service.ts` lines ~69-75](https://github.com/immich-app/immich/blob/v3.3.1/server/src/services/auth.service.ts))

`passwordLogin.enabled` is an admin system config, and Immich ships CLI tooling that flips it (`server/src/services/cli.service.ts` sets `passwordLogin.enabled = false`). This is the documented "disable password login" admin feature (Immich's own docs assets include `disable-password-login.webp` / `password-login-settings.webp`). A client whose only auth path is username+password **will be locked out** on such servers. The product requirement ("user logs into Immich with username + password via the config UI") must therefore be paired with an API-key fallback path in the same window.

**Login attempts are not rate-limited by the application.** I grepped the entire `v3.3.1` server source for `Throttle|throttler|rate-limit|rateLimit`, `too many requests`, `429` and found nothing relevant. What does exist is constant-time bcrypt comparison against a dummy hash to prevent user enumeration:

```ts
// Always run bcrypt so response time is constant regardless of whether the email
// is registered, preventing timing-based user enumeration.
const isAuthenticated = this.cryptoRepository.compareBcrypt(dto.password, user?.password ?? LOGIN_DUMMY_HASH);
```

Consequence: there is no server-side backoff to respect on auth failures, and conversely the client should not assume it will be throttled. Immich's own nginx example reverse-proxy config contains no rate limiting either — it focuses on `client_max_body_size` and buffering ([docs reverse-proxy.md](https://github.com/immich-app/immich/blob/main/docs/docs/administration/reverse-proxy.md)). Operators may add rate limiting at the proxy, so the client should still handle `429`.

**API keys.**

- `POST /api-keys` creates a key with an explicit `permissions` array (`min(1)`), and returns the `secret` **once**; only the SHA-256 hash is stored ([`server/src/services/api-key.service.ts`](https://github.com/immich-app/immich/blob/v3.3.1/server/src/services/api-key.service.ts)).
- `POST /api-keys/{id}/rotate` mints a new secret for the same key; `POST /api-keys/me`, `GET /api-keys`, `PUT`, `DELETE` round it out.
- **API keys do not expire.** No `expiresAt` field exists in `ApiKeyCreateDto`/`ApiKeyResponseDto` or in the service. They live until rotated or deleted.
- Auth header is `x-api-key`. Permissions are enforced centrally: `AuthService.authenticate()` rejects with `403 Missing required permission: <perm>` when an API key lacks the endpoint's `x-immich-permission`. The permission vocabulary is enumerated in `Permission` in [`server/src/enum.ts`](https://github.com/immich-app/immich/blob/v3.3.1/server/src/enum.ts) and mirrored in the spec as `x-immich-permission`.
- Scheme coverage varies by endpoint: `POST /auth/login` carries no `security` block at all (it is the unauthenticated entry point), while asset and sync endpoints list `bearer`, `cookie` and `api_key`. The `sync.stream` / `syncCheckpoint.*` permissions exist in the `Permission` enum and are ordinary grantable permissions — I found **no** exclusion list restricting them out of API keys (**inferred from source**; not documented).

**OAuth** is available (`/oauth/authorize`, `/oauth/callback`, `/oauth/link`, `/oauth/unlink`, `/oauth/mobile-redirect`, `/oauth/backchannel-logout`) and documented at [docs.immich.app/administration/oauth](https://docs.immich.app/administration/oauth/). For a desktop sync client, OAuth would require a loopback redirect URI; the API supports `redirectUri` and PKCE (`codeChallenge` is accepted by `POST /oauth/authorize`). This is a real option but is **not required** for v1, and the requirement as stated is username+password.

**v3 auth-relevant breaking changes** ([Migrating to v3](https://immich.app/blog/v3-migration), published 2026-07-01):
- OAuth HTTP requests are no longer permitted by default; `oauth.allowInsecureRequests` must be set in admin system config.
- `oauth.issuerUrl` must now parse as a valid URL.
- **Error responses changed shape entirely** (class-validator → Zod), and `correlationId` moved from the JSON body to the `X-Correlation-ID` **response header**. Old: `{"message":[...],"error":"Bad Request","statusCode":400,"correlationId":"..."}`. New: `{"message":"Validation failed","errors":[{"path":["comment"],"message":"..."}]}`. Any client that parses error bodies must be version-aware.
- Shared-link auth no longer accepts `query.password`.

### 1.2 Upload flow

**There is no chunked or resumable upload.** `POST /assets` is a single multipart/form-data request:

- Body schema `AssetMediaCreateDto`: `assetData` (binary, **required**), `sidecarData` (binary, optional), `filename`, `fileCreatedAt` (**required**), `fileModifiedAt` (**required**), `isFavorite`, `livePhotoVideoId`, `metadata` (array of `AssetMetadataUpsertItemDto`), `visibility`.
- Responses: `201` "Asset uploaded successfully" and `200` "Asset is a duplicate", both returning `AssetMediaResponseDto {id, status}` where `AssetMediaStatus ∈ {created, duplicate}`.
- Server side, `FileUploadInterceptor` configures multer with `fields([{name:'assetData',maxCount:1},{name:'sidecarData',maxCount:1}])` and **no `limits` option**. The only body-parser limit in the app is `json({limit:'10mb'})` / `urlencoded({limit:'10mb'})`, which does **not** apply to multipart file parts ([`server/src/app.common.ts`](https://github.com/immich-app/immich/blob/v3.3.1/server/src/app.common.ts), [`server/src/middleware/file-upload.interceptor.ts`](https://github.com/immich-app/immich/blob/v3.3.1/server/src/middleware/file-upload.interceptor.ts)).
- Therefore the effective ceiling is (a) the reverse proxy and (b) the user's quota. Immich's own nginx guidance is `client_max_body_size 50000M;` with `proxy_request_buffering off;` "to prevent OOM on reverse proxy server and make uploads twice as fast".
- Quota is enforced *before* the asset row is written: `requireQuota()` throws `BadRequestException('Quota has been exceeded!')` (HTTP 400) when `quotaSizeInBytes < quotaUsageInBytes + size`.
- Format is validated by filename via `canUploadFile()` → `mimeTypes.isAsset(filename)`; unsupported input throws `400 Unsupported file type <name>`.
- Zero-byte files are rejected: `400 File is empty`.

**Checksum-based dedup — two mechanisms, both SHA-1.**

1. **Per-file pre-check via header.** `POST /assets` accepts an optional `x-immich-checksum` header, documented in the spec as "sha1 checksum that can be used for duplicate detection before the file is uploaded". The `AssetUploadInterceptor` short-circuits: if a matching asset exists it returns `200` with `status: duplicate` **without consuming the upload body**.
2. **Bulk pre-check.** `POST /assets/bulk-upload-check` (Stable, permission `asset.upload`) — "Determine which assets have already been uploaded to the server based on their SHA1 checksums." Request `AssetBulkUploadCheckDto {assets: [{id, checksum}]}`; response `{results: [{action, id, assetId?, isTrashed?, reason?}]}` where `action ∈ {accept, reject}` and `reason ∈ {duplicate, unsupported-format}`. `id` is a "client-side identifier echoed in the response to match results to inputs".
   - Checksum encoding: `"Base64 or hex encoded SHA1 hash"`. The decoder is `Buffer.from(checksum, checksum.length === 28 ? 'base64' : 'hex')` ([`server/src/utils/request.ts`](https://github.com/immich-app/immich/blob/v3.3.1/server/src/utils/request.ts)) — i.e. 28 characters ⇒ base64, otherwise hex. A client must respect that length heuristic.
   - `isTrashed` is returned so a client can distinguish "already present" from "present but in trash".
   - The spec does **not** declare a `maxItems` for the `assets` array (**unverified limit**). Contrast `SyncAckSetDto.acks`, which is explicitly capped at 1000. Batch conservatively.
3. **Server-side always-on dedup.** The server streams the upload through `createHash('sha1')` while writing to disk, stores the digest in the asset row with `checksumAlgorithm: sha1File`, and treats an insert conflict as a duplicate: it looks the existing id up by checksum and returns `200 status: duplicate` ([`server/src/services/asset-media.service.ts`](https://github.com/immich-app/immich/blob/v3.3.1/server/src/services/asset-media.service.ts)). The CLI docs state this explicitly: "Immich always performs its own deduplication through hashing, so this is merely a performance consideration." ([CLI docs](https://github.com/immich-app/immich/blob/main/docs/docs/features/command-line-interface.md)). SHA-1 here is a change-detection key, not a security boundary.

**Correct client ordering.** `bulk-upload-check` (batch by SHA-1) → for each `accept` result, `POST /assets` with `x-immich-checksum` set so a race with another client still resolves to a cheap `200 duplicate` → treat `status: created` as success, `status: duplicate` as success-without-transfer. Because there is no resumable upload, a network drop mid-file means restarting that file from byte 0.

**Upload completion ≠ processing completion.** After the asset row is created the server queues `JobName.AssetExtractMetadata` (`{id, source:'upload'}`); thumbnail generation, ML and (if enabled) storage-template migration follow. Immich's docs describe this chain: "When a new asset is uploaded it kicks off a series of jobs, which include metadata extraction, thumbnail generation, machine learning tasks, and storage template migration" ([jobs-workers.md](https://github.com/immich-app/immich/blob/main/docs/docs/administration/jobs-workers.md)). A sync client should track *upload accepted*, not *processed*, and must not block on the queue.

**v3 upload-affecting breaking changes** ([migration guide](https://immich.app/blog/v3-migration)):
- `POST /assets`: `deviceId` and `deviceAssetId` **removed** from `AssetMediaCreateDto`; `duration` is now a nullable integer in milliseconds.
- `AssetResponseDto`: `deviceId`/`deviceAssetId` and `unassignedFaces` removed; `people` no longer includes faces; `width`/`height` became integers.
- `PUT /assets/:id/original` (`replaceAsset`) **removed**, along with the `asset.replace` permission — replacement is now `PUT /assets/:id/clone` (`copyAsset`).
- `POST /assets/exists` (`checkExistingAssets`) **removed** — use `bulk-upload-check`.
- `POST /sync/delta-sync` and `POST /sync/full-sync` **removed**.
- `GET /assets/random` removed (use `POST /search/random`), `GET /server/theme` removed (use `GET /custom.css`).
- Some error messages were changed to avoid leaking resource existence ([immich-app/immich#28154](https://github.com/immich-app/immich/issues/28154)).

### 1.3 Organizational features relevant to folder sync

**Immich has no server-side "upload folder" concept, and the API gives the client no way to set one at upload time.** `AssetMediaCreateDto` has no path/folder field. The server decides the on-disk location from the admin's **storage template** (default `Year/Year-Month-Day/Filename.Extension`), the per-user storage label, and the album (optionally) — all admin-controlled Handlebars templates, e.g. `{{y}}/{{#if album}}{{album}}{{else}}Other{{/if}}/{{MM}}/{{filename}}` ([storage template docs](https://github.com/immich-app/immich/blob/main/docs/docs/partials/_storage-template.md)). **Local folder structure is not preservable on the server.**

What Immich actually models, and the closest equivalents:

| Local concept | Immich model | API |
|---|---|---|
| Folder | none (a *view* over server library paths) | `GET /view/folder`, `GET /view/folder/unique-paths` (permission `folder.read`) |
| Folder | **album** (the practical mapping) | `POST /albums`, `PUT /albums/{id}/assets`, `PUT /albums/assets` |
| Burst / live photo | **stack** (`AssetMediaCreateDto.livePhotoVideoId` links the video to the photo) | `POST /stacks`, `PUT /stacks/{id}/assets/{assetId}` |
| Flat category | **tag** | `POST /tags`, `PUT /tags/{id}/assets` |
| Sidecar | XMP file uploaded as `sidecarData` | `POST /assets` (`sidecarData`) |

- The **folder view** is explicitly a browser over asset *original paths*: "Retrieve assets that are children of a specific folder", gated by `folders.enabled` and `sidebarWeb` config, and Immich's own docs say it is "handy for a highly curated and customized external library or a nicely configured storage template" ([folder-view.md](https://github.com/immich-app/immich/blob/main/docs/docs/features/folder-view.md)). It is not an upload target. Both `/view/folder` endpoints are `x-immich-state: Stable` (permission `folder.read`); the user-facing feature flags are the `FoldersResponse` / `FoldersUpdate` config types (`enabled`, `sidebarWeb`).
- The Immich CLI's `--album` flag is precisely the folder→album mapping: "Automatically create albums for each uploaded asset based on the name of the folder they are in." This is the pattern a GUI sync client should offer, opt-in.

**Sidecars (XMP).** Supported end-to-end: upload via `sidecarData`; the server stores the file next to the media and parses a *limited* field set — description (`dc:description`, `tiff:ImageDescription`), rating (`xmp:Rating`), datetime (a prioritized list of EXIF/XMP tags), location (`exif:GPSLatitude`/`GPSLongitude`), tags (`digiKam:TagsList`, `lr:HierarchicalSubject`, `IPTC:Keywords`). Naming rules: `IMG_0001.jpg.xmp` is preferred, `IMG_0001.xmp` is the fallback, other names are not recognized. Immich also writes back to the sidecar on metadata edits ([xmp-sidecars.md](https://github.com/immich-app/immich/blob/main/docs/docs/features/xmp-sidecars.md)).

**Live photos.** `livePhotoVideoId` (UUID) links a video asset to a photo asset. The server validates the link before insert (`onBeforeLink`), so the client must upload the video and capture its `id` first.

**RAW/HEIC.** Supported: the docs list HEIC, HEIF, RAW, RW2, TIFF, JPEG XL, AVIF, PSD, JPEG 2000, MPO as supported image formats and a broad video list (MOV, MP4, MKV, AVI, WEBM, 3GP, MPEG-TS, MXF, WMV). The authoritative list is server code: [`server/src/utils/mime-types.ts`](https://github.com/immich-app/immich/blob/main/server/src/utils/mime-types.ts) ([supported-formats.md](https://github.com/immich-app/immich/blob/main/docs/docs/features/supported-formats.md)).

**Duplicates.** Distinct from upload dedup: `GET /duplicates`, `GET /duplicates/{id}`, `POST /duplicates/resolve` (all `alpha`) expose Immich's perceptual duplicate detection.

### 1.4 Rate limiting, pagination, job queues

- **Rate limiting: none in the application.** No throttler module, no `429` emission, no lockout found in `v3.3.1` server source. Design implication: the client owns politeness. Respect a server-returned `429`/`Retry-After` if a proxy introduces one, but do not rely on the server to tell you to slow down.
- **Pagination:** `POST /search/metadata` (`MetadataSearchDto`, Stable) with `page`/`size`; `AssetBulkUploadCheckDto` is unpaginated. Search `page`/`size` became integers in v3.
- **Job queues are largely non-Stable.** In the v3.3.1 spec: `GET /jobs` and `PUT /jobs/{name}` are **Deprecated**; `GET /queues`, `GET /queues/{name}`, `GET /queues/{name}/jobs`, `PUT /queues/{name}`, `DELETE /queues/{name}/jobs` are **Alpha**. `POST /assets/jobs` ("Run an asset job") is Stable but triggers work rather than reporting status. Queue names are enumerated in `QueueName` (`thumbnailGeneration`, `metadataExtraction`, `videoConversion`, `sidecar`, `storageTemplateMigration`, …) and job outcomes are `success|failed|skipped`.
  **Design implication:** do not build sync correctness on job-status polling. Polling an Alpha endpoint creates a hard dependency on an unstable surface. Treat the queue as fire-and-forget and expose it only as optional diagnostics.
- **Pagination + queue depth also affect client retry design**: because upload acceptance is cheap and processing is asynchronous, the natural client-side "done" state is *server confirmed asset id*, not *thumbnail exists*.

### 1.5 API versioning and stability guarantees

- The API is published as OpenAPI; Immich's own [api.md](https://github.com/immich-app/immich/blob/main/docs/docs/api.md) is **only** about how the spec is generated (`@nestjs/swagger` decorators, `mise open-api`, generated TS/Dart SDKs under `packages/sdk/client` and `mobile/generated/openapi`). **It contains no third-party stability promise, no deprecation policy, and no supported-version window. I could not find any published API stability guarantee for third-party clients** — this is a significant negative finding, not an oversight in the search.
- What *is* published is per-operation machine-readable stability in the spec: `x-immich-state ∈ {Stable, Beta, Alpha, Deprecated, Internal}` and `x-immich-history` recording the transitions per API major version (`v1`, `v2`, `v3`). The `v3.3.1` spec documents **193 paths**; counting operations across those paths gives **Stable 200, Alpha 35, Deprecated 10, Beta 9, Internal 2, unmarked 23** (279 operations total). Even allowing for the fact that ~a third of the non-Stable entries are admin/maintenance endpoints a client never touches, the unstable surface a client can see includes `/queues`, `/duplicates/*`, `/view/folder`, `/asset-files` and the deprecated `/jobs`.
- The API major version tracks the server major version. Immich's own migration guide says the v3 breaking changes "are updates to API endpoints and affect only **third-party tools** that integrate with Immich's API" — i.e. Immich explicitly recognises third-party API consumers as the audience that must migrate, and does not guarantee compatibility across server majors.
- **Practical recommendation:** pin to `x-immich-state: Stable` operations only, feature-gate on server version (`GET /server/about` / `versionCheck`), and handle the v3 error-body change (`errors[]` + `X-Correlation-ID`) as a version-conditional parse.

### 1.6 Existing official/community clients (the actual gap)

**Official CLI — `@immich/cli`** ([docs](https://github.com/immich-app/immich/blob/main/docs/docs/features/command-line-interface.md)) is much closer to a sync client than the name suggests. Documented flags include:

- `--watch` — "Watch for changes and upload automatically"
- `--album` — create one album per source folder; `--album-name`
- `-c, --concurrency <number>` — default **4**
- `--dry-run`, `--json-output` (`newFiles`, `duplicates`, `newAssets`), `--no-progress`
- `-h, --skip-hash`, `-i/--ignore <glob>`, `-H/--include-hidden`, `--visibility`, `--delete`, `--delete-duplicates`, `--recursive`

Constraints: requires Node.js 22+ (or Docker, `ghcr.io/immich-app/immich-cli`), authenticates with an **API key only** (`immich login [url] [key]`), and stores it in **plaintext `auth.yml`** in `~/.config/immich/` with the docs explicitly warning the user to delete it after use. No GUI, no tray, no bandwidth cap, no persistent retry queue, no conflict handling.

**Community:** [immich-go](https://github.com/simulot/immich-go) (recommended by Immich's docs for Google Photos Takeout imports) and, per [tlwhittaker/awesome-immich](https://github.com/tlwhittaker/awesome-immich), a set of small utilities (face→album sync, upload optimizer, drop uploader). These are importers/utilities, not folder-sync daemons.

**Desktop client status.** I found **no official Immich desktop application** in Immich's docs or repository tree. The request is long-standing and unresolved: [immich-app/immich Discussion #5049 "[Feature] Windows & MAC OS Client"](https://github.com/immich-app/immich/discussions/5049) was opened 2023-11-14, accumulated 41 comments, and was **closed as `duplicate`** with no answer chosen.

**The gap is partially filled by third parties already** — worth knowing before building:
- [ImmiSync](https://immisync.astily.dev/) — "a UI desktop sync client for Windows PC… runs in the background, detects new files, and automatically uploads them."
- [Immich RapidSync](https://apps.microsoft.com/detail/9mznhqkrw3qv) (Microsoft Store) — WinUI 3 utility focused on scanning removable media and syncing to Immich.
These are the vendors' own product descriptions; I did not evaluate or inspect them.

**Conclusion for §1.6:** the genuine gap is not "upload" — it is a **tray-resident GUI with OS-keyring credentials, user-set bandwidth/concurrency, durable retry with permanent-vs-retriable classification, album mapping, and a status/conflict UI**. That is exactly the requirement as stated, and the CLI's `--watch`/`--album`/`--concurrency` semantics are the behaviour to match.

---

## 2. Sync-engine architecture

### 2.1 Reference architecture for a Drive-like folder sync

The shape that comparably-scoped tools converge on:

1. **Local state/index DB (SQLite)** — one row per tracked local file (path, size, mtime, SHA-1, remote asset id, state machine enum), one row per config, one row per pending retry. SQLite is the right default: embedded, transactional, crash-safe, and it gives the retry queue durability across restarts, which an in-memory queue does not.
2. **Filesystem watching** with a **periodic reconciliation scan** as the correctness backstop. Watchers miss events (overflow, network shares, sleep/resume, buffer overrun), so the scan is mandatory, not optional.
3. **Check-then-upload** using Immich's SHA-1 `bulk-upload-check`, then `POST /assets`.
4. **Atomic partial-file handling** — write in-progress state only after the server has confirmed the asset. Immich has no resumable upload, so a partial transfer is discarded, not resumed.
5. **Rename/move detection** — heuristic: same size + same mtime (or same inode/file-id on the platform) across a remove+create pair within a short window ⇒ treat as move, update the DB row, do not re-upload. Immich itself has no rename API for assets, so a rename on disk while the storage template is *off* leaves the server copy under the old internal path; the client should treat rename as "no server action" in v1.
6. **Dedup** — local SHA-1 index first (avoid re-hashing), then server `bulk-upload-check`. Hashing large libraries is the dominant startup cost; the CLI exposes `--skip-hash` for exactly this reason.
7. **Conflict detection** — for a **one-way (upload-only) v1**, conflicts are limited to "file already on server with different content". Immich's SHA-1 identity makes this clean: different hash + same filename is a *new asset*, not a conflict. Two-way sync would require modelling Immich-side deletes and edits, which the API does not expose with sufficient fidelity (see §1.5 and §6).

### 2.2 Retry classification, backoff, circuit breaking

- **Retriable:** `408`, `425`, `429`, `500`, `502`, `503`, `504`, and transport-level errors (DNS, TCP reset, TLS handshake, timeouts). `429` semantics are defined in [RFC 9110 §15.5.10](https://www.rfc-editor.org/rfc/rfc9110.html#name-429-too-many-requests); the client must honour `Retry-After` when present.
- **Permanent (mark and stop retrying):** `400`, `401` after one re-auth attempt, `403`, `404`, `405`, `409`, `413`, `415`, `422`. Two of these correspond to concrete Immich responses already verified in source: `400 Quota has been exceeded!` and `400 Unsupported file type`. `403 Missing required permission` from an API key is permanent *for that key* and should surface an actionable UI message ("re-grant `asset.upload`").
- **Backoff:** exponential with full jitter (`sleep = random(0, min(cap, base·2^attempt))`) is the standard; cap and per-file attempt ceiling enforced, with the exhausted file moved to a dead-letter state in SQLite that the UI can show and the user can retry manually. Libraries with first-party docs: Go [`github.com/cenkalti/backoff/v5`](https://pkg.go.dev/github.com/cenkalti/backoff/v5), Rust [`backon`](https://docs.rs/backon) or [`tokio-retry`](https://docs.rs/tokio-retry), or [`reqwest-retry`](https://docs.rs/reqwest-retry) for the HTTP middleware path.
- **Circuit breaker:** trip on a run of consecutive connection failures or on `401`/`403` (session revoked / key lacks permission) — retrying those in a loop is pointless and can lock an account. Half-open probe via `POST /auth/validateToken`.
- **Failure taxonomy worth persisting:** `auth_failed`, `permission_denied`, `quota_exceeded`, `unsupported_format`, `network_transient`, `server_transient`, `local_read_error` (locked file, permissions), `too_large` (proxy `413`). Only the last four are worth automatic retry beyond one attempt.

### 2.3 Bandwidth limiting and concurrency

**Go.**
- Rate limiting: [`golang.org/x/time/rate`](https://pkg.go.dev/golang.org/x/time/rate) — "A Limiter controls how frequently events are allowed to happen. It implements a 'token bucket' of size b, initially full and refilled at rate r tokens per second." It is explicitly "safe for simultaneous use by multiple goroutines" and offers `Wait(ctx)` (blocks until a token is available or the context is cancelled), which is the right primitive. `SetLimit`/`SetLimitAt` allow live reconfiguration when the user drags a bandwidth slider. Unit is *events/sec*, so for bytes/sec the client calls `WaitN(ctx, chunkSize)` per chunk to shape throughput.
- Concurrency: `golang.org/x/sync/semaphore` (weighted `Acquire`) or a plain buffered-channel semaphore; a worker pool sized to the user's concurrency setting.
- Both are pure Go, no cgo, and stable.

**Rust.**
- Rate limiting: [`governor`](https://docs.rs/governor) (GCRA / leaky bucket, `RateLimiter` with `check_n`/`until_n_ready`), or a hand-rolled token bucket. There is no `x/time/rate` equivalent in std.
- Concurrency: [`tokio::sync::Semaphore`](https://docs.rs/tokio/latest/tokio/sync/struct.Semaphore.html) — note `tokio` must be built with the `sync` feature.
- Bandwidth shaping itself is application work in both stacks: neither `reqwest` nor `net/http` has a built-in throughput cap; you wrap the request body in a rate-limited reader/writer.

### 2.4 Windows background/lifecycle concerns

- **Autostart.** The registry `Run` key (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`) is the lightweight, no-elevation option; Task Scheduler gives retry-on-failure and "run whether user is logged on or not" at the cost of setup complexity and an elevation prompt for machine-wide tasks. Microsoft's own guidance covers both ([Run and RunOnce Registry Keys](https://learn.microsoft.com/en-us/windows/win32/setupapi/run-and-runonce-registry-keys), [Task Scheduler](https://learn.microsoft.com/en-us/windows/win32/taskschd/task-scheduler-start-page)). Go can write the Run key directly with `golang.org/x/sys/windows/registry`. Rust has the `auto-launch` crate (v0.6.0, 6.65M downloads) — but it was **last published 2026-01-10**, so verify it against the current registry/`launchd` behaviour before depending on it.
- **Systray + event loop.** This is the sharpest difference between the two candidates; see §3 and §4. The Windows constraint is that `tray-icon` requires "an event loop… running on the thread" for the tray on Windows and "an event loop… on the main thread" on macOS ([tray-icon README](https://github.com/tauri-apps/tray-icon)). Wails v3 owns its own event loop and exposes the tray natively, so this constraint is invisible to the app developer.
- **Single instance.** Go: Wails v3 has it built in (`application.SingleInstanceOptions{UniqueID, EncryptionKey, OnSecondInstanceLaunch, AdditionalData}`, with optional AES-256-GCM encryption of the inter-instance payload) ([single-instance guide](https://v3.wails.io/guides/single-instance/)). Rust: the `single-instance` crate is **stale (v0.3.3, last published 2021-12-16)** — prefer a named mutex via `windows-sys` or a Unix-domain-socket/lock-file scheme on the other platforms.
- **Notifications.** Rust: `notify-rust` (v4.18.2, last published 2026-10-07) or GPUI Kit's own `SystemNotification` (requires an app identity: `cx.set_app_identity("com.example.app","Name")`). Wails: I could not retrieve the Wails reference index (the docs host returned HTTP 403 to the fetch tool), so **Wails v3's notification API surface is unverified in this pass**.
- **Sleep/resume and high DPI.** Unverifiable from docs for both stacks; GPUI Kit advertises high-refresh/on-demand update and theme-driven scaling, and Wails inherits WebView2's DPI handling. Treat both as **needs hands-on validation**.
- **Proxy and certificate stores.** Both stacks delegate to the platform: Go's `net/http` honours `HTTP_PROXY`/`HTTPS_PROXY` and imports the OS root store; `reqwest` reads the same env vars and, on Windows, uses the native TLS stack via `schannel` when `native-tls` is selected (vs `rustls` with `webpki-roots`, which bypasses the OS store). For a self-hosted Immich behind a corporate proxy or a private CA this is a real integration decision, and using the OS store is usually the correct choice.

---

## 3. Stack A — Rust + GPUI-Kit

### 3.1 What GPUI-Kit actually is

- **Identity.** GPUI Kit is a Rust desktop framework built on **Zed's GPUI**. Its own docs state: "GPUI Kit is a Rust desktop application framework built on GPUI. The `gpui-pre` dependency name refers to the published, version-pinned GPUI snapshot used by Kit, not a different rendering framework" ([gpui-kit.com/docs](https://gpui-kit.com/docs), [llms-full.txt](https://gpui-kit.com/llms-full.txt)).
- **Repo & provenance.** [`longbridge/gpui-kit`](https://github.com/longbridge/gpui-kit) — description "Rust GUI components for building fantastic cross-platform desktop application by using GPUI", **created 2024-06-13**, 16,920 stars, 1,044 forks, 99 open issues, pushed 2026-10-11, not archived. Developed by **Longbridge**; site tagline claims "Proven in production at Longbridge".
- **Renaming.** The umbrella name changed recently: crates.io `gpui-component` was **created 2025-02-06** while crates.io `gpui-kit` was **created 2026-09-03**. `gpui-component` 0.7.1 and `gpui-kit` 0.7.1 both resolve to the same repo. So: the *project* is ~20 months old; the `gpui-kit` crate name is **~1 month old as of this research**. Any "adoption" signal attached to the new name is new.
- **Crate graph.** `gpui-kit` (entry point, re-exports GPUI + Base + Component + assets), `gpui-base` (unstyled behaviour), `gpui-component` (75+ styled components), `gpui-shell` (JS extensions via QuickJS), `gpui-webview` (wry), `gpui-fps`, `gpui-kit-assets`. `gpui-shell` is v**0.1.0**, published once (2026-08-24) — do not depend on it.
- **License.** GitHub's license API reports `NOASSERTION` but the actual file is `LICENSE-APACHE`; the site states "Software · Apache-2.0", "Code examples and software source use Apache-2.0", and docs prose is additionally offered under CC BY 4.0. Underlying GPUI is "also Apache-2.0".
- **Release cadence.** GitHub releases: `v0.5.1` (2026-02-05) → `v0.6.0` (2026-09-03) → `v0.6.1`/`0.6.2`/`0.6.4`/`0.6.6` (Sept 2026) → `v0.7.0` (2026-09-28) → `v0.7.1` (2026-10-05). **Seven releases in ~5 weeks**, one minor version bump. That is a project still moving fast, not a stabilised 1.0.
- **Documentation quality is genuinely high.** `https://gpui-kit.com/llms-full.txt` is a **4.4 MB / ~95,600-line** machine-readable documentation dump covering installation, packaging, accessibility, testing, notifications, i18n, native extensions, WebView, WASM, mobile and a comparison matrix. There is a live component gallery and [docs.rs API reference](https://docs.rs/gpui-kit/latest/gpui_kit/). Whatever the maturity verdict, documentation is not the risk here.

### 3.2 Windows viability

- **Official platform requirement:** "Windows 10 or later", "Visual Studio 2022 Build Tools or Community with the **Desktop development with C++** workload, including MSVC and a Windows SDK. Use the MSVC Rust toolchain, not the GNU toolchain", plus CMake on `PATH`. Rust **1.92 or later** is required by the current locked dependency graph (the Linux GPUI platform dependency includes `oo7 0.6.0` which declares Rust 1.92 as MSRV) ([installation](https://gpui-kit.com/docs/installation)). So Windows is an explicitly supported, documented target.
- **Rendering backend.** GPUI is GPU-accelerated. On Linux GPUI Kit requires a working Vulkan driver and warns "installing `libvulkan1` alone does not install a GPU driver". On Windows the backend is DirectX via the GPUI platform layer (the `gpui-fast` alternative backend ships a `windows-manifest-provided-by-host` feature, indicating D3D is the host-provided path). **Unverified in this pass:** the exact Windows graphics API requirement (D3D11 vs D3D12), and any GPU-generation floor. This matters for older Windows fleet hardware and RDP/VM sessions.
- **WebView needed? No.** The UI is native. WebView is available but explicitly optional and **experimental**: the `gpui-wry` integration "is experimental"; on Windows the embedded WebView works "with its renderer setting"; on Linux "GTK hosting code is unfinished; no supported path yet".
- **IME/text input.** GPUI Kit ships a rope-backed editor with Tree-sitter highlighting and LSP support, an `Input` component, and documented clipboard/selection behaviour. It also documents a known cross-platform input quirk that is Windows-relevant: "[Multi-line Editor preserves CRLF carriage returns from Windows clipboard](https://github.com/longbridge/gpui-kit/issues/2877)". IME quality specifically for CJK on Windows is **not documented; unverified**.
- **High-DPI.** The framework documents `px`-based layout, theme-driven scaling, and high-refresh support with an explicit frame-budget caveat ("a 120 Hz display gives each frame about 8.3 ms; smoothness depends on the whole UI and device"). Per-monitor DPI migration on Windows is **unverified**.
- **Accessibility — the project's own rating is "partial".** The comparison table marks GPUI Kit `Accessibility = partial` (vs `egui = yes`, `Qt = yes`), and states: "A 'Yes' in the accessibility row still requires testing with assistive technology on each target platform." The accessibility guide documents AccessKit roles/names/states/actions and a headless UI test path, and its first example is a **checkbox with an empty accessible name (UIA Name)** open bug. For an "beautiful config UI" shipped to non-technical users, accessibility is a quality risk, not a blocker.
- **Windows bug tracker.** GitHub search for open issues mentioning "windows" in the repo returned **19 open issues**, including:
  - [#1866 Tabs with State Management - Clicking the tab has no effect v0.5.0 windows 11](https://github.com/longbridge/gpui-kit/issues/1866)
  - [#2018 Broken TabBar in TitleBar on Windows](https://github.com/longbridge/gpui-kit/issues/2018)
  - [#2496 TitleBar buttons hover not updating when moving from inside window (Windows)](https://github.com/longbridge/gpui-kit/issues/2496)
  - [#1909 Select dropdown menu allows mouse events to pass through to background elements](https://github.com/longbridge/gpui-kit/issues/1909)
  - [#3381 Checkbox: empty accessible name (UIA Name) when no label is provided](https://github.com/longbridge/gpui-kit/issues/3381)
  - [#3008 window hide show](https://github.com/longbridge/gpui-kit/issues/3008)
  These are the classes of bug that hurt a tray/app-shell-style app (title bar, tab behaviour, hide/show) most.

### 3.3 The dependency risk that dominates everything else: `gpui-pre`

GPUI Kit does **not** depend on the published `gpui` crate. Its root `Cargo.toml` pins an **exact pre-release snapshot**:

```toml
# GPUI comes from the `gpui-pre-*` snapshots of Zed's crates, and any snapshot
# may change GPUI's API. These requirements are what the published gpui-kit
# crates carry to crates.io, so they pin the exact snapshot the release was
# built against: a caret requirement let the weekly release move applications
# onto a newer snapshot that gpui-component did not compile with (#3156). Bump
# every snapshot crate together with its Cargo.lock entry; `script/check-gpui-pin.ts`
# fails CI when a pin is not exact.
gpui = { package = "gpui-pre", version = "=0.3.8" }
```
([gpui-kit `Cargo.toml`](https://github.com/longbridge/gpui-kit/blob/main/Cargo.toml))

Facts that follow, all from primary sources:
- `gpui-pre` is published from `github.com/zed-industries/zed`, described as "Zed's GPU-accelerated UI framework (gpui-pre snapshot of zed@93d7456)", first published **2026-09-03**, with a **weekly** cadence: 0.3.4 (2026-09-07), 0.3.5 (09-14), 0.3.6 (09-21), 0.3.7 (09-28), 0.3.8 (10-05), 0.3.9 (10-12) ([crates.io gpui-pre](https://crates.io/crates/gpui-pre)).
- The vendored comment says outright: **"any snapshot may change GPUI's API."** GPUI upstream publishes breaking API changes weekly and does not version them.
- Therefore: upgrading GPUI-Kit couples your application to a weekly-breaking upstream, and *not* upgrading means you pin an exact snapshot forever. The project mitigates with an exact pin plus CI check — good engineering, but it is mitigation of a structural risk, not removal.
- The officially published [`gpui` crate](https://crates.io/crates/gpui) is at **0.2.2, last published 2025-10-22** — a year stale — so the crates.io "gpui" name is not the maintained path. Any plan that assumes "just use GPUI directly" is on the wrong artifact.
- The project also maintains a **fork** of the platform layer, `gpui-fast` (own repo `longbridge/gpui-fast`, v0.1.6, first published 2026-10-06), as an alternative backend. A component library forking its own rendering platform is a meaningful maintenance signal.

**Verdict input (revised).** GPUI Kit is well-documented, actively developed, Apache-2.0, and genuinely cross-platform, and its UI framework dependency is a snapshot of the framework that **Zed ships in stable Windows builds** — so the core rendering/input/runtime path has real production mileage on the target OS. The `gpui-pre` pin is a liability, but it is the *same class* of liability as pinning Wails `v3.0.0-beta.28` exactly: both require pin-and-test discipline and neither is a stable contract. The difference is what an upgrade costs. Wails states its desktop API is stable, so bumping a beta should mostly be a re-test; `gpui-pre` snapshots are explicitly allowed to change API, so bumping *either* GPUI Kit or `gpui-pre` can require code migration — while staying put means forgoing upstream fixes. That is a real, quantified maintenance cost, not a reason the stack cannot ship. Also note: the stale published `gpui` crate is irrelevant to a GPUI-Kit app (you consume `gpui-pre` through `gpui-kit`), and the `gpui-fast` fork means Longbridge controls its own platform layer — capacity to fix, with divergence as the price. What remains genuinely unverified is the app-shell assembly: tray integration and the component layer's open Windows title-bar/tab/hide-show bugs. Both are testable in a short spike.

### 3.4 App-level pieces a Rust client needs

| Need | Crate | Primary-source status |
|---|---|---|
| System tray | [`tray-icon`](https://crates.io/crates/tray-icon) (tauri-apps) + [`muda`](https://crates.io/crates/muda) for menus | v0.26.1, 34.3M downloads, updated 2026-10-07, MIT. **Windows + macOS + Linux (AppIndicator/KSNI)**. Constraint: "On Windows and the Linux/FreeBSD AppIndicator backend, an event loop must be running on the thread"; macOS requires main thread. GPUI Kit ships **no tray component** (its Native Extensions doc covers only `NativeMenu` and the experimental Wry WebView), but this is the same crate the Tauri family uses. Integration is structurally plausible — a Win32 GUI framework runs a message pump on its main thread, which is what `tray-icon` requires — but **no primary source documents GPUI+tray-icon together, so treat it as a 1–3 day spike, not a known cost** (and not a blocker). |
| Single instance | `single-instance` | **Stale**: v0.3.3, last published 2021-12-16. Prefer a named mutex via `windows-sys`. |
| Autostart | [`auto-launch`](https://crates.io/crates/auto-launch) | v0.6.0, 6.65M downloads, **last published 2026-01-10**. Verify current behaviour. |
| Keyring | [`keyring`](https://crates.io/crates/keyring) 4.2.0 + per-platform stores | The crate was **restructured**: the API moved to [`keyring-core`](https://crates.io/crates/keyring-core) 1.0.0 and credential stores are now separate crates — [`windows-native-keyring-store`](https://crates.io/crates/windows-native-keyring-store) 1.1.0 (2026-05-24), [`apple-native-keyring-store`](https://crates.io/crates/apple-native-keyring-store) 1.0.2 (2026-08-06), [`linux-keyutils-keyring-store`](https://crates.io/crates/linux-keyutils-keyring-store) 1.0.0, [`dbus-secret-service-keyring-store`](https://crates.io/crates/dbus-secret-service-keyring-store) 1.0.1 (2026-08-15). License MIT OR Apache-2.0. The README explicitly says apps wanting the full ecosystem should depend on `keyring-core` + chosen stores rather than the `cli` feature. This is a **2026 migration in progress** — pin versions. |
| SQLite | [`rusqlite`](https://crates.io/crates/rusqlite) 0.40.2 (2026-08-08) or [`sqlx`](https://crates.io/crates/sqlx) 0.9.0 (2026-05-21) | rusqlite bundles SQLite (C, needs a C toolchain — fine on Windows with MSVC); sqlx is async but for SQLite still uses `libsqlite3-sys`. |
| HTTP | [`reqwest`](https://crates.io/crates/reqwest) 0.13.5 (2026-09-08) | Mature, 790M downloads. Choose `native-tls`/schannel for OS certificate store. Retry middleware: [`reqwest-retry`](https://crates.io/crates/reqwest-retry) 0.9.1. |
| Async runtime + GPUI | `tokio` 1.53.2 | GPUI has its own executor (`Task<T>`, `cx.spawn`). **Corrected from the first revision:** GPUI Kit's own docs document the pattern — "reqwest needs a tokio runtime; GPUI's executors are not one" — with a `LazyLock<tokio::runtime::Runtime>` worker runtime, and GPUI exposes an installable `HttpClient` trait (`cx.set_http_client(...)`) ([llms-full.txt](https://gpui-kit.com/llms-full.txt)). The bridge is documented, minor application wiring, not a research problem. |
| Notifications | `notify-rust` 4.18.2 (2026-10-07) or GPUI Kit `SystemNotification` | GPUI Kit requires `cx.set_app_identity(...)` before posting. |

**Packaging / code signing (Rust):** GPUI Kit itself ships no installer tooling — its packaging guide states: "`cargo build --release` produces an executable, not an installer. GPUI Kit supplies the application and UI layers; your application owns its package identity, icons, external files, installer, signing, and release tests." That is the normal division of labour in the Rust GUI ecosystem, and **the ecosystem fills the gap**: [`cargo-packager`](https://crates.io/crates/cargo-packager) (v0.11.8, Tauri/CrabNebula ecosystem) generates **NSIS `.exe` and MSI via WiX** on Windows (plus deb/AppImage/DMG elsewhere), and [`cargo-wix`](https://crates.io/crates/cargo-wix) (v0.3.9, 574k downloads) is the dedicated MSI path. GPUI Kit's own docs recommend ZIP-portable or [Inno Setup](https://jrsoftware.org/ishelp/topic_setup_signeduninstaller.htm) (sample `packaging.iss`, two-pass signed uninstaller, signtool ordering); `cargo-packager`'s NSIS path replaces most of that script. So "GPUI Kit has no installer" ≠ "Rust has no installer": the cost is choosing and wiring a tool once — on the order of a day for NSIS plus code-signing setup — not a weeks-long workstream.

---

## 4. Stack B — Go + Wails 3 (beta)

### 4.1 Status, stability guarantees, release plan

- **Repo:** [`wailsapp/wails`](https://github.com/wailsapp/wails) — MIT, 36,518 stars, 1,881 forks, 388 open issues, created 2018-12-15, pushed 2026-10-12. Not archived.
- **Current release:** [`v3.0.0-beta.28`](https://github.com/wailsapp/wails/releases/tag/v3.0.0-beta.28), published **2026-10-05**, marked `prerelease: true`. `v3.0.0-beta.1` was published **2026-08-02** — i.e. **28 betas in ~2 months**, roughly daily-to-weekly. There is no v3 stable release.
- **The beta announcement is explicit about the compatibility posture** ([Wails v3 Beta, 2026-08-02](https://v3.wails.io/blog/wails-v3-beta/)):
  > "This is a beta release, not the final 3.0 release. **The desktop API is stable and teams are already using v3 in production**, but you should test thoroughly before deploying. We are using the beta period to catch the final compatibility and workflow issues with the community. **Wails v2 remains the current stable release** and will continue to receive fixes."
- **Package support in beta:** "The beta supports Windows on amd64 and arm64, macOS on Intel and Apple Silicon, and Linux on amd64 and arm64. GTK4 with WebKitGTK 6.0 is the default Linux stack; GTK3 remains available as a legacy option throughout the v3.0 series. Mobile support is exciting but remains experimental and is **not part of the desktop beta compatibility promise**."
- **The project is candid about its own schedule risk.** From the same post: "The first v3 alpha tag was published on **18 January 2023**. That is a long time to be in alpha, and it deserves more than a vague acknowledgement." It also states that the maintainer "has not always adapted our processes as quickly or as clearly as that growth required", and that the beta introduces "explicit beta, release-candidate, and GA milestones; clearer compatibility commitments; an updated security policy; and a WEP (Wails Enhancement Proposal) process".
- **No published GA date.** I found no dated GA commitment in the blog, changelog or docs — **unverified**.
- **v2 → v3 migration is a port, not a bump:** "v3 is a new major version, and migration is a real port rather than a version number change. The main conceptual shifts are the application and window lifecycle, services instead of context-bound bindings, direct application and window APIs instead of the v2 runtime package, and regenerated frontend bindings." A [migration guide](https://v3.wails.io/migration/v2-to-v3/) with a feature map and testing checklist is the supported path; an automated "migration assistant" is being evaluated but "is not part of this beta release".
- **Assessment for a commercial Windows app:** the API-stability claim plus 36k stars and an explicit GA milestone process is materially stronger than GPUI Kit's position. But it is still a `0.0.0-beta.N` artifact whose minor releases arrive every few days; pinning an exact beta and re-testing on upgrade is mandatory, and the fallback plan (stay on v2, or drop the shell) must exist.

### 4.2 Windows viability

- **WebView2.** The install guide states WebView2 Runtime is "usually pre-installed" and "**Windows 10/11 includes WebView2 by default**. If missing: Download from Microsoft". The **packaging** doc states the generated installer "includes a WebView2 bootstrapper that downloads the runtime if needed. If you need offline installation, download the Evergreen Standalone Installer from Microsoft" ([Windows packaging](https://v3.wails.io/guides/build/windows/), [installation](https://v3.wails.io/quick-start/installation/)). So the runtime dependency is handled at install time; fully-offline deployment needs the standalone installer bundled, which is extra work.
- **Installer.** `wails3 package GOOS=windows` "runs `wails3 task windows:package` which: Builds the application, Generates the WebView2 bootstrapper, Creates an NSIS installer" → output `build/windows/nsis/<AppName>-installer.exe`. MSIX is available: `wails3 package GOOS=windows FORMAT=msix` → `bin/<AppName>-<arch>.msix`, requiring `makeappx.exe` (Windows SDK) or standalone MSIX tooling. NSIS customisation lives in `build/windows/nsis/project.nsi`; metadata in `build/windows/info.json`. ARM64 builds are documented (`GOARCH=arm64`). `makensis` must be installed (`winget install NSIS.NSIS`).
- **Code signing is first-party.** Configure `SIGN_CERTIFICATE` (`.pfx`) or `SIGN_THUMBPRINT` (cert in the Windows store) and `TIMESTAMP_SERVER` in `build/windows/Taskfile.yml`; run `wails3 sign GOOS=windows`, or `wails3 task windows:sign` / `windows:sign:installer`. `wails3 setup signing` handles the certificate password. The docs explicitly tie unsigned builds to SmartScreen warnings. This is **strictly more turnkey than the Rust path**.
- **System tray is a native, first-class API.** `app.SystemTray.New()` returning a `*SystemTray` with `SetIcon([]byte)`, `SetMenu(*Menu)`, `SetTooltip(string)`, `OnClick(func())` (the docs' own system-tray example is `func setupSystemTray(app *application.App, window application.Window)`). The implementation lives at [`v3/pkg/application/systemtray.go`](https://github.com/wailsapp/wails/blob/v3.0.0-beta.28/v3/pkg/application/systemtray.go) and even includes macOS-specific label styling (`SystemTrayLabelPart` with `FgColor`/`BgColor` and an ANSI parser hook). Current docs pages: [reference/menu](https://v3.wails.io/reference/menu/), [reference/application](https://v3.wails.io/reference/application/). **This directly satisfies the tray requirement with zero custom event-loop integration.**
- **Single instance is built in** (see §2.4) — `application.SingleInstanceOptions`.
- **Window management:** "first-class multi-window support"; windows are explicit objects with their own lifecycle, which maps cleanly to "tray + config window + progress window".
- **Linux/macOS packaging.** Linux defaults to GTK4 + WebKitGTK 6.0 (GTK3 legacy through v3.0). macOS: I did not read the macOS packaging page in this pass — **unverified**, though a [macOS build guide](https://v3.wails.io/guides/build/macos/) and a generic [signing guide](https://v3.wails.io/guides/build/signing/) exist, and the sibling [Windows page](https://v3.wails.io/guides/build/windows/) documents signing in depth.

### 4.3 App-level Go pieces

| Need | Choice | Primary-source status |
|---|---|---|
| Keyring | [`zalando/go-keyring`](https://github.com/zalando/go-keyring) — MIT, 1,345 stars, last pushed **2026-07-24**, 36 open issues | Windows: [`danieljoos/wincred`](https://github.com/danieljoos/wincred) generic credentials, target name `service:username`. macOS: shells out to `/usr/bin/security`. Linux/BSD: D-Bus [Secret Service](https://specifications.freedesktop.org/secret-service/latest/) (GNOME Keyring), requiring the default `login` collection to exist. **Documented limits** (`ErrSetDataTooBig`): Windows password ≤ 2560 bytes, service ~32 KiB (code limits service to 512/30 KiB); macOS service+user+password ≲ 3000 bytes; Linux no theoretical limit but "performance suffers with big values (>100 KiB)". No Windows Hello/biometric gating; no libsecret-native path. Store the *Immich token* or the *password* — not both. **Note:** on Linux this needs a Secret Service provider; a headless or KDE-without-GNOME-Keyring setup fails, so a documented fallback (encrypted file + OS file permissions) is advisable for Linux. |
| SQLite | [`modernc.org/sqlite`](https://pkg.go.dev/modernc.org/sqlite) (pure Go, CGo-free) vs `github.com/mattn/go-sqlite3` (cgo) | modernc is "a `sql/database` driver using a CGo-free port of the C SQLite3 library", SQLite **3.53.4**, and lists explicit supported `GOOS/GOARCH` pairs. **Caveat from its own docs**: "**Fragile `modernc.org/libc` dependency** — When you import this package you should use in your `go.mod` file the exact same version of `modernc.org/libc` as seen in the `go.mod` file of this repository." That is a real operational hazard (transitive pinning), but it buys a cgo-free build: faster compiles, no MSVC, trivial cross-compilation. |
| HTTP client | stdlib `net/http` | Mature, honours proxy env vars and the OS trust store. Enough for multipart uploads; no third-party dependency needed. |
| Concurrency | `golang.org/x/sync/semaphore` + worker pool | See §2.3. |
| Rate limiting | [`golang.org/x/time/rate`](https://pkg.go.dev/golang.org/x/time/rate) | Token bucket, goroutine-safe, `Wait(ctx)` / `SetLimit`. See §2.3. |
| FS watching | [`github.com/fsnotify/fsnotify`](https://pkg.go.dev/github.com/fsnotify/fsnotify) | Backends: inotify (Linux), kqueue (BSD/macOS), **ReadDirectoryChangesW (Windows)**, FEN (illumos). **Not implemented:** FSEvents on macOS ("Needs support in x/sys/unix") and **USN Journals on Windows** ("Needs support in x/sys/windows"); **no polling backend**. So macOS falls back to kqueue, which needs an open FD per watched file — a scalability constraint on large trees. Windows uses ReadDirectoryChangesW, which supports recursive watching (the reason a Windows-first product is well served here). |
| Autostart | `golang.org/x/sys/windows/registry` (Run key) or Task Scheduler | See §2.4. |
| Single instance | Wails v3 built-in | See §2.4. |
| Retry | `cenkalti/backoff/v5`, or hand-rolled (a few dozen lines) | See §2.2. |
| Notifications | **unverified** | Could not retrieve Wails' reference index (HTTP 403 to the fetch tool). |
| Front-end | Any framework | Wails generates TypeScript bindings from static Go source analysis ("richer TypeScript bindings, preserving comments and meaningful parameter names"); front-end choice is documented as free. Cost: a WebView2 process tree (memory footprint **not measured in this pass — unverified**; WebView2 spawns separate browser/GPU/renderer processes, so expect substantially more RSS than a native UI). |

---

## 5. Head-to-head comparison and recommendation

> **Framing (revised 2026-10-12).** The first revision compared "GPUI-Kit alone" against "Wails v3", which structurally favours Wails: Wails is an application framework that bundles tray, single-instance, installers and signing, while GPUI-Kit is a component/UI toolkit that expects the app to assemble those from the Rust ecosystem. The honest comparison is **assembled stack vs assembled stack**: GPUI + GPUI-Kit + `tray-icon`/`muda` + `cargo-packager` + `keyring-rs` versus Wails v3 + Go libraries. Under that framing, most "missing piece" rows become "choose a crate", and the remaining differences are narrower and genuine: initial integration effort, upgrade/migration cost on the `gpui-pre` pin, web-UI velocity, and WebView2 footprint. Rows below are corrected accordingly.

| Dimension | A: Rust + GPUI-Kit | B: Go + Wails 3 (beta) | Winner for this app |
|---|---|---|---|
| **Windows maturity** | Documented Win10+ / MSVC-only target; **core GPUI is production on Windows** (Zed ships [stable Windows builds](https://zed.dev/windows)); 19 open GPUI-Kit Windows issues incl. title-bar, tab-click, hide/show; GPU floor and RDP behaviour still unverified | WebView2 preinstalled on Win10/11; amd64 + arm64; NSIS/MSIX; maturity inherited from WebView2 + Chromium | **B, small edge** — framework-level maturity is comparable; GPUI-Kit's shell bugs and unverified tray integration are the delta |
| **Systray quality** | No bundled tray. Ecosystem answer: `tray-icon` v0.26.1 (+ `muda` menus), the crate the Tauri family uses; needs an event loop on the thread (Windows) / main thread (macOS), which a Win32 GUI main thread plausibly satisfies — **unverified, 1–3 day spike** | Native `app.SystemTray.New()` with icon/menu/tooltip/click; own implementation in-repo | **B, small edge** (bundled vs assembled; A's real cost unverified) |
| **Config-UI quality & velocity** | 75+ native components, 38 themes, gallery, DataTable/VirtualList/Dock/Editor; high-quality 95k-line docs; no visual designer | Whatever the web gives you — any framework; fastest iteration, richest existing design ecosystem | **B** on velocity, **A** on native feel — but this app's UI is a settings window + tray menu, so the web-velocity edge is smaller than for content-heavy apps |
| **Async + IO fit for a sync engine** | tokio + reqwest + rusqlite — excellent; GPUI has its own executor but **GPUI Kit documents the tokio + reqwest pattern** (`LazyLock<tokio::runtime::Runtime>` + `cx.set_http_client`), so the bridge is minor wiring | Go scheduler + `net/http` + `database/sql` + fsnotify — idiomatic Go, no bridge at all | **Tie** (Go marginally simpler; Rust not blocked) |
| **Keyring** | `keyring-core` + `windows-native-keyring-store` / `apple-native-keyring-store` / `dbus-secret-service-keyring-store`; MIT/Apache-2.0; **fresh 2026 restructure — pin versions** | `zalando/go-keyring` MIT; simple API; documented size limits; macOS via `/usr/bin/security`; Linux needs Secret Service | **Tie** (Rust has cleaner per-store control; Go has a simpler API and fewer moving parts) |
| **SQLite** | rusqlite (bundled C SQLite via MSVC) or sqlx 0.9 | modernc.org/sqlite (pure Go, cgo-free, but "fragile libc pin") or mattn (cgo) | **A** (no cgo-pinning hazard), **B** on build simplicity |
| **Packaging / installers** | No tooling bundled, but the ecosystem has it: `cargo-packager` (NSIS + WiX MSI) and `cargo-wix` (MSI); GPUI Kit docs also give an Inno Setup recipe | NSIS by default, MSIX optional, WebView2 bootstrapper generated, ARM64 documented | **B, small edge** (turnkey vs one-day wiring) |
| **Code signing** | Manual: signtool + ordering rules (two-pass signed uninstaller with Inno) | `SIGN_CERTIFICATE`/`SIGN_THUMBPRINT`/`TIMESTAMP_SERVER` + `wails3 task windows:sign[:installer]` | **B, small edge** (certificate work is comparable either way) |
| **Binary size** | GPUI Kit's own estimate **~12 MB** minimal (their own note: "approximate references from different build conditions, not verified minimums") | Not published; WebView2 is out-of-process (system-provided), so the shipped app is small but total install footprint includes WebView2 | **A** nominally; not measured |
| **Memory** | Native GPU UI | WebView2 multi-process browser runtime | **A**, but **not measured in this pass** |
| **Startup time** | Native, on-demand GPU draw | WebView2 process warm-up | **A**, **not measured** |
| **Upstream stability** | Pinned to exact `gpui-pre` snapshots whose API may change per snapshot. Mitigations: `gpui-pre` is a snapshot of the framework **Zed ships in stable Windows builds**, so runtime maturity is real; the exposure is migration cost on upgrade (or freeze and forgo fixes). `gpui-fast` backend fork = divergence risk; crate name 1 month old | `0.0.0-beta.28`; docs claim "**the desktop API is stable**"; beta/RC/GA milestones + WEP process; v2 remains the fallback stable line. **Symmetric reality:** this is also an exact pre-release pin, with the same re-test discipline | **B, moderate** (Wails' API-stability statement makes upgrades cheaper; both are pre-GA) |
| **Hiring / maintainability** | Rust GUI specialists are scarce; GPUI-specific knowledge is scarcer | Go is mainstream; web front-end skills are abundant; UI work is ordinary web work | **B** |
| **Long-term maintenance risk** | Compounding: framework pin + backend fork + 0.x cadence + partial a11y + smaller talent pool | Concentrated: one pre-GA dependency with a stated GA path, plus a real (if costly) v2 fallback | **B, moderate**, with the caveat that Wails' 3.5-year alpha is itself evidence of schedule risk |

### Pros/cons for *this* app

**Stack A (Rust + GPUI-Kit) — pros.** Best-looking, most "native" result; tiny footprint relative to a webview; genuinely good component library and documentation; Apache-2.0 throughout; the sync engine in Rust is fast and safe (tokio/reqwest/rusqlite are best-in-class); a single statically-linked `.exe` is a nice deployment artifact; and there is no WebView2 runtime question at all.

**Stack A (Rust + GPUI-Kit) — cons (for this app specifically, revised).** (1) **No bundled tray** — `tray-icon`/`muda` exist, but GPUI integration is unproven, so budget a 1–3 day spike. (2) **`gpui-pre` pin**: upgrades can require migrations; freezing forgoes fixes (same discipline as Wails, higher upgrade tax). (3) **No bundled installer/signing** — `cargo-packager`/`cargo-wix` + signtool make this one-time wiring, not a blocker. (4) Accessibility self-rated "partial". (5) Open GPUI-Kit Windows title-bar/tab/hide-show bugs hit shell behaviours a tray app needs. (6) Rust GUI specialists are hard to hire. None is fatal; (1) and (5) are what a spike must prove or kill.

**Stack B (Go + Wails 3) — pros.** Tray, single-instance, multi-window, NSIS/MSIX, WebView2 bootstrapping, ARM64 and code-signing tasks are **first-party and documented**. The sync engine is idiomatic Go with stable, boring libraries (`net/http`, `database/sql`, `x/time/rate`, fsnotify). ReadDirectoryChangesW is fsnotify's Windows backend and supports recursive watching. Front-end velocity for a "beautiful config UI" is high, and the skill pool is deep. The maintainers publish an explicit stability posture and GA milestones.

**Stack B — cons.** (1) **It is a beta** (`0.0.0-beta.28`) with releases every few days; you must pin and re-test. (2) 3.5 years of alpha is a schedule-risk signal that the GA milestone may slip again. (3) WebView2 memory footprint — likely meaningfully higher than native, and **unmeasured here**. (4) WebView2 offline install needs the standalone installer bundled. (5) `modernc.org/sqlite`'s "fragile `modernc.org/libc` dependency" needs exact pinning. (6) `zalando/go-keyring` on Linux needs a running Secret Service.

### Recommendation

**Revised recommendation: Go + Wails 3 remains the lower-risk default, but the margin is modest and the decision belongs to a short spike.** After correcting the comparison, Wails does not win on "the other stack can't do it" — it wins on **time-to-shippable-Windows-artifact** and **familiarity/velocity**, which are real advantages for a small team. GPUI-Kit is a legitimate choice for a Rust-capable team that wants a native artifact and should not be ruled out on the earlier framing.

1. **First-party shell features remove integration risk, not capability.** Wails bundles tray, single-instance, multi-window, NSIS/MSIX and signing; the Rust equivalents exist as separate assembly choices. Summed: days of work and a few unknowns (chiefly tray + GPUI event-loop coexistence).
2. **UI velocity favours Wails, but less than it looks.** The UI is a settings window plus a tray menu; GPUI-Kit's components and themes cover that comfortably. The web advantage is design polish and hiring, not raw possibility.
3. **The sync engine is fine on both.** Go is marginally simpler (boring stdlib, no executor question); Rust is excellent and the tokio/reqwest pattern is documented. Do not choose the stack on this axis.
4. **Upstream risk is differently shaped, not absent on either side.** Wails: one pre-GA dependency with a stated stable desktop API and a v2 fallback. GPUI-Kit: pre-GA toolkit over a snapshot of a framework that is nevertheless in production on Windows via Zed. Wails' upgrade tax is lower; GPUI's runtime is not unproven.
5. **Immich's API is small, stable-enough and version-aware.** The four operations that matter (`/auth/login`, `/auth/validateToken`, `/assets/bulk-upload-check`, `POST /assets`) are all `x-immich-state: Stable`. v3's breaking changes are manageable with version gating, and the folder-structure limitation is a product-design constraint (album-per-folder), not a stack choice.

**Decision procedure (either choice is defensible; this settles it).** A 3–5 day Windows spike: (a) GPUI-Kit window + `tray-icon` icon/menu driving it — the single biggest unknown; (b) `cargo-packager` NSIS build + signtool with a test certificate; (c) upload one file to an Immich instance and read `bulk-upload-check`; (d) in parallel, Wails v3 idle-in-tray and startup memory. If (a) lands in a day or two, Rust clears its main practical hurdle and the rest is team-skills/native-footprint preference. If (a) fights back, take Wails.

**Mitigations that make the recommendation safe.**
- Put the entire sync engine in a `internal/sync` package with **no Wails imports** (interfaces for events/UI notifications). Then "swap the shell" is a real option, not a slogan.
- Pin `v3.0.0-beta.28` exactly; set an upgrade policy (re-test tray, signing, installer, single-instance on every bump); keep a written fallback plan: (a) Wails v2, or (b) a local HTTP/WebSocket service + any thin shell.
- Do not depend on Alpha endpoints (`/queues`) for correctness; use only Stable operations.
- Treat keyring and Linux Secret Service as an explicit compatibility matrix with a documented fallback.

**What evidence would change this recommendation (revised):**
- The tray spike result, either way. If `tray-icon`/`muda` integrates with GPUI in a few hundred lines, the Rust stack's main practical unknown is cleared. If it requires fighting GPUI's event loop or patching a platform layer, Wails wins outright.
- Whether GPUI-Kit's open Windows shell bugs (#2018, #2496, #1866, #3008) are fixed promptly in the beta cadence or linger.
- Whether `gpui-pre` gains a stability/versioning contract (a maintained published `gpui`, or GPUI Kit depending on tagged releases rather than `gpui-pre` snapshots) — that removes the upgrade tax.
- A measured WebView2 resident-memory/startup budget (idle tray and a 50k-file library) against your product targets; if modest, Wails' main counter-cost shrinks.
- If Wails v3 GA slips again past a date you can tolerate, or a beta regression lands in tray/installer/signing and is not fixed in a reasonable window, prefer v2 or re-open the Rust option.

**Third options, briefly.** Tauri 2 is relevant mostly because it *owns the same tray crate Rust would need* — [`tauri-apps/tray-icon`](https://github.com/tauri-apps/tray-icon) is maintained by the Tauri org — and because Tauri 2 has its own official [System Tray guide](https://v2.tauri.app/learn/system-tray/). But its keyring story is a third-party plugin (`tauri-plugin-keyring` v0.1.0, last published 2024-12-23; `tauri-plugin-keyring-store` v0.2.0, 2026-05-22), so it moves the keyring risk rather than removing it. It sits between A and B (Rust backend + webview UI) and **was not evaluated in depth here**. `egui` is the other credible native-Rust option and is rated `yes` on accessibility in GPUI Kit's own comparison table, but it bundles neither tray nor installer tooling either — the same ecosystem crates (`tray-icon`/`muda`, `cargo-packager`) apply, and its accessibility edge is offset by a more utilitarian visual style and weaker documentation depth than GPUI Kit.

---

## 6. Gap analysis and scope

### 6.1 What Immich Desktop/CLI already do

- **There is no official Immich desktop app** (verified: no desktop client in the docs tree or repo; [Discussion #5049](https://github.com/immich-app/immich/discussions/5049) was closed as `duplicate`). The `my.immich.app` and web UI are the web client; the mobile app is separate.
- **`@immich/cli` is the closest thing to a sync engine and it is CLI-only.** It already implements: recursive upload, folder→album mapping, concurrency (default 4), watch mode, dedup-via-hash with `--skip-hash`, glob ignore, visibility, dry-run, JSON output, and optional local deletion. It does **not** offer: a GUI, a tray, OS-keyring credential storage (API key in **plaintext `auth.yml`**), bandwidth limits, a durable retry queue with error classification, a status/history UI, autostart, or single-instance behaviour.
- **Community:** [immich-go](https://github.com/simulot/immich-go) (import-focused, recommended by Immich's docs for Takeout), plus small utilities per [awesome-immich](https://github.com/tlwhittaker/awesome-immich). These are import tools, not resident sync daemons.
- **Third-party GUI sync clients already exist** ([ImmiSync](https://immisync.astily.dev/), [Immich RapidSync](https://apps.microsoft.com/detail/9mznhqkrw3qv)), so "the gap" is contested ground. The differentiators that remain genuine, given the stated requirements, are: **OS-keyring credential storage**, **user-configurable bandwidth + concurrency**, **rigorous retriable/permanent failure classification with a durable queue**, and **a polished, accessible config UX**. Those are quality axes, not feature-presence axes — plan accordingly.

### 6.2 Minimum viable v1

Must have (maps 1:1 to the requirements):
1. **Config window**: server URL, email+password login (`POST /auth/login`), optional API-key path (`x-api-key`) for servers where `passwordLogin.enabled` is off; credential stored via OS keyring; `POST /auth/validateToken` on startup.
2. **Folder selection**: add/remove local folders, recursive toggle, include/exclude globs, optional album-per-folder mapping (`--album` semantics).
3. **Resident sync engine**: fsnotify (ReadDirectoryChangesW on Windows) + periodic reconciliation scan; SQLite state table; SHA-1 index; `bulk-upload-check` → `POST /assets` with `x-immich-checksum`; sidecar (`sidecarData`) and live-photo (`livePhotoVideoId`) support.
4. **User limits**: bandwidth cap (token bucket over the request body, live-adjustable) and upload concurrency (semaphore), both persisted.
5. **Reliability**: status-class retry table, exponential backoff with full jitter, dead-letter state visible in the UI with a manual "retry" action, `401` → re-auth prompt, `403` → actionable permission message, `400 Quota…` → permanent-quota state.
6. **Shell**: tray icon with Show / Pause / Resume / Sync-now / Quit and a live status; single instance (focus the config window when a second instance starts); autostart toggle (HKCU Run key by default, Task Scheduler optional); OS notifications for completed batches and errors; the window closes to tray rather than exiting.
7. **Operational**: a diagnostics view showing server version, granted API-key permissions, last error per category, and a rate/throughput readout.

### 6.3 Explicitly defer

- **Two-way sync** (download, delete propagation, rename propagation, edit reconciliation). Immich does not expose a download-side change feed with enough fidelity for delete/rename semantics at the asset level, and `x-immich-state` on candidate endpoints is not all Stable; and the v3 removal of `delta-sync`/`full-sync` in favour of `/sync/stream` (whose own description scopes it to "the mobile app") means the download-sync contract is currently mobile-oriented, not third-party-oriented.
- **Conflict resolution UI.** A one-way uploader with SHA-1 identity has no true conflicts — same content is a duplicate, different content is a new asset.
- **Job-status polling** — `/jobs` is Deprecated and `/queues` is Alpha; surface it as optional diagnostics only, never as sync correctness.
- **External library management** (`/libraries`), duplicate resolution (`/duplicates/resolve`, Alpha), OCR, workflows, cluster groups.
- **macOS notarization and Linux packaging** beyond a documented, untested build. Ship Windows first.
- **OAuth login** (loopback + PKCE). Add later if password-login-disabled servers become common in the target user base.
- **Mobile/companion clients.**

---

## 7. Open questions and uncertainty

Labelled by what would resolve them.

**Must verify by hands-on test (highest priority):**
1. **GPUI Kit + `tray-icon`/`muda` on Windows.** No primary source documents this integration; it is the swing factor for Stack A. Measure lines of code and whether the tray event loop coexists with GPUI's. Also test the open Windows bugs [#2018](https://github.com/longbridge/gpui-kit/issues/2018), [#2496](https://github.com/longbridge/gpui-kit/issues/2496), [#1866](https://github.com/longbridge/gpui-kit/issues/1866) against a tray-app shell, and confirm the `cargo-packager` NSIS + signtool path end to end.
2. **Wails v3 beta memory/startup**, idle in tray and with a 50k-file library. No primary source publishes a footprint figure. **Unverified.**
3. **GPUI Kit on Windows GPU/RDP.** D3D backend and minimum GPU are undocumented; the Linux path requires a working Vulkan driver, which suggests a hard floor that may bite in RDP/VM/older hardware. **Unverified.**
4. **Wails v3 notification API.** The reference index returned HTTP 403 to programmatic fetch, so I could not confirm what exists. **Unverified.**

**Facts I could not establish from a primary source:**
5. Whether Immich intends any third-party API stability guarantee. [api.md](https://github.com/immich-app/immich/blob/main/docs/docs/api.md) contains only generator notes. **No published policy found.**
6. Whether `session.updatedAt` is refreshed on ordinary API use. I found no per-request touch in the session repository, so the 90-day cleanup appears measured from last *write*, meaning even an actively-syncing client may be logged out ~90 days after login. **Inferred from source; treat as "expect periodic re-auth".**
7. The `maxItems` limit for `POST /assets/bulk-upload-check`. The spec declares none (unlike `SyncAckSetDto.acks` at 1000). **Unverified — probe empirically and batch ≤ a few hundred.**
8. Wails v3 GA timing. **No date published.**
9. Wails v3 macOS packaging/notarization specifics — the guide exists but I did not read it in this pass. **Unverified.**
10. Whether API keys can hold `sync.stream` / `syncCheckpoint.*`. The permissions exist in the enum and I found no exclusion list, but I did not find documentation confirming they are grantable via the UI. **Inferred from source.**

**Product-level uncertainty (not verifiable from docs):**
11. Immich's server-side storage template means the user's local folder tree is **not** mirrored on the server. This is a product expectation to set explicitly in the UI ("folders become albums; the server decides its own file layout"), not a bug to fix. The Immich CLI's `--album` flag is the precedent.
12. Competitive risk: at least two third-party GUI sync clients already exist. The remaining differentiation is quality on keyring, bandwidth limits, failure classification, and UX — not feature presence.
13. Perceptual-duplicate handling: `bulk-upload-check` rejects on SHA-1 identity only, while `/duplicates` is Alpha. A user with visually-duplicate-but-byte-different files gets no warning in v1. Consider surfacing `/duplicates` later.

---

## Sources

Primary sources consulted, grouped by owner.

**Immich — API spec and docs**
- Immich OpenAPI spec, `main` (`3.3.0-rc.0`): https://raw.githubusercontent.com/immich-app/immich/main/open-api/immich-openapi-specs.json
- Immich OpenAPI spec, tag `v3.3.1`: https://raw.githubusercontent.com/immich-app/immich/v3.3.1/open-api/immich-openapi-specs.json
- API docs entry point: https://api.immich.app/introduction
- `POST /auth/login`: https://api.immich.app/endpoints/authentication/login
- API overview doc (generator only, no stability policy): https://github.com/immich-app/immich/blob/main/docs/docs/api.md
- Migrating to v3 (breaking changes): https://immich.app/blog/v3-migration
- Release v3.0.0 notes: https://github.com/immich-app/immich/releases/tag/v3.0.0
- Release v3.3.1: https://github.com/immich-app/immich/releases/tag/v3.3.1
- CLI docs (flags, watch, album, concurrency, API-key-in-plaintext): https://github.com/immich-app/immich/blob/main/docs/docs/features/command-line-interface.md
- Supported formats: https://github.com/immich-app/immich/blob/main/docs/docs/features/supported-formats.md
- Folder view: https://github.com/immich-app/immich/blob/main/docs/docs/features/folder-view.md
- XMP sidecars: https://github.com/immich-app/immich/blob/main/docs/docs/features/xmp-sidecars.md
- Storage template (server decides layout): https://github.com/immich-app/immich/blob/main/docs/docs/partials/_storage-template.md
- Jobs and workers: https://github.com/immich-app/immich/blob/main/docs/docs/administration/jobs-workers.md
- Reverse proxy example (`client_max_body_size`): https://github.com/immich-app/immich/blob/main/docs/docs/administration/reverse-proxy.md
- Scaling: https://github.com/immich-app/immich/blob/main/docs/docs/guides/scaling-immich.md
- Python upload guide (output format is stale vs current DTO): https://github.com/immich-app/immich/blob/main/docs/docs/guides/python-file-upload.md
- OAuth administration: https://docs.immich.app/administration/oauth/
- Discussion #5049, "[Feature] Windows & MAC OS Client" (closed duplicate): https://github.com/immich-app/immich/discussions/5049

**Immich — server source at tag `v3.3.1`**
- `auth.service.ts` (passwordLogin gate, opaque 32-byte token, constant-time bcrypt, `validate()`, permission enforcement): https://github.com/immich-app/immich/blob/v3.3.1/server/src/services/auth.service.ts
- `api-key.service.ts` (create/rotate, hashed secret, no expiry): https://github.com/immich-app/immich/blob/v3.3.1/server/src/services/api-key.service.ts
- `asset-media.service.ts` (quota check, dedup, job enqueue, `getUploadAssetIdByChecksum`, bulk check): https://github.com/immich-app/immich/blob/v3.3.1/server/src/services/asset-media.service.ts
- `middleware/asset-upload.interceptor.ts` (`x-immich-checksum` short-circuit): https://github.com/immich-app/immich/blob/v3.3.1/server/src/middleware/asset-upload.interceptor.ts
- `middleware/file-upload.interceptor.ts` (multer with no size limits, SHA-1 while streaming): https://github.com/immich-app/immich/blob/v3.3.1/server/src/middleware/file-upload.interceptor.ts
- `app.common.ts` (`json({limit:'10mb'})` — applies to JSON only): https://github.com/immich-app/immich/blob/v3.3.1/server/src/app.common.ts
- `repositories/session.repository.ts` (expiry + 90-day cleanup): https://github.com/immich-app/immich/blob/v3.3.1/server/src/repositories/session.repository.ts
- `utils/request.ts` (`fromChecksum` base64/hex heuristic): https://github.com/immich-app/immich/blob/v3.3.1/server/src/utils/request.ts
- `utils/mime-types.ts` (authoritative format list): https://github.com/immich-app/immich/blob/main/server/src/utils/mime-types.ts
- `enum.ts` (`Permission`, `QueueName`, `JobName`, `JobStatus`, `ImmichCookie`, `RouteKey`): https://github.com/immich-app/immich/blob/v3.3.1/server/src/enum.ts
- Issue #28154 (error messages changed to avoid leaking existence): https://github.com/immich-app/immich/issues/28154

**GPUI Kit / GPUI / Rust ecosystem**
- GPUI Kit site: https://gpui-kit.com/
- GPUI Kit docs index: https://gpui-kit.com/docs
- GPUI Kit full documentation dump (4.4 MB): https://gpui-kit.com/llms-full.txt
- GPUI Kit repo: https://github.com/longbridge/gpui-kit
- GPUI Kit `Cargo.toml` (`gpui-pre` exact pin + rationale): https://github.com/longbridge/gpui-kit/blob/main/Cargo.toml
- crates.io `gpui-kit`: https://crates.io/crates/gpui-kit
- crates.io `gpui-component`: https://crates.io/crates/gpui-component
- crates.io `gpui-pre` (weekly Zed snapshots): https://crates.io/crates/gpui-pre
- crates.io `gpui` (stale at 0.2.2, 2025-10-22): https://crates.io/crates/gpui
- crates.io `gpui-fast` (Longbridge's backend fork): https://crates.io/crates/gpui-fast
- crates.io `tray-icon`: https://crates.io/crates/tray-icon ; repo/README: https://github.com/tauri-apps/tray-icon
- crates.io `keyring` / `keyring-core` / per-platform stores: https://crates.io/crates/keyring , https://crates.io/crates/keyring-core , https://crates.io/crates/windows-native-keyring-store , https://crates.io/crates/apple-native-keyring-store , https://crates.io/crates/dbus-secret-service-keyring-store , https://crates.io/crates/linux-keyutils-keyring-store ; README: https://github.com/open-source-cooperative/keyring-rs
- crates.io `rusqlite`, `sqlx`, `reqwest`, `tokio`, `notify-rust`, `governor`, `backon`, `tokio-retry`, `reqwest-retry`, `auto-launch`, `single-instance`: https://crates.io/crates/rusqlite , https://crates.io/crates/sqlx , https://crates.io/crates/reqwest , https://crates.io/crates/tokio , https://crates.io/crates/notify-rust , https://crates.io/crates/governor , https://crates.io/crates/backon , https://crates.io/crates/tokio-retry , https://crates.io/crates/reqwest-retry , https://crates.io/crates/auto-launch , https://crates.io/crates/single-instance
- Rust packaging + tray/menu (added 2026-10-12): `cargo-packager` https://crates.io/crates/cargo-packager , `cargo-wix` https://crates.io/crates/cargo-wix , `muda` https://crates.io/crates/muda , `tray-icon` https://crates.io/crates/tray-icon
- Zed on Windows — GPUI in production on Windows: https://zed.dev/windows , https://zed.dev/docs/windows
- GPUI Kit open Windows issues: #1866 https://github.com/longbridge/gpui-kit/issues/1866 , #1909 https://github.com/longbridge/gpui-kit/issues/1909 , #2018 https://github.com/longbridge/gpui-kit/issues/2018 , #2496 https://github.com/longbridge/gpui-kit/issues/2496 , #3008 https://github.com/longbridge/gpui-kit/issues/3008 , #3381 https://github.com/longbridge/gpui-kit/issues/3381
- Zed (GPUI upstream): https://github.com/zed-industries/zed ; https://gpui.rs
- Inno Setup signed-uninstaller workflow: https://jrsoftware.org/ishelp/topic_setup_signeduninstaller.htm

**Wails 3 / Go ecosystem**
- Wails v3 docs home: https://v3.wails.io/
- Wails v3 docs index for tools: https://v3.wails.io/llms.txt
- Wails v3 Beta announcement (stability posture, alpha history, milestones): https://v3.wails.io/blog/wails-v3-beta/
- Installation (Go 1.25+, WebView2 preinstalled on Win10/11): https://v3.wails.io/quick-start/installation/
- Windows packaging (NSIS, MSIX, WebView2 bootstrapper, signing, ARM64): https://v3.wails.io/guides/build/windows/
- Single instance (`SingleInstanceOptions`): https://v3.wails.io/guides/single-instance/
- Menus + system tray (`app.SystemTray.New()`): https://v3.wails.io/reference/menu/
- Application reference (incl. `SystemTray`): https://v3.wails.io/reference/application/
- System tray implementation: https://github.com/wailsapp/wails/blob/v3.0.0-beta.28/v3/pkg/application/systemtray.go
- v2→v3 migration: https://v3.wails.io/migration/v2-to-v3/
- Repo + releases: https://github.com/wailsapp/wails ; https://github.com/wailsapp/wails/releases/tag/v3.0.0-beta.28 ; https://github.com/wailsapp/wails/releases/tag/v3.0.0-beta.1
- pkg.go.dev `zalando/go-keyring` (limits; `ErrSetDataTooBig`): https://pkg.go.dev/github.com/zalando/go-keyring ; README: https://github.com/zalando/go-keyring ; Windows impl: https://github.com/zalando/go-keyring/blob/master/keyring_windows.go
- pkg.go.dev `modernc.org/sqlite` (CGo-free, fragile libc pin, SQLite 3.53.4): https://pkg.go.dev/modernc.org/sqlite
- pkg.go.dev `fsnotify` (backends; FSEvents/USN not implemented): https://pkg.go.dev/github.com/fsnotify/fsnotify ; README: https://github.com/fsnotify/fsnotify
- pkg.go.dev `golang.org/x/time/rate` (token bucket, goroutine-safe): https://pkg.go.dev/golang.org/x/time/rate
- `github.com/cenkalti/backoff/v5`: https://pkg.go.dev/github.com/cenkalti/backoff/v5

**Standards / OS vendors / other first-party**
- RFC 9110 §15.5.10 (429 Too Many Requests): https://www.rfc-editor.org/rfc/rfc9110.html#name-429-too-many-requests
- Microsoft, Run and RunOnce Registry Keys: https://learn.microsoft.com/en-us/windows/win32/setupapi/run-and-runonce-registry-keys
- Microsoft, Task Scheduler: https://learn.microsoft.com/en-us/windows/win32/taskschd/task-scheduler-start-page
- D-Bus Secret Service specification: https://specifications.freedesktop.org/secret-service/latest/
- Tauri 2 System Tray guide: https://v2.tauri.app/learn/system-tray/
- crates.io `tauri-plugin-keyring` / `tauri-plugin-keyring-store`: https://crates.io/crates/tauri-plugin-keyring , https://crates.io/crates/tauri-plugin-keyring-store
- awesome-immich (community project list): https://github.com/tlwhittaker/awesome-immich
- immich-go (community importer): https://github.com/simulot/immich-go
- Third-party sync clients (vendor-published descriptions, not evaluated): https://immisync.astily.dev/ , https://apps.microsoft.com/detail/9mznhqkrw3qv
