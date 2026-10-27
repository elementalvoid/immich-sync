# Changelog

## [0.2.0](https://github.com/elementalvoid/immich-sync/compare/v0.1.0...v0.2.0) (2026-10-27)


### Features

* **macos:** macOS 13+ (arm64 and x86_64) ([#71](https://github.com/elementalvoid/immich-sync/issues/71)) ([d332eac](https://github.com/elementalvoid/immich-sync/commit/d332eace3b7eb6e27671fb18176a725ca777cb72))

## 0.1.0 (2026-10-25)

First release: a tray client that keeps a set of local folders in an Immich
library, for Windows 10/11 (x64) and Linux desktops.

- **Watches the folders you add** and uploads new and changed files as they
  happen, with a reconciliation scan at startup and every 30 minutes.
- **Upload only.** A tracked file that disappears appears in **Review**, where
  you choose **Keep** or **Trash**; a rename is recognised by content and is
  not a deletion. Nothing on the server changes on its own.
- **Albums** per leaf folder, or one album for the whole folder.
- **Limits and retries.** Bandwidth and concurrency caps, backoff with a
  per-file attempt cap, and a **Failed items** view with **Retry**.
- **A tray that keeps working when the window is closed.** Activity in the
  tooltip; Show, Pause, Resume, Sync now and Quit in the menu.
- **Built for support.** Logs, Diagnostics, and a one-click **Collect bundle**.
- **Private by design.** Credentials in the OS credential store, redacted logs,
  no telemetry; local state stays in SQLite on your machine.

Windows packages are unsigned, so SmartScreen warns on first launch.
