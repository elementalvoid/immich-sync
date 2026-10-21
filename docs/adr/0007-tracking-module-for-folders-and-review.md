# Folder edits and review decisions go through one tracking module

Status: accepted
Date: 2026-10-20

The client edits source folders and records keep, trash, and retry decisions
while it is signed out, and the sync engine does the same work while it runs.
Those were two implementations of one rule set: the shell chose between
`Engine` and `StateStore` at every call site, and the signed-out album edit
compared paths byte for byte where the engine compared path keys, so a folder
spelled differently was refused with "That folder is not in the client."

The rules now live in `sync_core::tracking::Tracking`, which always exists once
the store opens. The engine attaches an observer and follows the same writes;
with no engine attached the observer list is empty and the changes only reach
the store. The folder counts stay with the engine: they are a hot-path cache for
a status read that must not touch SQLite, and their update hooks are engine
internals.

## Considered Options

- **Let `Engine` exist without a session.** Rejected: it pushes
  `Option<SharedSession>` through the uploader, the album mapper, and the
  versioner, and turns "signed out" into a mode inside the upload engine.
- **Free functions in sync-core, with the engine delegating.** Rejected: the
  rules would live once, but the shell would still choose an entry point per
  operation, so a new operation can pick the wrong one.
- **Keep the state store as the only interface.** Rejected: the rules that keep
  a folder row whole (the album fields a re-add must carry, the path key, the
  answer for an unknown folder) would spread across call sites again.

## Consequences

- The engine no longer writes source folder rows or review decisions; it reacts
  to the tracking changes. `Engine::add_folder`, `set_album_mapping`,
  `set_album_scope`, `remove_folder`, `keep_review_item`, and `retry_failed`
  delegate.
- An observer is registered when the engine is built and removed when it is
  dropped. The list is not a single slot: a start that loses its race is
  stopped and dropped while the winner keeps observing.
- A change is announced after the write lock is released, so an observer may
  call back into tracking. Nothing assumes the notifications arrive in store
  order: the engine reads the store row under the lock it applies it with, so
  the last callback to run always applies the newest state.
- The trash action stays on the engine, because it needs the session; it keeps
  its "sign in first" answer.
- Folder counts are not part of the tracking interface. The status derive still
  owns them.
