# Upload-only sync with a confirmation-gated deletion review

Status: accepted
Date: 2026-10-12

The client uploads local files to Immich. It does not download, and it never changes server data on its own. When a tracked local file disappears, the client records it as a missing file and shows a deletion review with a per-file choice. The server copy changes only after the user confirms. We chose this because Immich has no stable download-change feed, and because automatic delete propagation is the fastest way to lose a photo library.

## Considered Options

- **Upload-only, ignore deletions.** Smallest v1. Rejected: the user wants to know when files disappear.
- **Upload-only with automatic delete propagation.** Rejected: silent data loss risk.
- **Two-way sync.** Rejected for v1: the API does not track download-side changes with enough fidelity.

## Consequences

- The review offers two actions: keep on server, or move the asset to trash. v1 has no permanent delete.
- The state model needs a missing-file state and a review queue.
- The tray needs a status indicator for items that wait for a decision.
- The trash action needs a delete permission on the credential. The app must show a clear message when the credential lacks it.
- A rename is detected by content hash and size. The client keeps the asset mapping, and no review item appears.
