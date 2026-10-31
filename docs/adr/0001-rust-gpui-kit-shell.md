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

The shell stack moves to `gpui-kit = "=0.8.1"`, the last release before the component work starts, so the component tickets build on 0.8's fixed focus, keyboard and accessibility behaviour instead of working around 0.7.2. It is a prefactor: no behaviour change, and no assertion was weakened — the only test edit is one accessibility role the release itself changed, which the test now asserts per field (below). The Consequences rule above still governs — upgrade only at milestone boundaries, and pin exactly — because 0.8.1 pins `gpui-pre =0.3.10`, another exact snapshot that moved several re-exported GPUI APIs.

What the new snapshot broke, and what this app had to touch: the scroll-input rename (`PlatformInput::ScrollWheel` → `Scroll`, `ScrollWheelEvent` → `ScrollEvent`, `on_scroll_wheel` → `on_scroll`), which is two source sites plus one test's event type; `Keystroke::unparse` now taking the host `OperatingSystem`, which the menu-shortcut test reads from `cx.platform_info().os`; and a masked input now reporting `Role::PasswordInput` instead of `Role::TextInput`, which the settings test now asserts per field rather than accepting either. The release's other two breaking changes needed no call site here: `Modifiers::secondary`/`secondary_key` now take `&App` and the app calls neither (every `.secondary()` in it is the zero-argument `Button::secondary()` variant), and `KeyBinding::new` now takes a `&'static str`, which its two bindings already are.

`gpui-tray` stays at `=0.1.4`: its `gpui-pre ^0.3.4` range admits 0.3.10, so the resolved graph still holds exactly one `gpui-pre` and no tray bump is needed. That single copy is the constraint to re-check on every future bump: if a tray release pins a `gpui-pre` that gpui-kit's exact pin excludes, the app would hold two GPUI runtimes, which is not a state to patch around.


## Addendum: the surface switch is a tab list, not `base::Tab`s (2026-10-30)

Adopting an identity the rest of the app already depends on is a real cost of a component, and it has to be checked before the component is used. The two layers of the kit's tab lose different things in 0.8.1. `component::Tab` cannot keep our id: `TabBar` always calls `Tab::ix(usize)`, which sets the element id to the integer index (`gpui-component-0.8.1/src/tab/tab.rs:717`), so a tab cannot carry the stable `nav-*` id the per-surface tests click. `base::Tab` keeps its id — `Tab::new(id)`/`.id()` render `self.base.id(self.id)` (`gpui-base-0.8.1/src/tabs.rs:59-66`, `:155-158`) — so the id half is not its problem; what it cannot do is hand focus back. It takes no focus handle of its own, and its `render` wraps itself in the test-support observer (`.test_support()`, `tabs.rs:156`) before any caller can, so a caller's `track_focus` still makes it a working tab stop but `window.find(id).focused()` cannot read that back: the kit panics with its own "focus binding was not observed". Its source records the missing keyboard model too ("Tabs do not participate in keyboard focus by themselves").

The rejected middle ground is `base::Tab` plus a caller's own `track_focus`: the tab would be reachable by Tab and Enter, but no test could see focus move, and the focused-tab assertions in `logs_ui`, `settings_ui` and `diagnostics` are how the keyboard path this app ships is tested.

The surface switch therefore keeps the kit's tab-list root (`base::Tabs`) and writes the tab role, accessible label, position in the set, selected state and focus handle on each tab's own element. Those are the same accessibility fields `base::Tab` writes; what the app adds is a stable id, a tab stop per tab, and a horizontally scrolling strip, which is what lets the window minimum come from the narrowest surface instead of from seven labels. Treat the same question — can this component carry our id, our focus handle, our keyboard path, and let us observe the focus? — as part of choosing any further kit component on this pinned version, and prefer composing the base layer over dropping the identity.

## Addendum: `rows.rs` mirrors `SettingItem` for cards (2026-10-31)

A card cannot host a real `SettingItem` on 0.8.1, so `crates/app/src/rows.rs::Row::render` draws that item's layout by hand. What makes it impossible: `SettingItem` is a plain enum with no `Render`, `RenderOnce` or `IntoElement` impl, its only renderer is `SettingItem::render_item`, which is `pub(super)` (`gpui-component-0.8.1/src/setting/item.rs:252-325` — the range the mirror reproduces), `SettingGroup::render` and `SettingPage::render` are `pub(crate)`/`pub(super)` (`group.rs:114`, `page.rs:125`), and the one public renderer, `Settings` (`settings.rs:382`), draws its own sidebar and search box — inside a card that buys a second sidebar and a second search field, not a row.

One deliberate deviation, at `rows.rs` `Row::render`: the label column is `flex_1().min_w_0()` instead of the kit's `flex_1().max_w_3_5()` cap, which is `max_w(relative(0.6))` — 60%, not 3.5 rem (`gpui-pre-macros-0.3.10/src/styles.rs:1133`). A settings page caps a fixed-width field column; a card's field is a content-sized control, and the cap only shortens the help sentence beside it. The mirror also skips the help's `size_full` and the kit's 480 px stacking breakpoint (`settings.rs` passes it to every item), so a card row stays label-left at every width.

What a gpui-kit bump must re-check, because no test can see it: diff `gpui-component-<version>/src/setting/item.rs:252-325` against `crates/app/src/rows.rs::Row::render` and re-run the row table in `crates/app/tests/logs_ui.rs` and `crates/app/tests/diagnostics.rs`. The suite asserts the rows' words and names, not their geometry. If a release makes `render_item` public, gives `SettingItem` a `Render` impl, or makes the group or page renderers public, delete the mirror and hand the item to the card; until then a bump that restyles the settings surface leaves these ten rows on 0.8.1's layout and nothing in `cargo test` will say so.
