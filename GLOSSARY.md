# immich-sync

An Immich desktop sync client for Windows. The client uploads files from user-selected folders to one Immich server. It runs in the background with a config window and a tray icon.

## Language

### Sync

**Source folder**:
A local directory that the user selected for upload.
_Avoid_: sync root, watched folder, library

**Owner**:
The one source folder a sync item belongs to: the first source folder in the client's folder list that contains the item's path. That folder's card counts the item and its traffic meter counts its bytes, so a nested folder owns only the files no folder before it contains.
_Avoid_: parent folder, matching folder

**Sync item**:
One local file that the client tracks from discovery to upload result.
_Avoid_: job, task, file record

**Unsupported file**:
A file whose extension is not one Immich accepts. The client never queues it: it is not a sync item, has no attempt, and never becomes a failure. The folder detail shows how many the last scan of that folder refused, and the count is stored with the folder so a restart keeps it.
_Avoid_: invalid file, rejected file, bad format

**Tracking**:
The client's record of the source folders it holds and the decisions the user has made about their items. One module owns it, so a signed-out client and a running engine write the same rows by the same rules.
_Avoid_: library, registry, folder list

**Reconciliation scan**:
A full read of a source folder. The client uses it to find changes that watch events missed.
_Avoid_: rescan, poll

**Album mapping**:
A per-folder rule that creates one Immich album for the folder. The album's title defaults to the folder's name and can be changed. One album belongs to one folder: two folders that share a name are two albums.
_Avoid_: folder sync, album mode

**Album title**:
The user-visible name of one mapped album. The source folder's own album takes the user's title, which defaults to the folder's name and can be changed after the album exists. A leaf folder's album takes the leaf's path relative to the source folder, optionally starting with the source folder's album title, or the leaf folder's name alone. Changing a title renames the Immich album.
_Avoid_: album name, title override

**Album scope**:
The user's choice for one mapped source folder: one album for the whole source folder, or one album per folder that directly contains files. Under the per-folder choice, the folder's two naming choices title the albums it makes.
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

**Upload throughput**:
The rate at which upload bodies move bytes to the server, measured over a short rolling window.
_Avoid_: speed, transfer rate

**Upload in flight**:
An upload request that has started and has not finished. Hashing, indexing, and the server duplicate check are not uploads in flight.
_Avoid_: active upload, running transfer

**Sync tag**:
The one Immich tag, `immich-sync`, that the client adds to the assets it syncs so they can be found with Immich's tag search. It is on by default; the user can turn it off in Settings. While it is on, the client tags new uploads and backfills the tag onto the assets it uploaded before.
_Avoid_: upload tag, source tag

**Backfill**:
The pass that adds the sync tag to the assets the client uploaded before the setting was turned on: every current asset, every old version, and every duplicate, a batch of ids per request. Its progress and completion are recorded, so a restart resumes it instead of starting over.
_Avoid_: re-tag, scan

### Immich

**Asset**:
One media object on the Immich server.
_Avoid_: photo, file, media

**Asset link**:
The Immich web address of one uploaded asset, opened in the default browser.
_Avoid_: photo URL, deep link

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
The operating-system secret store. On Windows this is the Credential Manager, on
macOS the login Keychain, and on Linux the Secret Service.
_Avoid_: vault, password file

### Shell

**Config window**:
The main window. The user sets the server, the credentials, the source folders, the limits, and the deletion review here.
_Avoid_: settings dialog, preferences

**Tray icon**:
The status icon in the notification area. It shows sync state and it opens the config window.
_Avoid_: system tray, notification icon

**Pause**:
The user's hold on new uploads, from the dashboard control or the tray menu. It is separate from the known-network hold, so resuming never overrides the list.
_Avoid_: stop, disable sync

**Folder status**:
The sync state of one source folder. The dashboard shows one card per source folder, and the folder detail renders the same status in full.
_Avoid_: folder state, folder progress

**Folder setup**:
The dialog that confirms a new source folder's recursion and album settings. The client records and scans the folder only when the user confirms.
_Avoid_: add dialog, onboarding

**Folder detail**:
The enlarged view of one source folder: its full path, live state and counts, its own upload activity, settings, timestamps, and the list of the assets it has uploaded, with links into the Failed and Review views filtered to it. Its Remove needs a confirmation, because the folder's uploaded assets stay on the server.
_Avoid_: folder modal, folder popup

**Known networks**:
The user's list of network names for the **Sync only on known networks** setting, compared against the name the OS reports for the current wireless network. A wired link always syncs, because it has no name to match. The list is off by default.
_Avoid_: allowed networks, trusted networks, whitelist

**Network check**:
The client's one read of the OS's current network, through a **network source** per platform. It decides whether uploads are held, and its answer is what the Settings page shows as the current network.
_Avoid_: SSID lookup, network probe, reachability check

**Network source**:
The platform backend that answers the network check: NetworkManager over D-Bus on Linux, the Network List Manager on Windows, and CoreWLAN with CaptiveNetwork on macOS. It also reports the OS's network-change events, which the client uses to re-check before the 30-second fallback poll.
_Avoid_: provider, adapter, detector

**Pause reason**:
Why uploads are held: the user's own **pause**, or the known-network list. Both read as the paused activity state; the dashboard and the tray tooltip phrase the reason.
_Avoid_: pause kind, pause cause, hold type

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

**Live tail**:
The Logs view's automatic reading of new lines from the newest log file. Following pauses while the user reads older lines or holds a selection.
_Avoid_: auto-refresh, follow mode
