# The update check never installs anything

Status: accepted
Date: 2026-11-02

The client reads GitHub's newest published release at startup and at most once
every 24 hours, and it tells the user when a newer version exists. It never
downloads an installer, verifies a checksum, installs, or relaunches. The one
network request is
`GET https://api.github.com/repos/elementalvoid/immich-sync/releases/latest` with
`Accept: application/vnd.github+json` and `User-Agent: immich-sync/<version>`,
and it reads three fields: `tag_name`, `html_url`, and `body`. `/releases/latest`
already excludes drafts and pre-releases, which is the shape this repository
publishes — release-please creates a draft and the `checksums` job flips it.

This is a decision, not a first milestone. Installing an update on three
platforms, signed or not, is a second product: it needs a verified download, an
installer the OS accepts, a relaunch that does not race the running instance, and
a rollback story for a bad release. None of that is true while the macOS and
Windows builds are unsigned (ADR 0008), because a replaced binary is exactly
what an unsigned install cannot prove. So the release page is where the user
gets the new version: the notification, the tray item, the app menu item, and
the **Updates** page all point at a browser.

## Considered Options

- **Check and install (auto-update).** Rejected for v1. It needs signing, a
  delta or full download, checksum verification, and a relaunch path on every
  platform; a failed install on an unsigned build leaves no way to verify what
  was replaced.
- **Do not check at all.** Rejected: a user on an old build has no in-app signal
  that a fix exists, and the README's release notes are not where they look.
- **Check and show the release page.** Chosen. It is one request, one notice,
  one page, and no ability to change the installed binary.
- **Read the GitHub releases API on a timer the OS schedules.** Rejected: a
  background service for one optional check is more moving parts than the app
  already has; the check runs on the tokio runtime the app holds and stops with
  the process.

## Consequences

- The README's privacy promise changes: the Immich server is no longer the only
  network traffic. The one GitHub request and the switch that turns it off are
  named there, and SPEC gains R12.
- Failure is silence. A timeout, a 403 (unauthenticated GitHub allows 60
  requests an hour per IP), a 5xx, an unparsable body, and a tag the client
  cannot compare all mean "no newer release". The reason is one `info` line in
  the log, the page says only that it will try again at the next check, and the
  next attempt is the next interval — never an immediate retry.
- Nothing is stored about when the check ran. A start is the startup check, so
  the notes are fresh after every restart. Only the user's silence is stored,
  as the version they silenced, so the next release speaks again.
- The compared version is the package version, not `client_version()`: the
  revision suffix (`0.3.0 (abc1234)`) would never compare against a tag, and a
  build from a branch compares equal to the release it came from.
- No `semver` crate. The tag parse is "strip a leading `v`, then three `u32`s",
  and it carries its own test.
- `gpui-tray` 0.1.4 re-evaluates its menu closure when the tray is built and
  after each activation or item click, not when the menu opens, so the shell
  pushes `refresh_menu` when the release changes and when it is dismissed. The
  macOS menu bar is rebuilt the same way. A future `gpui-tray` with an
  open-menu callback would let the closure be the only path.
