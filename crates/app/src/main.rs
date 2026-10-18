// The app is a tray process: Explorer must start the packaged binary with no
// console window, and closing the terminal that launched it must not end it.
// Debug and test builds keep the console for panics and the startup logging
// failures. The Windows CI job asserts the release PE subsystem.
#![cfg_attr(all(not(test), not(debug_assertions)), windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
    immich_sync::run()
}
