# macOS ships unsigned, without an Apple Developer Program account

Status: accepted
Date: 2026-10-26
Amended: 2026-10-26 (the app bundle is ad-hoc signed; see the amendment)

macOS becomes a supported platform, and its packages are **unsigned and
unnotarized**. There is no Apple Developer Program membership, no Developer ID
certificate, no notarization, no Mac App Store listing, and no App Sandbox. The
user downloads a `.dmg`, drags the app to `/Applications`, and the first launch
is blocked by Gatekeeper until they allow it in **System Settings › Privacy &
Security › Open Anyway**. `SHA256SUMS` is the integrity check. This is the macOS
counterpart of the unsigned Windows build: the warning is expected, documented,
and not a sign that the download was altered. The constraint is explicit and
durable: no Apple Developer Program account will be created, and notarization is
impossible without one. The code does not need it — the arm64 toolchain's linker
ad-hoc signs the executable, which satisfies the kernel's requirement that arm64
code be signed; Gatekeeper is the separate first-launch exception. x86_64 has no
signature at all.

The supported matrix is **macOS 13 (Ventura) or later on arm64 and x86_64**, as
two per-architecture DMGs. The credential store is the login Keychain
(`apple-native-keyring-store`, `keychain` feature), autostart is
`SMAppService.mainApp` with a `~/Library/LaunchAgents` fallback if an unsigned
bundle is refused, and the app keeps the Regular activation policy (Dock icon and
app menu). macOS path keys are case-insensitive like Windows, because APFS is
case-insensitive by default. The research, including the notarization path we are
not taking, is in
[`docs/research/macos-support.md`](../research/macos-support.md).

## Considered Options

- **Enrol in the Apple Developer Program and notarize (USD 99/year).** Rejected
  by the user: no account will be created. It is the only way to remove the
  Gatekeeper warning; the research records the full path if that ever changes.
- **Ship unsigned and document the Gatekeeper exception.** Chosen. Same posture
  as Windows SmartScreen; the checksum, and later the provenance attestation,
  are the integrity checks.
- **Do not support macOS.** Rejected: the runtime stack is already portable, so
  the port is small relative to the platform it opens.
- **Mac App Store.** Rejected: it requires an account, and the sandbox would
  force security-scoped bookmarks for every source folder.
- **Ad-hoc sign the `.app` bundle as a release step.** Not chosen at first: it
  gives no stable identity and does not remove the Gatekeeper warning, so it
  fixed neither of the problems a Developer ID would fix. Revisited on 2026-10-26
  (see the amendment): hardware showed that on arm64 a bundle with no bundle-level
  signature is reported as **damaged**, with no **Open Anyway** route at all, so
  the signature is now required for the app to be launchable, not a substitute
  for an identity.

## Consequences

- Gatekeeper blocks the first launch. The README documents the exact **Open
  Anyway** flow (macOS 26/27 no longer documents Control-click → Open) and says
  the warning is expected, the same way it documents SmartScreen.
- There is no Apple-verified integrity check. Every release publishes
  `SHA256SUMS`; the provenance attestation covers the DMGs when the repository
  variable enables it. The README tells users to verify the checksum before
  overriding Gatekeeper.
- No Developer ID means no stable code identity. The login-keychain item and the
  Files & Folders TCC grant may prompt again after an update; the user allows
  them again. If that becomes a support burden, the only real fix is an Apple
  Developer account, which is out of scope.
- `cargo-packager` builds the `.app` with no `signingIdentity`, so it skips
  signing and notarization silently; the release job then ad-hoc signs the
  bundle with `codesign --force --sign - --timestamp=none`, verifies it with
  `codesign --verify --strict`, and builds the DMG itself with `hdiutil` (see the
  amendment). The job sets no Apple secret and must fail when either the bundle
  or the app inside the mounted image fails verification.
- Autostart uses `SMAppService.mainApp`. If an unsigned bundle is refused, the
  macOS backend falls back to a `~/Library/LaunchAgents` plist rather than
  adding an account.
- The `Info.plist` carries the bundle identifier, the five file-access usage strings,
  and `LSMinimumSystemVersion`. Notifications run from the `.app`; gpui-pre disables
  them when there is no bundle identifier, and whether an unsigned bundle is also
  refused is part of the P0 probe. If it is, notifications degrade to nothing, not a
  crash.
- If an Apple Developer account ever appears, the change is additive: add the
  identity and secrets, and `cargo-packager` signs with hardened runtime and
  notarizes. No application code changes.
- This ADR adds macOS to the supported platforms.

## Amendment 2026-10-26: the bundle is ad-hoc signed

Hardware evidence from the first arm64 DMG this workflow produced: launching the
app gave **"Immich Sync.app is damaged and can't be opened. You should move it to
the Trash."**, a dialog that offers no **Open Anyway**. The bundle carried no
bundle signature: the arm64 linker's ad-hoc signature covers the executable
only, and Gatekeeper judges the bundle. A probe app whose bundle was signed with
`codesign --force --sign -` showed the normal first-launch dialog and launched
after **System Settings › Privacy & Security › Open Anyway**.

Decision: the macOS release job signs the bundle ad-hoc, then builds the DMG.
`cargo-packager` produces only the `.app` (`--formats app`); the job runs
`codesign --force --sign - --timestamp=none "dist/Immich Sync.app"`, verifies it
with `codesign --verify --strict`, and assembles the DMG with `hdiutil` from a
staging directory holding the signed bundle and an `Applications` symlink. It
verifies the app again from the mounted image. The DMG is not signed; only the
bundle is. There is still no signing identity, no notarization, and no Apple
account.

Hardware confirmed on an Intel Mac: with the bundle ad-hoc signed,
`SMAppService.mainApp.register()` is accepted and the app appears under Login
Items, so the `~/Library/LaunchAgents` fallback is the escape hatch for an
unsigned local build rather than the shipped path.

Not fixed: there is still **no stable code identity**, so the login-Keychain item
and the Files & Folders grant can still ask again after an update; and the
Gatekeeper warning and the **Open Anyway** step remain exactly as documented.
Only a Developer ID plus notarization changes either, and that needs the account
this ADR refuses. The `.app` and DMG file names are unchanged, so the install
flow for users does not change.
