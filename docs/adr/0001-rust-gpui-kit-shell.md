# Rust with GPUI-Kit as the shell stack

Status: accepted
Date: 2026-10-12

The research first recommended Go + Wails 3 for time-to-ship. The review correction showed that Wails' lead rests on bundled shell features that the Rust ecosystem provides as ordinary crates. The user chose the Rust route. We will build the client in Rust with GPUI-Kit for the UI, `tray-icon` and `muda` for the tray and native menus, `keyring-core` with a Windows store for credentials, and `cargo-packager` for installers. Rust gives one language across the sync engine and the UI, a native artifact with no WebView2 dependency, and a strong IO ecosystem for a background sync engine.

## Considered Options

- **Go + Wails 3 (beta).** Bundled tray, single-instance, NSIS/MSIX, and signing. Rejected: the bundled advantages are one-time wiring costs, not capability gaps, and the user chose Rust.
- **Tauri 2.** Rust backend with a webview UI and bundled tray and installers. Kept as the fallback if the tray integration fails.
- **egui.** Native and mature. Rejected: weaker visuals and documentation for this product's UI.

## Consequences

- The UI chain includes `gpui-pre`, an exact weekly snapshot with no API stability contract. Pin it exactly. Upgrade only at milestone boundaries.
- GPUI-Kit ships no tray or installer tooling. The app assembles them from `tray-icon`, `muda`, and `cargo-packager`.
- Tray integration with GPUI is unproven. M0 tests it before engine work starts. If it fails, this ADR is superseded by a Tauri 2 decision.
