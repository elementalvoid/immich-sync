# The dashboard theme seam

`crates/app/src/theme.rs` maps a `SystemTheme` (`Light`/`Dark`) to the
gpui-kit `ThemeMode` (`theme_mode`), applies it (`apply_system_theme`), and
keeps following the window's system appearance (`follow_system_theme`), which
the shell calls when it opens the main window.

The mapping and the apply step are covered by headless tests in
`crates/app/tests/dashboard.rs`; the appearance observer itself is shell
wiring.

## Screenshots

On Linux the system appearance reported under Xvfb is Light, so a plain run
captures the light theme. To force a theme for a screenshot, set
`IMMICH_SYNC_THEME` to `light` or `dark`. With it set,
`follow_system_theme` applies the value and skips the appearance observer, so
the capture is reproducible:

```sh
IMMICH_SYNC_THEME=dark target/debug/immich-sync
```

## Picking the captured view

The shell starts on the dashboard. To capture another surface, set
`IMMICH_SYNC_VIEW` to `dashboard`, `settings`, or `logs`; any other value is
ignored and the dashboard opens. `shell::view_for_name` is the pure mapping and
is covered by a headless test.
