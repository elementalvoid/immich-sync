# Contributing to Immich Sync

Thanks for helping. This is a deliberately narrow client: it uploads files from
local folders to one Immich server, and it never deletes server-side assets on
its own. Changes that widen that scope need a discussion in an issue first.

- For a bug report, include the client version (Diagnostics), the operating
  system, the Immich server version, and a debug bundle if it is safe to share.
- For a security problem, do not open a public issue — see
  [SECURITY.md](SECURITY.md).
- Small, focused pull requests are easier to review than broad ones.

## Building from source

Rust is pinned by [`rust-toolchain.toml`](rust-toolchain.toml) (1.99.0), so
`rustup` installs the right toolchain automatically.

On Debian/Ubuntu-family Linux, the packages CI uses are:

```sh
sudo apt-get install -y \
  gcc g++ clang pkg-config \
  libfontconfig-dev libfreetype-dev libwayland-dev libxkbcommon-dev \
  libxkbcommon-x11-dev libx11-xcb-dev libxcb1-dev libvulkan1 mesa-vulkan-drivers \
  libssl-dev libzstd-dev libdbus-1-dev
```

For the headless window tests and screenshots, add:

```sh
sudo apt-get install -y xvfb xauth dbus-daemon imagemagick x11-utils xdotool
```

On Windows, use the MSVC Rust toolchain with the Visual Studio C++ build tools.
macOS is not part of v1.

```sh
cargo run -p immich-sync
```

## Workspace layout

| Crate | What it is |
| --- | --- |
| `crates/app` | The GPUI shell: window, tray, views, and app models |
| `crates/sync-core` | The sync engine, state store, credentials, limits, and backoff |
| `crates/immich-client` | The hand-written Immich HTTP client |
| `crates/mock-immich` | The test double the engine and UI tests run against |

## Gates

CI runs these on every push, on Linux and Windows:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Plus a headless Linux smoke that launches the real binary under Xvfb with
lavapipe and proves the captured frame is not blank. UI changes should keep the
headless GPUI tests green; `cargo test -p immich-sync --test dashboard`,
`--test logs_ui`, and the other `tests/*_ui.rs` files cover the surfaces.

`--all-features` is invalid here: the three `gpui-tray` backends are mutually
exclusive.

Some tests are wall-clock budgets rather than progress-bounded waits:
`a_folder_with_100_files_syncs_without_manual_action`, the re-login budget
tests in `engine.rs`,
`a_server_that_stops_answering_produces_backoff_not_a_hot_loop`, the pacing
test in `limits.rs`, and the port race in `connection.rs`. In
`crates/app/tests/engine_model.rs`,
`a_click_on_a_running_engine_starts_a_scan` is one too: it hashes a 64 MiB
fixture so the scan outlives the 250 ms status poll that carries the scan flag,
and `EngineModel`'s surface has no deterministic barrier. They can starve under
heavy parallel load; run one alone before treating a failure as a regression.

## Where the design lives

| What | Where |
| --- | --- |
| Requirements and acceptance criteria | [`SPEC.md`](SPEC.md) |
| Milestones | [`PLAN.md`](PLAN.md) |
| Project terms | [`GLOSSARY.md`](GLOSSARY.md) |
| Decisions | [`docs/adr/`](docs/adr/) |
| Primary-source research | [`docs/research/`](docs/research/) |

## Screenshots

The dashboard image in the README is a real frame from the app, seeded with
`crates/app/tests/screenshot_seed.rs`. Give the capture its own XDG directories
so it cannot touch your real state:

```sh
export XDG_CONFIG_HOME="$PWD/.scratch/shot/config"
export XDG_DATA_HOME="$PWD/.scratch/shot/data"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME/immich-sync"

IMMICH_SYNC_CAPTURE_DB="$XDG_DATA_HOME/immich-sync/state.db" \
IMMICH_SYNC_CAPTURE_ROOT="$XDG_DATA_HOME/capture" \
cargo test -p immich-sync --test screenshot_seed -- --ignored
```

Then launch the binary with the same environment under Xvfb, with lavapipe for
Vulkan and no Wayland display, and capture the window:

```sh
cargo build -p immich-sync
export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json
export VK_DRIVER_FILES="$VK_ICD_FILENAMES"
export LIBGL_ALWAYS_SOFTWARE=1
unset WAYLAND_DISPLAY

dbus-run-session -- xvfb-run -a -s "-screen 0 1440x1000x24" bash -c '
  ./target/debug/immich-sync &
  APP=$!
  sleep 10
  WIN=$(xwininfo -root -tree | awk "/900x650/ {print \$1; exit}")
  xdotool windowsize "$WIN" 1200 900
  # The window opens centred, which would push its lower right past the
  # screen; at the origin the whole frame is captured.
  xdotool windowmove "$WIN" 0 0
  sleep 3
  import -window "$WIN" docs/screenshot.png
  kill $APP
'
```

A blank or crashed window is a single `#000000` histogram bin; `identify
-format "%k"` should report thousands of colours.

## Commits and pull requests

- Use a conventional subject with a scope, as the history does:
  `feat(app): ...`, `fix(client): ...`, `test(app): ...`, `docs(spec): ...`.
  These subjects are what release-please turns into the changelog, so the type
  and scope are read by more than humans.
- One logical change per commit; explain the *why* in the body when it is not
  obvious.
- Add a failing test first for behaviour changes. UI changes need headless GPUI
  coverage; configuration or packaging changes need executed evidence in the
  pull request.
- Keep the gates above green.

## Releases

For maintainers.

- **Trigger.** [release-please](https://github.com/googleapis/release-please)
  keeps one release pull request open on `main`. Merging it writes the version
  into the crate manifests and `Cargo.lock`, prepends the `CHANGELOG.md` entry,
  tags `v<version>` and creates the GitHub release **as a draft**. It then calls
  `release.yml` as a reusable workflow to build the packages into that draft and
  publish it, so a broken build leaves nothing public. A tag pushed by hand
  triggers `release.yml` directly and builds and publishes the same way.
- **Version.** The workspace root is a package too
  (`immich-sync-workspace`), holding no code: release-please's `rust` strategy
  refuses a virtual manifest, and versioning every member in lockstep is what
  keeps `Cargo.lock` in step with the released version. Only `crates/app`
  ships, and every crate is `publish = false`. The tag must equal `version` in
  `crates/app/Cargo.toml`; the workflow fails before building when it does not.
  **Run workflow** (manual dispatch, no tag) builds the packages without
  publishing a release.
- **Bootstrap.** `.release-please-manifest.json` holds the last released
  version and is written by release-please, never by hand. It started at
  `0.0.0`, so the first release came out as the `0.1.0` the app already
  reported rather than a bump past it.
- **Checks on a release pull request.** A pull request the Actions token opens
  starts no `ci.yml`. Read the diff: only the generated version files, the
  manifest and the changelog change.
- **What is built.** [`cargo-packager`](https://github.com/crabnebula-dev/cargo-packager)
  (version pinned in the workflow) builds the NSIS installer on `windows-latest`
  and the deb and AppImage on `ubuntu-24.04`. The packager configuration lives
  in `crates/app/Cargo.toml` under `[package.metadata.packager]`.
- **Checksums.** A final job downloads both platforms' packages and generates
  `SHA256SUMS` from the real files; nothing is hand-written.
- **Provenance.** The workflow attests each platform's packages with
  `actions/attest-build-provenance`, gated on the repository variable
  `RELEASE_ATTEST`. Set it to `true` under **Settings → Secrets and variables →
  Actions → Variables** once the release is public. While it is unset the
  attestation steps are skipped and the release workflow stays green. Verify a
  package with:

  ```sh
  gh attestation verify immich-sync_<version>_amd64.deb --repo elementalvoid/immich-sync
  ```

- **Signing.** v1 is unsigned, so Windows shows a SmartScreen warning. Windows
  code signing is the public-release step that removes it.

## License

Contributions are dual-licensed under MIT or Apache-2.0, matching the project;
see [README.md](README.md#license). There is no CLA.
