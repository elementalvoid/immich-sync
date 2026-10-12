# File changes create a new version with copied associations

Status: accepted
Date: 2026-10-12

When a tracked file changes in place, the client uploads the new bytes as a new asset. It then calls `PUT /assets/copy` with the old asset as source and the new asset as target. That call moves album memberships, shared links, favorite state, sidecar, and stack membership across. If the old asset is in no stack, the client calls `POST /stacks` with the new asset first. The old version stays. We chose this because Immich has no byte-replacement endpoint, and a plain re-upload loses share state. The cost is version chains and stack management in the state DB.

## Considered Options

- **Plain new asset.** Rejected: albums and shared links would be lost or redone by hand.
- **`/assets/{id}/edits`.** Rejected: it only applies crop, rotate, and mirror recipes to one asset. It accepts no new bytes.
- **Delete and re-upload.** Rejected: the asset id changes, so all share state is lost.

## Consequences

- The state DB stores the current asset id, the version chain, and the stack id per sync item.
- The client needs `asset.copy`, `asset.update`, and `asset.upload` permissions on the credential.
- The client applies the old asset's tags to the new version with the tag endpoints. That work needs the `tag.asset` permission.
- The copy endpoint does not transfer faces, descriptions, or custom metadata. Those stay on the old version.
- The timeline shows one stack entry per sync item.
- A user who edits files often creates new assets. A per-folder option to disable versioning is possible later.
