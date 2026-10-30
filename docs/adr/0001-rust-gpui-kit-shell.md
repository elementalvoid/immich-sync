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

## Addendum: `gpui-tray` replaces raw `tray-icon` + `muda` (2026-10-12)

`gpui-tray` 0.1.4 with the `gpui-kit` backend replaces the raw `tray-icon` and `muda` wiring named above. It is a thin GPUI-native wrapper over the same native primitives (Win32 `Shell_NotifyIconW`, StatusNotifierItem/DBusMenu over `zbus` on Linux, AppKit on macOS). It starts no second event loop: its native callbacks send platform-neutral events to a task on GPUI's foreground executor. On Linux it needs no GTK, and `tray-icon` and `muda` are absent from the resolved dependency tree. Pin `gpui-tray = "=0.1.4"` exactly, like the rest of the shell stack.

Evidence: the executed M0 probe (`01-gpui-tray-probe.md`) built and ran `gpui-kit` 0.7.2 + `gpui-tray` 0.1.4 + `interprocess` 2.4.5 in one process on Linux. It rendered a real frame under Xvfb + lavapipe and completed the single-instance handoff. Windows coexistence is source-verified only, so the Windows runtime checks (tray icon, menu, window hide/show, per-monitor DPI, Explorer restart) stay outstanding. They must pass on a real Windows 10/11 machine before this ADR's Tauri fallback clause is dropped.

## Addendum: `gpui-kit` 0.7.2 → 0.8.1 (2026-10-30)

The shell stack moves to `gpui-kit = "=0.8.1"`, the last release before the component work starts, so the component tickets build on 0.8's fixed focus, keyboard and accessibility behaviour instead of working around 0.7.2. It is a prefactor: no behaviour change, and the suite passes with its assertions untouched. The Consequences rule above still governs — upgrade only at milestone boundaries, and pin exactly — because 0.8.1 pins `gpui-pre =0.3.10`, another exact snapshot that moved several re-exported GPUI APIs.

What the new snapshot required of this app, and nothing wider: the modifier helpers that now take `&App`, `Keystroke::parse`/`KeyBinding::new` now taking static strings, and the scroll-input rename (`PlatformInput::ScrollWheel` → `Scroll`, `ScrollWheelEvent` → `ScrollEvent`, `on_scroll_wheel` → `on_scroll`).

`gpui-tray` stays at `=0.1.4`: its `gpui-pre ^0.3.4` range admits 0.3.10, so the resolved graph still holds exactly one `gpui-pre` and no tray bump is needed. That single copy is the constraint to re-check on every future bump: if a tray release pins a `gpui-pre` that gpui-kit's exact pin excludes, the app would hold two GPUI runtimes, which is not a state to patch around.

