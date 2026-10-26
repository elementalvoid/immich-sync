# macOS ships unsigned, without an Apple Developer Program account

Status: accepted
Date: 2026-10-26

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
- **Ad-hoc sign the `.app` bundle as a release step.** Not chosen: it does not
  satisfy Gatekeeper and does not give a stable identity across versions, so it does
  not fix either of the problems a Developer ID would fix.

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
- `cargo-packager` builds `.app` and `.dmg` with no `signingIdentity`; it skips
  signing and notarization silently. The release job sets no Apple secrets and must
  assert that the DMG exists and is checksummed.
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
- This ADR adds macOS to the supported platforms. The deferral in `SPEC.md` §2 and
  N1 is removed when the macOS work lands in a post-v1 release; `SPEC.md`,
  `PLAN.md`, `README.md`, and `CONTRIBUTING.md` are updated then.
