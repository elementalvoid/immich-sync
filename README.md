# Immich Sync

A tray application that uploads files from folders you choose to one
[Immich](https://immich.app/) server. It watches the folders, uploads changes as
they happen, and scans again on a schedule so nothing is missed. Upload only: it
never deletes server-side assets on its own, and a local file that disappears
waits in a review list until you decide what happens to the server copy.

v1 ships for Windows 10/11 (x64) and Linux desktops (deb and AppImage). The
Windows binaries are **unsigned** for v1, so the installer shows a SmartScreen
warning (see [Troubleshooting](#troubleshooting)). macOS is not part of v1.

The requirements this build implements are in [SPEC.md](SPEC.md); the milestone
plan is in [PLAN.md](PLAN.md).

## Install

Download the packages from the
[releases page](https://github.com/elementalvoid/immich-sync/releases). Each
release carries, for version `<version>`:

| File | Platform |
| --- | --- |
| `immich-sync_<version>_x64-setup.exe` | Windows 10/11 (x64) installer |
| `immich-sync_<version>_amd64.deb` | Debian/Ubuntu-family Linux |
| `immich-sync_<version>_x86_64.AppImage` | Any Linux desktop |
| `SHA256SUMS` | Checksums for all three packages |

### Windows 10/11

1. Run `immich-sync_<version>_x64-setup.exe`.
2. Windows SmartScreen shows "Windows protected your PC" because the installer
   is not code-signed. Click **More info**, then **Run anyway**. Code signing is
   planned for the public release; until then this warning is expected and is
   not a sign the download was altered. The checksums below are how you check
   that.
3. The installer is per-user: it needs no administrator password, installs to
   `%LOCALAPPDATA%\Immich Sync`, and adds Start Menu and desktop shortcuts. It
   can start the app when it finishes.

Uninstall from **Settings → Apps → Immich Sync**. Settings, state, and logs
(see [Where everything lives](#where-everything-lives)) are kept; delete those
folders yourself for a complete reset.

### Linux (deb)

```sh
sudo apt install ./immich-sync_<version>_amd64.deb
```

`apt` pulls in the runtime libraries the package declares. Start the app from
your desktop's application grid or run `immich-sync`. The package installs
`/usr/bin/immich-sync`, a desktop entry, and the app icon.

Remove it with `sudo apt remove immich-sync`.

### Linux (AppImage)

```sh
chmod +x immich-sync_<version>_x86_64.AppImage
./immich-sync_<version>_x86_64.AppImage
```

The AppImage needs FUSE 2 to mount itself (`libfuse2t64` on Ubuntu 24.04). On a
system without FUSE, run it extracted instead:

```sh
./immich-sync_<version>_x86_64.AppImage --appimage-extract-and-run
```

The desktop needs a Vulkan-capable driver for the window; a desktop install
already has one.

## Verify the download

### Checksums

`SHA256SUMS` lists the SHA-256 of every package in the release. Put it in the
same directory as the downloaded packages.

Linux:

```sh
sha256sum -c SHA256SUMS
```

Windows PowerShell:

```powershell
Get-FileHash .\immich-sync_<version>_x64-setup.exe -Algorithm SHA256
```

Compare the printed hash with the line for that file in `SHA256SUMS`.

### Build provenance

Once the repository is public, each package also carries a build provenance
attestation, and you can check that it was built by this repository's release
workflow:

```sh
gh attestation verify immich-sync_<version>_amd64.deb --repo elementalvoid/immich-sync
```

While the repository is private the attestation steps are disabled (the
`RELEASE_ATTEST` repository variable in the release workflow), so this command
has nothing to verify yet. See [Releases](#releases) for how it is switched on.

## First run

On first launch the window opens on the **Dashboard**.

### Sign in

1. Open the **Settings** tab, under **Connection**.
2. Enter the server URL (for example `https://immich.example.com`), the email
   address of the Immich account, and either the password or an API key.
   Certificate verification stays on; a self-hosted server behind a private CA
   needs that CA in the operating system's certificate store.
3. Click **Sign in**.

The credential is stored in the operating system's credential store — Windows
Credential Manager, or the Secret Service (GNOME Keyring or KWallet) on Linux.
It is never written to the settings file or to the log. Later launches sign in
again from that stored credential, without asking. A wrong password shows an
error in the window and does not crash the client; the connection shows as
"Needs attention" in the window and the tray tooltip.

If the server has password login disabled, choose **API key** instead and paste
a key from Immich's account settings.

### Add folders

1. On the **Dashboard**, type or paste a folder path into **Add a folder to
   sync** and click **Add folder**.
2. The folder is watched recursively, and a status card shows its activity
   (idle, syncing, paused, needs attention), upload progress, and item counts.
3. **Album mapping** is off by default. Switch it on for a folder to put its
   uploads into an album, and choose the scope: **One album per folder** (one
   album per leaf folder), or **One album for this folder**.
4. **Remove** on a folder card stops syncing that folder. Nothing is deleted
   from the server.

Hidden files and folders, symlinks, temporary/partial files, and system files
are skipped by design. A source folder that is unplugged shows as unavailable on
its card instead of producing one error per file.

## Using the app

- **Tray.** The tray icon is fixed; its tooltip shows the activity (idle,
  syncing, or paused), the connection state (including needs attention), the
  review count, upload progress, and the last error. The tray menu has
  **Show**, **Pause**, **Resume**, **Sync now**, and **Quit**.
- **Window.** Closing the window hides it; the client keeps running in the tray.
  **Quit** exits the client. Starting the app again focuses the running
  instance instead of starting a second one.
- **Views.** *Dashboard* (status and folders), *Upload* (send one file now to
  test the pipeline), *Failed items* (files the client gave up on, with
  **Retry**), *Review* (see below), *Settings*, *Logs*, *Diagnostics*.
- **Deletion review.** A tracked local file that disappears appears in
  **Review**; nothing changes on the server until you choose **Keep** or
  **Trash** for it. A rename is recognized by content and is not a deletion.
- **Settings.** Changes save immediately. General holds **Autostart at login**
  (off by default), Limits holds the bandwidth limit (0 means unlimited),
  concurrent uploads, the reconciliation scan interval, and the retry attempt
  cap.
- **Diagnostics.** Server version and permissions, the state database and its
  integrity, the last error per class, throughput, and a **Rebuild** action that
  starts the state over and rescans every folder.
- **Logs.** The tail of the log file and the level control. The log never
  contains the password, API key, or session token.
- The theme follows the system theme.

## Where everything lives

All paths are per user. `%APPDATA%` and `%LOCALAPPDATA%` are the usual Windows
roaming and local application-data directories; the Linux paths follow the XDG
base directories (`~/.config`, `~/.local/share`).

| What | Windows | Linux |
| --- | --- | --- |
| Settings (`settings.toml`) | `%APPDATA%\immich-sync\` | `~/.config/immich-sync/` |
| State database (`state.db`) and logs | `%LOCALAPPDATA%\immich-sync\` | `~/.local/share/immich-sync/` |
| Autostart entry (when enabled) | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` | `~/.config/autostart/immich-sync.desktop` |

The log file is `immich-sync.log.<date>`, one file per day, five kept. The state
database stays on this machine by design: never put it on a network share or in
a cloud-sync folder. The **Settings** view's *Storage* card shows the resolved
paths.

## Troubleshooting

### "Windows protected your PC" when installing (SmartScreen)

Expected: v1 Windows binaries are unsigned. Click **More info → Run anyway**.
Verify the download with `SHA256SUMS` first if the warning worries you. Signing
arrives with the public release.

### The app seems to do nothing after launch

It starts in the tray. Double-click the tray icon, or start the app again to
bring the window to the front. If there is no tray icon, see the Linux note
below; the window is still running.

### No tray icon on Linux (GNOME)

GNOME does not show StatusNotifierItem icons without an AppIndicator extension.
Install one such as "AppIndicator and KStatusNotifierItem Support" and restart
the session. Without a tray the client still runs, but because there is no
**Quit** menu item, closing the window exits the client.

### Sign-in fails with a credential-store message (Linux)

The password and API key are stored in the Secret Service, so a Secret Service
provider (GNOME Keyring, KWallet) must be running and unlocked. Install or start
one, then sign in again; the client retries the credential store on every
sign-in.

### The AppImage will not start

Install FUSE 2 (`libfuse2t64` on Ubuntu 24.04) or run the file with
`--appimage-extract-and-run`.

### Sync does not pick up a new file

Hidden files, symlinks, temporary files, and system files are skipped by
design, and a file inside a folder that is not added is not tracked. Check the
folder card and **Diagnostics**; if the watcher missed a change, the next
reconciliation scan (30 minutes by default) finds it, or click **Sync now** in
the tray.

### A file keeps failing

Open **Failed items**: the reason is listed per file. Fix the cause (for
example an unsupported format or a missing server permission) and click
**Retry**.

### The settings file was damaged

A settings file that cannot be parsed is left in place, the defaults are used,
and the Settings view shows what happened. Fix or delete the file and change a
setting to write a fresh one.

### Logs for a bug report

Open the **Logs** view or read the file listed in
[Where everything lives](#where-everything-lives). Raise the level to `Debug`
while reproducing a problem, then set it back. Credentials are redacted, so the
log is safe to attach.

## Releases

For maintainers.

- **Trigger.** Pushing a tag `v<version>` builds and publishes a GitHub release.
  The version must equal `version` in `crates/app/Cargo.toml`; the workflow
  fails the tag before building when it does not. **Run workflow** (manual
  dispatch) builds the same packages without publishing a release.
- **What is built.** `cargo-packager` (version pinned in the workflow) builds
  the NSIS installer on `windows-latest` and the deb and AppImage on
  `ubuntu-24.04`. The packager configuration lives in
  `crates/app/Cargo.toml` under `[package.metadata.packager]`.
- **Checksums.** A final job downloads both platforms' packages and generates
  `SHA256SUMS` from the real files; nothing is hand-written.
- **Provenance.** The workflow attests each platform's packages with
  `actions/attest-build-provenance`, gated on the repository variable
  `RELEASE_ATTEST`. Set `RELEASE_ATTEST` to `true` under **Settings → Secrets
  and variables → Actions → Variables** once the repository is public (ticket
  #16). While it is unset the attestation steps are skipped and the release
  workflow stays green.
- **Signing.** v1 is unsigned. Windows code signing is the public-release
  ticket, and is the step that removes the SmartScreen warning above.

## Development

```sh
cargo run -p immich-sync
```

The workspace gates are `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, and
`cargo test --workspace`; `.github/workflows/ci.yml` runs them on every push.
