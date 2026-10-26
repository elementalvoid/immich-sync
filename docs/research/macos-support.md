# macOS support: what it takes

**Date:** 2026-10-26
**Status:** research complete; recommendation at the end.
**Decision:** [ADR 0008](../adr/0008-macos-unsigned-distribution.md) chooses unsigned,
unnotarized packages and no Apple Developer Program account. The signing and
notarization material below is retained as the alternative and as background.

## Bottom line

macOS is a **port, not a rewrite**. The runtime stack is already portable: GPUI's
macOS backend is Zed's primary platform, `gpui-tray` 0.1.4 ships an AppKit status-item
backend, `notify` uses FSEvents by default, SQLite is bundled, and TLS already uses the
system trust store through `rustls-platform-verifier`. Nothing needs a web view, a
runtime, or a Homebrew dependency.

What is missing is narrower than the platform list suggests:

1. **Five platform seams in the app**: the Keychain store, autostart, "Open folder",
   the menu bar/reopen behaviour, and the menu-bar icon. Plus one latent bug: macOS
   currently gets case-sensitive path keys, which is wrong on a default Mac volume.
2. **Packaging and release**: a `.app` + `.dmg` and a macOS CI job. `cargo-packager`
   0.11.8 already builds both; it can sign and notarize too, but ADR 0008 ships
   unsigned because no Apple Developer Program account will be created.
3. **Real-hardware verification**: Apple Silicon and Intel, TCC prompts, an external
   drive, login-item approval, sleep/wake, and an upgrade over an installed copy.

Estimate: **2–3 weeks of focused engineering**. Without notarization there is no
Apple enrolment, certificate, or notary onboarding; the long pole is the unproven
`gpui-kit`/`gpui-tray` combination and real-hardware verification. Dropping Intel
removes another day or two.

Chosen scope (ADR 0008): **macOS 13 (Ventura) or later, arm64 and x86_64, unsigned
`.dmg`, direct distribution**. Not the Mac App Store, not sandboxed, not macOS
10.15–12. The Gatekeeper warning is accepted; notarization is unavailable without an
Apple Developer Program account.

## Method

Every external claim below was checked against a primary source retrieved on
**2026-10-26**: Apple developer documentation and support pages, crates.io/docs.rs
metadata, upstream crate source at the versions in this repo's `Cargo.lock`, and the
`cargo-packager` 0.11.8 source. The repository claims carry `file:line` references.
No macOS host was available, so **no build or run was performed**; the "unverified"
list at the end is explicit about that.

## 1. Where the stack already stands

| Component | Version here | macOS today | Evidence |
|---|---|---|---|
| `gpui-pre` (Zed's GPUI) | 0.3.8 | **Works.** `gpui-pre-macos` + `gpui-pre-apple` implement AppKit/Metal; this is Zed's primary platform. | `Cargo.lock`; [gpui-pre](https://crates.io/crates/gpui-pre) |
| `gpui-kit` / `gpui-component` | 0.7.2 | **Advertised, untested here.** "Ship one Rust codebase to macOS, Windows, and Linux." | crate README |
| `gpui-tray` | 0.1.4 | **Works.** AppKit `NSStatusBar`/`NSStatusItem` backend in the pinned crate. | `gpui-tray-0.1.4/src/backend/macos.rs`; [README](https://github.com/domenkozar/gpui-tray) |
| `notify` | 8.2 | **Works.** FSEvents is the default macOS backend (`macos_fsevent`). | `notify-8.2/Cargo.toml`; [docs.rs](https://docs.rs/notify/8.2.0/notify/) |
| `notify-debouncer-full` | 0.7 | **Works.** Its default feature set includes `macos_fsevent`. | crate `Cargo.toml` |
| `rusqlite` | 0.40, `bundled` | **Works.** SQLite compiles from the amalgamation with clang. | ADR 0005; crate docs |
| `reqwest` + `rustls-platform-verifier` | 0.13.5 | **Works.** The lock already carries `security-framework`; a private CA in the Keychain validates. | `Cargo.lock` |
| `keyring-core` | `=1.0.1` | **Missing store.** Needs `apple-native-keyring-store` 1.0.2. | §3.1 |
| `interprocess` | 2.4.5 | **Works.** On macOS, `GenericNamespaced` maps to a `/tmp/<name>` socket. | crate `local_socket/name/type.rs` |
| `directories` | 6.0.0 | **Works.** Config and state both resolve under `~/Library/Application Support`. | crate `src/mac.rs` |
| Autostart | — | **Missing.** macOS hits `UnavailableAutostart`. | `crates/app/src/autostart.rs:65` |
| Notifications | — | **Works from a `.app` only.** `gpui-pre` uses `UNUserNotificationCenter` and disables itself when the binary has no bundle identifier. | `gpui-pre-macos/src/system_notifications.rs` |
| Packaging/CI | — | **Missing.** No macOS job, no `.dmg`, no signing. | `.github/workflows/` |

### Runtime shape

- **Build host:** macOS 13+ with Xcode and the Command Line Tools. GPUI compiles its
  Metal shaders at build time with `xcrun metal` (`gpui-pre-apple/build.rs`). On
  macOS 26 the Metal toolchain can be a separate downloadable component; Zed's own
  build docs say to run `xcodebuild -downloadComponent MetalToolchain` when `xcrun
  metal` is missing ([Zed macOS build guide](https://github.com/zed-industries/zed/blob/main/docs/src/development/macos.md)).
- **Target machine:** any Mac that runs macOS 13+. Rendering is Metal-only; GPUI exits
  with status 1 if it cannot get a Metal device (`gpui_apple/src/metal_renderer.rs`).
  There is no software fallback, so a GPU-less VM is not a test environment.
- **Deployment floor:** GPUI's shader build hardcodes `-mmacosx-version-min=10.15.7`
  (`gpui-pre-apple/build.rs`), and Zed supports macOS 13+ fully
  ([Zed install docs](https://github.com/zed-industries/zed/blob/main/docs/src/installation.md)).
  A 13.0 floor is the practical choice; 10.15–12 buys nothing and complicates autostart.
- **Bundle:** a real `.app` is required for notifications (a bare `cargo run` binary
  has no `CFBundleIdentifier` and notification setup is skipped, not crashed).
- **Self-contained:** no WebView, no .NET/Java, no dynamic library beyond the system
  frameworks. The `.app` is the whole install.

## 2. What must change in the app

| # | Change | Where | Why | Size |
|---|---|---|---|---|
| 1 | macOS Keychain store | `crates/app/src/credentials.rs:143`, `crates/app/Cargo.toml` | No credential store on macOS today; every operation reports "unsupported". | S |
| 2 | Autostart via `SMAppService` | `crates/app/src/autostart.rs:65` | R8.5 currently reports "not implemented" on macOS. | M |
| 3 | `open` for "Open folder" | `crates/app/src/folder_panel.rs:66-68` | The non-Windows branch runs `xdg-open`, which macOS does not have. | XS |
| 4 | Case-insensitive path keys on macOS | `crates/sync-core/src/paths.rs:29` | Default APFS is case-insensitive; today a case-only rename becomes delete + create, and a duplicate spelling becomes two rows. | XS + test |
| 5 | App menu, Dock reopen, tray icon | `crates/app/src/shell.rs` (`init`, `install_tray`, `icon`) | No menu bar means no Cmd+Q/Cmd+W/Edit; clicking the Dock icon after closing the window does nothing; the 32 px coloured icon with a text title is wrong for a 24 pt menu bar. | M |
| 6 | Rescan on directory/`MustScanSubDirs` events | `crates/sync-core/src/engine.rs:1204-1237`, `watch.rs` | FSEvents may drop/coalesce events and says "rescan the subtree"; the current watcher ignores directory paths, so the 30-minute scan is the only recovery. | S |
| 7 | Friendly TCC denial message | folder status rendering in `crates/app` | "Operation not permitted" is not actionable; macOS denial is permanent until the user changes System Settings. | S |
| 8 | macOS OS version in the debug bundle | `crates/app/src/debug_bundle.rs:242` | `os_release()` reads `/proc`; macOS reports only `macos`. Optional. | XS |
| 9 | Single-instance socket under a private path | `crates/app/src/instance.rs:84-95` | `GenericNamespaced` lands in world-writable `/tmp`; optional hardening, not a blocker. | S |

### 2.1 Credentials (Keychain)

`apple-native-keyring-store` 1.0.2 (2026-08-06) is the sibling of the Windows and
Linux stores already in use, same publisher, same `keyring-core` contract
([crates.io](https://crates.io/crates/apple-native-keyring-store)). It requires
`keyring-core ^1`, so the existing `=1.0.1` pin stays.

Use the **`keychain`** feature and `keychain::Store::new()`, which targets the
file-based login keychain. The `protected` module is the data-protection keychain and
needs a provisioning profile and entitlement; this app has neither
([README](https://github.com/open-source-cooperative/apple-native-keyring-store)).
The `keychain` module needs no entitlement and no provisioning profile.

```toml
[target."cfg(target_os = \"macos\")".dependencies]
apple-native-keyring-store = { version = "=1.0.2", features = ["keychain"] }
```

```rust
#[cfg(target_os = "macos")]
fn install_platform_store() -> Result<(), String> {
    let store = apple_native_keyring_store::keychain::Store::new()
        .map_err(|error| keychain_message(&error.to_string()))?;
    keyring_core::set_default_store(store);
    Ok(())
}
```

Add a `keychain_message` in the same style as `credential_manager_message`. Expect
keychain re-authorisation prompts after each **ad-hoc** rebuild, because the ACL is
tied to the code identity (Apple TN3137/TN3127); a stable Developer ID signature gives
the ACL a stable identity to match, which is what stops the repeats. That is a
packaging problem, not a code problem.

### 2.2 Autostart

`SMAppService.mainApp.register()` / `unregister()` is Apple's API for a macOS 13+
app that starts at login ([Apple](https://developer.apple.com/documentation/servicemanagement/smappservice/mainapp)).
It shows up in System Settings › General › Login Items, and the user can veto it —
`register()` returns `kSMErrorLaunchDeniedByUser`, which is a normal outcome the
settings view must display rather than an error to swallow. If the status is
`requiresApproval`, call `openSystemSettingsLoginItems()`.

Use `objc2-service-management` 0.3.2 (the objc2 project; the app already uses objc2
transitively through `gpui-tray`) or the narrower `smappservice-rs` 0.1.3. Do not
write a `~/Library/LaunchAgents` plist: it still works, but Apple documents it as the
pre-13 mechanism and it shows the signing organisation rather than the app name in
Login Items ([Apple](https://developer.apple.com/documentation/servicemanagement/updating-helper-executables-from-earlier-versions-of-macos)).
The existing `Autostart` trait is the seam; the macOS backend ignores `AppPaths`.

### 2.3 Open folder

`open_in_file_manager` uses `explorer` on Windows and `xdg-open` everywhere else.
macOS needs `open <path>`. One `#[cfg(target_os = "macos")]` arm.

### 2.4 Path case (latent duplicate-sync bug)

`PathCase::for_platform()` returns `Insensitive` only for Windows
(`crates/sync-core/src/paths.rs:29`), so macOS gets **sensitive** keys. APFS is
case-insensitive by default (case-sensitive is an option), and normalization-insensitive
in both variants ([APFS guide](https://developer.apple.com/library/archive/documentation/FileManagement/Conceptual/APFS_Guide/FAQ/FAQ.html);
[APFS reference](https://developer.apple.com/support/downloads/Apple-File-System-Reference.pdf)).

Consequences of the current behaviour on a default Mac:

- A case-only rename (`IMG_0001.JPG` → `img_0001.jpg`) looks like a deletion plus a
  new file: one review item and a new asset, instead of a rename.
- Two spellings of the same folder can become two `source_folder` rows, two watchers,
  and duplicate uploads.

Make macOS `Insensitive` like Windows. The honest limit: a case-sensitive APFS/HFSX
volume (developer setups, some external disks) can hold two files that differ only in
case; they would collapse into one key and one file would never sync. That is a
`shortcut:` worth stating in the code and the README. The correct long-term fix is a
per-folder case mode probed at folder-add, stored with the folder row; it touches the
schema and every `PathCase::for_platform()` caller, so it is not v1.

Unicode normalization is a smaller, separate issue: APFS compares filenames after
NFD decomposition, so the same file can be spelled NFC or NFD. All paths here come
from the filesystem itself (scan, watcher, picker), so the bytes are stable in
practice. Do not add `unicode-normalization` for v1; note it as a known limitation.

### 2.5 Menu bar, Dock reopen, tray icon

These are the visible macOS gaps, all in `crates/app/src/shell.rs`:

- **No app menu.** GPUI has no default menu bar; `setMainMenu_` is only called from
  `App::set_menus` (`gpui-pre-macos/src/platform.rs:1156-1165`). Without it there is no
  Cmd+Q, no Cmd+W, and no Edit menu for the text fields. Add a minimal app menu
  (About, Quit), Edit menu (Cut/Copy/Paste/Select All) and Window menu (Close,
  Minimize) on macOS.
- **Dock reopen does nothing.** `hide_window` removes the window
  (`shell.rs:234`); clicking the Dock icon calls `applicationShouldHandleReopen`, but
  `gpui-pre` only invokes a callback registered with `App::on_reopen`
  (`gpui-pre-macos/src/platform.rs:1460`). Register `show_window` there. Without it,
  a user who closes the window can only get it back from the tray.
- **Tray icon is wrong for a menu bar.** The icon is a 32×32 coloured circle
  (`shell.rs:726-739`), and the tray is built with `.title("Immich Sync")`
  (`shell.rs:620`). A macOS status item is 24 pt high and expects an ~18 pt
  **template** image (black + clear, auto-inverted for light/dark
  ([Apple](https://developer.apple.com/documentation/appkit/nsimage/istemplate))).
  `gpui-tray` 0.1.4 does not expose `setTemplate` and builds the `NSImage` from the
  PNG's pixel size with no Retina representation, so a 32 px icon renders oversized
  and a colour icon does not invert. Options, cheapest first:
  1. Ship an 18×18 monochrome icon, drop the title on macOS, accept no auto-inversion
     (visible in one of the two appearances).
  2. Ship an 18×18 coloured icon that reads on both appearances, drop the title.
  3. Upstream a `template`/scale flag to `gpui-tray` and use a 36 px template; this is
     the correct fix and a small patch.
  For v1 macOS, (2) plus an upstream issue is the lazy choice; (3) is the polish.

**Activation policy decision.** Keep the default **Regular** policy: the app has a real
dashboard window and GPUI sets Regular for windowed apps
(`gpui-pre-macos/src/platform.rs:216-226`). The app then has a Dock icon and an app
menu; closing the window leaves the Dock icon, and the reopen handler brings it back.
The alternative, `LSUIElement=1`/Accessory, gives the menu-bar-only feel but removes
the app menu, makes window activation the app's job, and makes the tray the only
command surface. That is a product decision for a later release, not a v1 requirement.
`cargo-packager` exposes it as `macos.backgroundApp` (`LSUIElement`) if wanted later.

### 2.6 File watching

`notify` uses FSEvents with `FileEvents | NoDefer` and `latency = 0`
(`notify-8.2/src/fsevent.rs`). Three macOS behaviours need code or docs:

- **Dropped/coalesced events.** FSEvents can deliver `MustScanSubDirs` and expects the
  client to rescan the subtree ([Apple](https://developer.apple.com/documentation/coreservices/kfseventstreameventflagmustscansubdirs)).
  The watcher flattens events to paths, and `enqueue_watch_batch` drops directories
  (`engine.rs:1223-1225`), so a rescan hint is lost until the 30-minute scan. Treat an
  `EventKind::Any`/directory path for a source root as "reconcile this folder now".
- **Unpaired renames.** FSEvents cannot pair the two sides of a rename; notify says so
  in `fsevent.rs`. The engine already detects renames by content hash (R5.2), so this
  is handled, but a rename that also changes content will look like delete + create.
- **Network volumes.** notify documents that network filesystems may emit no events at
  all ([docs.rs](https://docs.rs/notify/8.2.0/notify/)). The 30-minute reconciliation
  scan is the safety net; "new file within 10 seconds" (R2.3) does not hold on an SMB
  share. Either document that, or add a polling fallback for network mounts (a
  `PollWatcher` or a shorter scan interval per folder). This is the one watcher item
  that can grow; keep it out of v1 unless SMB is a target.

### 2.7 TCC and first-run UX

macOS prompts before an app reads **Desktop, Documents, Downloads, iCloud Drive,
removable volumes, or network volumes**; the prompt is triggered by the access, not by
an API call, and applies to Developer ID apps as much as App Store apps
([Apple Platform Security](https://support.apple.com/guide/security/controlling-app-access-to-files-secddd1d86a6/web);
[Mac User Guide](https://support.apple.com/guide/mac-help/control-access-to-files-and-folders-on-mac-mchld5a35146/mac)).

- **No Full Disk Access is needed** for a user-picked folder tree under those
  locations. FDA is for things the app cannot prompt for (other users' homes, arbitrary
  `~/Library` subtrees, `/Users/Shared`).
- **A denial is permanent** until the user changes System Settings (or `tccutil reset`).
  The folder must render as "unreadable with a reason", never as "empty". Map a
  permission error to: "macOS is blocking access. Open System Settings › Privacy &
  Security › Files & Folders and allow Immich Sync." Do not silently retry.
- Add the five `NS*UsageDescription` strings through `cargo-packager`'s
  `macos.infoPlistPath` so the prompt explains why: `NSDesktopFolderUsageDescription`,
  `NSDocumentsFolderUsageDescription`, `NSDownloadsFolderUsageDescription`,
  `NSRemovableVolumesUsageDescription`, `NSNetworkVolumesUsageDescription`. They are
  optional but highly recommended ([Apple](https://developer.apple.com/documentation/bundleresources/information-property-list/nsdesktopfolderusagedescription)).
- The prompt can appear during the startup scan, possibly before the window is
  painted. That is normal; do not treat a slow first scan as a hang.

### 2.8 Single instance and diagnostics

`instance.rs` uses `GenericNamespaced`; on macOS that maps to `/tmp/<name>`
([interprocess docs](https://docs.rs/interprocess/2.4.5/interprocess/local_socket/name/type/)).
It works, but `/tmp` is shared and `try_overwrite(true)` unlinks a stale socket. A
per-user path under `$TMPDIR` (or `~/Library/Application Support`) is cleaner; it is
hardening, not a blocker. Watch the Unix socket path limit (104 bytes) if the app
moves to `Application Support`.

`debug_bundle.rs:242` reads `/proc/sys/kernel/osrelease`; on macOS it returns `None`
and the bundle reports "Operating system: macos". A one-line `uname -r`/`sw_vers`
fallback would make support bundles more useful; optional.

`skip.rs` already covers `.DS_Store`, `Icon\r`, `@eaDir`, `Thumbs.db`, temporary files
and symlinks, so the default skip rules need no macOS additions. The macOS hidden flag
(`UF_HIDDEN`) is not checked, but the known offenders are already name-matched.

## 3. Packaging and release

### 3.1 What `cargo-packager` 0.11.8 already does

The pinned version is also the current release (2025-11-27). On macOS it supports two
formats: **`.app` and `.dmg`** ([crate docs](https://docs.rs/cargo-packager/0.11.8/cargo_packager/)).
The relevant config is `[package.metadata.packager.macos]`
([`MacOsConfig`](https://docs.rs/cargo-packager/0.11.8/cargo_packager/config/struct.MacOsConfig.html)):

| Key | Use here |
|---|---|
| `minimumSystemVersion = "13.0"` | Written as `LSMinimumSystemVersion`. It does **not** set the compiler's deployment target; set `MACOSX_DEPLOYMENT_TARGET=13.0` yourself. |
| `signingIdentity = "Developer ID Application: …"` | Only if an account exists. It has **no env-var override**; it must be in config. ADR 0008 does not set it. |
| `entitlements` | Not needed: no sandbox, no JIT, no unsigned dylibs. |
| `backgroundApp` | `false` for a Dock icon (recommended); `true` sets `LSUIElement`. |
| `infoPlistPath` | A plist merged over the generated one; put the five usage descriptions here. |
| `notarizationCredentials` | Env only (`APPLE_*`); it cannot be set in the config file. |

The metadata this repo would add:

```toml
[package.metadata.packager.macos]
minimumSystemVersion = "13.0"
backgroundApp = false
infoPlistPath = "macos/Info.plist"
# signingIdentity is injected by the release job; do not commit it here if local
# unsigned builds must keep working.
```

And the compiler's deployment target, which the packager does not set:

```toml
# .cargo/config.toml
[env]
MACOSX_DEPLOYMENT_TARGET = "13.0"
```

What it runs on a macOS host **when a signing identity is configured**, from the
0.11.8 source
([`codesign/macos.rs`](https://github.com/crabnebula-dev/cargo-packager/blob/@crabnebula/packager-v0.11.8/crates/packager/src/codesign/macos.rs),
[`package/app/mod.rs`](https://github.com/crabnebula-dev/cargo-packager/blob/@crabnebula/packager-v0.11.8/crates/packager/src/package/app/mod.rs)):

- imports the `.p12` from `APPLE_CERTIFICATE`/`APPLE_CERTIFICATE_PASSWORD` into a
  temporary keychain (or uses the login keychain if the certificate is already there);
- signs the app and its native binaries inside-out with
  `codesign --force -s <identity> --options runtime --timestamp` (the DMG is signed
  too, without `--options runtime`);
- notarizes the `.app` with `ditto` + `xcrun notarytool submit --wait` and staples the
  ticket when credentials are present;
- if no notarization credentials are found, it **warns and continues**, so CI must
  assert the ticket itself;
- builds the DMG with a pinned `create-dmg` script downloaded at package time, which
  uses `hdiutil`; it passes `--skip-jenkins` when `CI=true`;
- signs the `.dmg`, but does **not** notarize or staple the DMG (the app inside is
  stapled, which is what Gatekeeper checks);
- generates the `.icns` from the `icons` list, and writes the standard `Info.plist`
  (`CFBundleIdentifier`, `CFBundleShortVersionString`, `NSHighResolutionCapable`,
  `LSMinimumSystemVersion`, `LSUIElement` when `backgroundApp`, …).

It does **not** create a universal binary. `--target universal-apple-darwin` makes it
look for a binary under `target/universal-apple-darwin/<profile>`; producing that
binary is a `lipo -create` step you own. Two per-arch DMGs are simpler.

### 3.2 Apple requirements (not taken)

A signed and notarized public release needs:

- **Apple Developer Program membership**, USD 99/year
  ([Apple](https://developer.apple.com/programs/)).
- A **Developer ID Application** certificate, created by the Account Holder
  ([Apple](https://developer.apple.com/help/account/certificates/create-developer-id-certificates/)).
- **Code signing of every executable**, **hardened runtime**, and a **secure timestamp**.
  `cargo-packager` does all three for native binaries.
- **Notarization** through `notarytool`, then **stapling**. Since macOS 10.15, all
  Developer ID software built after 2019-06-01 must be notarized
  ([Apple](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)).

An unsigned/unnotarized DMG still opens, but only through **System Settings › Privacy
& Security › Open Anyway** after a blocked launch; the old Control-click → Open
shortcut is no longer the documented path on macOS 26/27
([Apple](https://support.apple.com/en-us/102445)). **ADR 0008 accepts this** because no
Apple Developer Program account will be created. If an account ever exists,
notarization is the only way to remove the warning.

### 3.3 CI and release wiring

Current runners (2026-10-26): **`macos-26`** is arm64, **`macos-26-intel`** is Intel;
`macos-14` retires **2026-11-02**; Intel macOS CI ends after **Fall 2027**
([GitHub docs](https://docs.github.com/en/actions/reference/runners/github-hosted-runners);
[runner-images](https://github.com/actions/runner-images)).

Recommended shape:

- **Per-push CI:** one `macos-26` (arm64) job: `cargo clippy --workspace --all-targets
  -- -D warnings` and `cargo test --workspace`. Add the Metal toolchain step first:
  `xcrun metal --version || (xcodebuild -runFirstLaunch && xcodebuild -downloadComponent
  MetalToolchain)`. Do not add an Intel test job for v1; the code is platform-neutral
  and the Intel runner is on a clock.
- **Release:** one arm64 job builds both targets (`aarch64-apple-darwin` natively,
  `x86_64-apple-darwin` cross-compiled; the macOS SDK supports it), then runs
  `cargo packager` twice with `--target` and `--formats app,dmg`. This avoids paying
  for an Intel runner and keeps the two DMGs consistent. If cross-compiling the
  bundled SQLite or the Metal shaders proves troublesome, fall back to
  `macos-26-intel`.
- **Secrets (only if an account exists):** `APPLE_CERTIFICATE` (base64 `.p12`),
  `APPLE_CERTIFICATE_PASSWORD`, and one notarization route: `APPLE_API_KEY` +
  `APPLE_API_ISSUER` + `APPLE_API_KEY_PATH` (recommended), or `APPLE_ID` +
  `APPLE_PASSWORD` (app-specific) + `APPLE_TEAM_ID`.
  The signing **identity** is not a secret but has no env override, so put it in a
  CI-only `Packager.toml` passed with `--config` (or a committed
  `macos.signingIdentity`). Do not commit it to the main `Cargo.toml` if local
  unsigned builds must keep working: the packager signs whenever the identity is set.
  Do not set `APPLE_CERTIFICATE` if you set up the keychain yourself; the packager
  would create a second one.
- **Unsigned release (the chosen path):** no `APPLE_*` secrets and no
  `signingIdentity`. The packager builds `.app`/`.dmg` without invoking codesign at
  all; there is no warning to rely on, so the job asserts the DMG exists, checksums
  it, and stops there. The signing bullets apply only if an account ever exists.
- **Assertions:** after packaging, fail if `dist/*.dmg` is missing. With signing,
  also run `xcrun stapler validate "Immich Sync.app"` and check the DMG with
  `spctl -a -vvv -t open --context context:primary-signature <dmg>`; an unsigned
  build gets the file and checksum assertion only. This is what catches the
  "packager warned and skipped notarization" path.
- **Checksums/attestation:** add the two DMGs to `dist/`, extend the `find` count
  assertions in `checksums` (`1 deb`, `1 AppImage`, `1 setup.exe`, `2 dmg`), and add
  `dist/*.dmg` to the attestation `subject-path`.
- **Artifacts:** `Immich Sync_<version>_aarch64.dmg` and
  `Immich Sync_<version>_x64.dmg` (the packager renders `x86_64` as `x64`). A
  universal DMG would be one artifact but needs the `lipo` step; skip it for v1.

Runner cost: macOS minutes are billed at a premium (10× Linux on the standard
private-repo plan); one arm64 job per push and two packaging runs per release is the
cheap shape.

## 4. Decisions recorded in ADR 0008

[ADR 0008](../adr/0008-macos-unsigned-distribution.md) records:

1. **Distribution:** unsigned, unnotarized `.dmg` from GitHub Releases; direct
   distribution, not the Mac App Store, not sandboxed. No Apple Developer Program
   account.
2. **Support matrix:** macOS 13+ on arm64 and x86_64; two per-arch DMGs; no universal
   binary in v1; Intel drops when GitHub retires Intel macOS runners (Fall 2027) or
   earlier.
3. **Activation policy:** Regular (Dock icon + app menu) for v1; `LSUIElement`/
   Accessory is a later product option.
4. **Credentials:** `apple-native-keyring-store` with the `keychain` feature, login
   keychain; no provisioning profile, no entitlement.
5. **Autostart:** `SMAppService.mainApp`, macOS 13+; a `~/Library/LaunchAgents`
   plist is the fallback if an unsigned bundle is refused.
6. **Path semantics:** macOS is case-insensitive like Windows; case-sensitive volumes
   are a documented limitation until a per-folder case mode exists.
7. **Watcher:** FSEvents as-is, with directory/rescan events triggering reconciliation;
   network volumes rely on the periodic scan unless polling is added.

## 5. Work breakdown

Estimates are engineering days for someone familiar with the code; they are not
measured.

| Phase | Work | Estimate |
|---|---|---|
| **P0 probe** | Build and run on a real Apple Silicon Mac: window, theme, tray, menu, picker, notification, watcher, single instance. Keychain and autostart come after. Gate before the rest. | 1–2 d |
| **Platform seams** | Keychain store; `SMAppService` autostart; `open`; case-insensitive path keys; menu bar + `on_reopen`; tray icon/title; TCC message; watcher rescan. | 4–6 d |
| **Packaging** | `macos` metadata, `Info.plist` usage strings, 1024 px icon, deployment target, `.app`/`.dmg`, local unsigned build. | 2–3 d |
| **Release (unsigned)** | Packager config, `.dmg` build, checksums, attestation, runner matrix, no Apple secrets. | 2–3 d |
| **Hardening & docs** | Network-volume decision, case-sensitive-volume note, README install/troubleshooting/paths, SPEC/PLAN/CONTRIBUTING (ADR 0008 is written). | 3–4 d |
| **Verification** | Apple Silicon + Intel, clean install, Gatekeeper first launch, TCC allow/deny, external drive, login item approval, sleep/wake, upgrade over installed copy, uninstall. | 3–5 d |
| **Total** | | **~3 weeks** |

Suggested ticket split (one epic, ten children):

1. macOS build + smoke probe (blocker).
2. Keychain credential store.
3. Autostart via `SMAppService`.
4. Menu bar, Dock reopen, tray icon.
5. Open folder, path case, TCC message.
6. Watcher rescan and network-volume decision.
7. Packaging: `.app`/`.dmg`, plist, icons, deployment target.
8. Unsigned release CI (DMG, checksums, attestation).
9. README, SPEC, PLAN, CONTRIBUTING (ADR 0008 is written).
10. Verification matrix.

## 6. Risks

| Risk | Impact | Mitigation |
|---|---|---|
| `gpui-tray`/`gpui-kit` macOS bugs are unproven here | Tray or UI gaps found late | P0 probe on real hardware; pin versions; upstream patches are small. |
| No default app menu, no Dock reopen | Looks unfinished; Cmd+Q missing | Add `set_menus` + `on_reopen` in the first macOS PR. |
| Tray icon template/Retina gap in `gpui-tray` 0.1.4 | Blurry or wrong-coloured menu-bar icon | Upstream `template`/scale flag, or ship a coloured 18 px icon for v1. |
| Gatekeeper blocks the first launch | Users think the download is broken | README shows the Open Anyway flow and the checksum; same posture as SmartScreen. |
| No stable code identity (unsigned) | Keychain/TCC re-prompts after updates | Document it; the only real fix is an Apple Developer account (ADR 0008). |
| Metal toolchain on CI | Build fails with `unable to find utility "metal"` | `xcodebuild -downloadComponent MetalToolchain` step; assert `xcrun metal --version`. |
| TCC prompt at first scan | Looks like a hang or a broken folder | Friendly denial message; document Files & Folders; test deny + allow. |
| Network volumes emit no FSEvents | SMB folder only syncs every 30 min | Document, or add polling for network mounts. |
| Case-sensitive APFS volume | Two files collapse to one path key | Document the limitation; per-folder case mode later. |
| macOS runner cost | CI bill | One arm64 job per push; two packaging runs per release. |
| GPUI 0.3.8 vs macOS 27 | Untested combination | P0 probe on the current OS; pin and upgrade at milestone boundaries as ADR 0001 says. |
| Intel retirement (Fall 2027) | Intel artifact eventually unbuildable on hosted CI | Cross-compile from arm64; drop Intel when the runner goes. |
| Ad-hoc dev builds re-prompt for Keychain/TCC | Noisy development | Use a stable local self-signed identity, or accept the prompts. |

## 7. What was not verified

- No macOS host was available, so nothing was built or run. The GPUI/gpui-kit
  rendering, the GPUI test suite on a macOS runner, the tray, and the picker are
  source-verified only.
- `cargo-packager` signing/notarization was read from the 0.11.8 source, not executed.
- The exact DMG artifact names, and whether `cargo packager --config` merges or
  replaces the `package.metadata.packager` table, need a local run.
- Apple's `xattr -d com.apple.quarantine` workaround on macOS 26/27 is not documented
  by Apple and was not tested; do not rely on it.
- Whether `SMAppService.mainApp.register()` requires the app to be in `/Applications`
  is not stated by Apple; only forum reports exist. Test a build from `~/Applications`
  and from `/Applications`.
- Whether a folder picked in `NSOpenPanel` grants implied consent for its whole tree is
  not stated by Apple (the implied-consent wording says "a file"). Test with a fresh
  TCC state (`tccutil reset …`).

## 8. Recommendation

**Do it, after v1, as a dedicated milestone with a P0 probe first.** The runtime risk is
low because macOS is GPUI's home platform; the real risk is packaging, the Gatekeeper
first-run experience, and the unproven `gpui-kit`/`gpui-tray` combination. Sequence:

1. **Probe** (1–2 days): build on a real Mac, run the app, confirm window, tray, menu,
   picker, notifications, watcher, single instance. Stop and reassess if this fails.
2. **Seams** (1 week): credentials, autostart, open folder, case, menu/reopen/tray.
3. **Packaging** (2–3 days): `.app`/`.dmg`, plist, icon, unsigned local build.
4. **Release (2–3 days):** unsigned `.dmg`, checksums, attestation, CI job. No Apple
   account or secrets.
5. **Docs + verification** (1 week): README/SPEC/PLAN, clean-install and TCC matrix.

Skip for v1 macOS: Mac App Store/sandbox, notarization (no account), universal
binary, Sparkle auto-update, Homebrew cask, macOS 10.15–12, Full Disk Access
handling, Unicode-normalized path keys, per-folder case mode. Name them in the ADR
as later options.

## 9. Sources

All retrieved **2026-10-26**.

**Apple — distribution and signing**
- [Notarizing macOS software before distribution](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)
- [Customizing the notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow)
- [Hardened Runtime](https://developer.apple.com/documentation/security/hardened-runtime)
- [Create Developer ID certificates](https://developer.apple.com/help/account/certificates/create-developer-id-certificates/)
- [Apple Developer Program](https://developer.apple.com/programs/)
- [Safely open apps on your Mac](https://support.apple.com/en-us/102445)
- [Apple security releases (latest macOS 27.0.1)](https://support.apple.com/en-us/100100)

**Apple — platform APIs**
- [SMAppService](https://developer.apple.com/documentation/servicemanagement/smappservice) · [mainApp](https://developer.apple.com/documentation/servicemanagement/smappservice/mainapp) · [register()](https://developer.apple.com/documentation/servicemanagement/smappservice/register())
- [Updating helper executables from earlier versions of macOS](https://developer.apple.com/documentation/servicemanagement/updating-helper-executables-from-earlier-versions-of-macos)
- [UNUserNotificationCenter](https://developer.apple.com/documentation/usernotifications/unusernotificationcenter) · [Asking permission to use notifications](https://developer.apple.com/documentation/usernotifications/asking-permission-to-use-notifications)
- [NSImage.isTemplate](https://developer.apple.com/documentation/appkit/nsimage/istemplate) · [NSStatusItem](https://developer.apple.com/documentation/appkit/nsstatusitem) · [ActivationPolicy.accessory](https://developer.apple.com/documentation/appkit/nsapplication/activationpolicy-swift.enum/accessory)
- [Menu bar HIG](https://developer.apple.com/design/human-interface-guidelines/the-menu-bar)
- [TN3137: On Mac keychain APIs and implementations](https://developer.apple.com/documentation/technotes/tn3137-on-mac-keychains) · [TN3127: Inside Code Signing: Requirements](https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements)
- [Controlling app access to files in macOS](https://support.apple.com/guide/security/controlling-app-access-to-files-secddd1d86a6/web) · [Control access to files and folders](https://support.apple.com/guide/mac-help/control-access-to-files-and-folders-on-mac-mchld5a35146/mac) · [`NSDesktopFolderUsageDescription`](https://developer.apple.com/documentation/bundleresources/information-property-list/nsdesktopfolderusagedescription) (and the Documents/Downloads/Removable/Network variants)
- [FSEventStreamCreate](https://developer.apple.com/documentation/coreservices/1443980-fseventstreamcreate) · [kFSEventStreamEventFlagMustScanSubDirs](https://developer.apple.com/documentation/coreservices/kfseventstreameventflagmustscansubdirs) · [kFSEventStreamEventFlagRootChanged](https://developer.apple.com/documentation/coreservices/kfseventstreameventflagrootchanged)
- [APFS FAQ](https://developer.apple.com/library/archive/documentation/FileManagement/Conceptual/APFS_Guide/FAQ/FAQ.html) · [Apple File System Reference](https://developer.apple.com/support/downloads/Apple-File-System-Reference.pdf) · [TN1150 HFS Plus Volume Format](https://developer.apple.com/library/archive/technotes/tn/tn1150.html)

**Crates and upstream source**
- [cargo-packager 0.11.8](https://docs.rs/cargo-packager/0.11.8/cargo_packager/) · [`MacOsConfig`](https://docs.rs/cargo-packager/0.11.8/cargo_packager/config/struct.MacOsConfig.html) · [source at the 0.11.8 tag](https://github.com/crabnebula-dev/cargo-packager/blob/@crabnebula/packager-v0.11.8/crates/packager/src/codesign/macos.rs)
- [apple-native-keyring-store](https://crates.io/crates/apple-native-keyring-store) · [README](https://github.com/open-source-cooperative/apple-native-keyring-store)
- [objc2-service-management](https://crates.io/crates/objc2-service-management) · [smappservice-rs](https://crates.io/crates/smappservice-rs)
- [notify 8.2](https://docs.rs/notify/8.2.0/notify/) · [gpui-pre](https://crates.io/crates/gpui-pre) · [gpui-tray](https://github.com/domenkozar/gpui-tray)
- [Zed macOS build guide](https://github.com/zed-industries/zed/blob/main/docs/src/development/macos.md) · [Zed install docs](https://github.com/zed-industries/zed/blob/main/docs/src/installation.md)

**GitHub Actions**
- [GitHub-hosted runners reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
- [runner-images README](https://github.com/actions/runner-images/blob/main/README.md) · [macOS 26 arm64 image](https://github.com/actions/runner-images/blob/main/images/macos/macos-26-arm64-Readme.md)
- [macOS 13 runner retirement](https://github.blog/changelog/2025-09-19-github-actions-macos-13-runner-image-is-closing-down/) · [macOS 14 deprecation](https://github.com/actions/runner-images/issues/13518)
- [Installing an Apple certificate on macOS runners](https://docs.github.com/en/actions/deployment/deploying-xcode-applications/installing-an-apple-certificate-on-macos-runners-for-xcode-development)
