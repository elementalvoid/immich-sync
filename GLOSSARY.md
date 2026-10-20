# immich-sync

An Immich desktop sync client for Windows. The client uploads files from user-selected folders to one Immich server. It runs in the background with a config window and a tray icon.

## Language

### Sync

**Source folder**:
A local directory that the user selected for upload.
_Avoid_: sync root, watched folder, library

**Sync item**:
One local file that the client tracks from discovery to upload result.
_Avoid_: job, task, file record

**Tracking**:
The client's record of the source folders it holds and the decisions the user has made about their items. One module owns it, so a signed-out client and a running engine write the same rows by the same rules.
_Avoid_: library, registry, folder list

**Reconciliation scan**:
A full read of a source folder. The client uses it to find changes that watch events missed.
_Avoid_: rescan, poll

**Album mapping**:
A per-folder rule that copies a mapped folder's name to the name of one Immich album. One album belongs to one folder: two folders that share a name are two albums.
_Avoid_: folder sync, album mode

**Album scope**:
The user's choice for one mapped source folder: one album for the whole source folder, or one album per folder that directly contains files.
_Avoid_: album mode, album structure

**Duplicate**:
A sync item whose content already exists on the server.
_Avoid_: existing file, clash

**Rename**:
A missing file and a new file with the same content and size. The client keeps the asset mapping and raises no review item.
_Avoid_: move, path change

**Version**:
The content of a sync item at one point in time. Each version is a separate asset, and the client keeps the versions in one stack.
_Avoid_: revision, copy

**Missing file**:
A sync item whose local file is no longer present. The client asks the user what to do with the server copy.
_Avoid_: deleted file, orphan

**Deletion review**:
The list in the config window where the user decides the server fate of each missing file. The client changes nothing on the server before a decision.
_Avoid_: deletion queue, cleanup list

### Upload

**Upload attempt**:
One try to send one sync item to the server.
_Avoid_: transfer, job

**Retriable error**:
A failure that can succeed on a later attempt, such as a timeout or a 503 response.
_Avoid_: soft error, temporary failure

**Permanent error**:
A failure that cannot succeed on a later attempt without a change, such as an unsupported format or a bad credential.
_Avoid_: hard error, fatal error

**Dead letter**:
The state of a sync item after the client stops its automatic retries. The user can retry the item by hand.
_Avoid_: failed queue, DLQ

**Bandwidth limit**:
The user-set maximum number of upload bytes per second.
_Avoid_: speed cap, throttle

**Concurrency limit**:
The user-set maximum number of uploads that may run at one time.
_Avoid_: parallelism, workers

### Immich

**Asset**:
One media object on the Immich server.
_Avoid_: photo, file, media

**Album**:
An Immich collection of assets. It is the closest server-side match for a local folder.
_Avoid_: collection, gallery

**Sidecar**:
An XMP file that carries metadata for one asset.
_Avoid_: metadata file

**Live photo pair**:
A photo asset and its linked video asset.
_Avoid_: motion photo, burst

**Stack**:
An Immich group of related assets. The timeline shows one stack entry.
_Avoid_: group, collection

### Credentials

**Session token**:
The credential that the Immich login endpoint returns. The server can revoke it or delete it at any time.
_Avoid_: access token, bearer token, JWT

**API key**:
A long-lived Immich credential with a fixed permission list.
_Avoid_: token, secret

**Credential store**:
The operating-system secret store. On Windows this is the Credential Manager.
_Avoid_: vault, password file

### Shell

**Config window**:
The main window. The user sets the server, the credentials, the source folders, the limits, and the deletion review here.
_Avoid_: settings dialog, preferences

**Tray icon**:
The status icon in the notification area. It shows sync state and it opens the config window.
_Avoid_: system tray, notification icon

**Folder status**:
The sync state of one source folder. The dashboard shows one card per source folder.
_Avoid_: folder state, folder progress

### Diagnostics

**Client version**:
The package version and the short source revision the running binary was built from, for example `0.1.0 (abc1234)`. Diagnostics and the log's start line both report it, and a build outside a git repository reports `unknown` in place of the revision.
_Avoid_: app version, build version

**Server version**:
The version the connected Immich server reports. Diagnostics shows it beside the client version, so a support session can tell which side a fault is on.
_Avoid_: Immich version, API version

**Debug bundle**:
One archive Diagnostics collects for a support message: the logs, versions, database information, configuration, and sizing.
_Avoid_: support bundle, log bundle, diagnostics archive
