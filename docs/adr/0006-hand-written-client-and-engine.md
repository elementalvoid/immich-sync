# Hand-written Immich client and sync engine

Status: accepted
Date: 2026-10-12

The Immich client is hand-written for the Stable operations the app uses, and the sync engine is hand-written on component crates. We tested generation: `cargo progenitor` fails on the v3.3.1 OpenAPI spec with `UnexpectedFormat("unexpected content type: multipart/form-data")`. `paperclip` self-describes as not ready for production. No Rust Immich client crate is maintained for v3. No whole sync engine can target a third-party HTTP API. Job-queue frameworks conflict with rusqlite or add nothing: apalis-sqlite pulls sqlx and is an RC, sqlxmq is Postgres-only, and background-jobs is AGPL with no SQLite backend. The durable queue is a SQLite table plus a worker loop, so queue state and item state are transactional together.

## Considered Options

- **Generate the client.** Rejected: the generator fails on the upload operation, which is the core operation.
- **Embed apalis-sqlite.** Rejected: a second SQLite stack, RC status, and the queue would no longer share transactions with item state.
- **Embed a whole sync engine.** Rejected: no candidate targets a third-party HTTP upload API.

## Consequences

- The client covers `POST /auth/login`, `POST /auth/validateToken`, `GET /server/about`, `POST /assets/bulk-upload-check`, `POST /assets`, `PUT /assets/copy`, `POST /stacks`, tag endpoints, and `DELETE /assets`.
- Component crates: `notify`, `notify-debouncer-full`, `ignore`, `sha1`, `base64`, `reqwest`, `backon`, `tokio`, `rusqlite`.
- Bandwidth shaping is a hand-written byte bucket over the body stream. `governor` remains a valid alternative through its weighted-cell API.
- If the API surface grows far beyond these operations, revisit generation on a pruned spec.
