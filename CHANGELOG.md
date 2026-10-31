# Changelog

## [0.3.0](https://github.com/elementalvoid/immich-sync/compare/v0.2.1...v0.3.0) (2026-10-31)


### Features

* **diagnostics:** render the value rows as a kit description list ([#80](https://github.com/elementalvoid/immich-sync/issues/80)) ([9355513](https://github.com/elementalvoid/immich-sync/commit/935551382da313bd90b820d9d8ce3fb8c33e5dbe))
* **diagnostics:** show the debug bundle in the file manager ([#90](https://github.com/elementalvoid/immich-sync/issues/90)) ([1c53e30](https://github.com/elementalvoid/immich-sync/commit/1c53e30937361f807d57da36f2808b730be657a4)), closes [#87](https://github.com/elementalvoid/immich-sync/issues/87)
* **settings:** render the surface as kit pages of rows with search ([#78](https://github.com/elementalvoid/immich-sync/issues/78)) ([b6300c7](https://github.com/elementalvoid/immich-sync/commit/b6300c7b7c2369634fa0b941ea6a3bbb5d92531f))
* **shell:** choose the folder filter with the kit select ([#79](https://github.com/elementalvoid/immich-sync/issues/79)) ([2847ddb](https://github.com/elementalvoid/immich-sync/commit/2847ddb5ea387afc04085b1db60666a0b3294d5c))
* **shell:** confirm each copy beside its own control ([#82](https://github.com/elementalvoid/immich-sync/issues/82)) ([3c21bbf](https://github.com/elementalvoid/immich-sync/commit/3c21bbf2c634fa0e89e00562ef901e75e3d6844b))
* **shell:** open the folder panel through the dialog layer ([#81](https://github.com/elementalvoid/immich-sync/issues/81)) ([ec66961](https://github.com/elementalvoid/immich-sync/commit/ec6696107e3b8d1a79ab191a913c76ee4ebcb80c))
* **shell:** render every empty state from one component ([#77](https://github.com/elementalvoid/immich-sync/issues/77)) ([18b0fcc](https://github.com/elementalvoid/immich-sync/commit/18b0fcc86c07e423f90772d1e1d76442adb7b99b))
* **shell:** render every notice from one component ([#76](https://github.com/elementalvoid/immich-sync/issues/76)) ([15a4937](https://github.com/elementalvoid/immich-sync/commit/15a49379c487446fbe65a54764ea9a73dab92f29))
* **shell:** switch surfaces with the kit tab list ([#75](https://github.com/elementalvoid/immich-sync/issues/75)) ([b56efde](https://github.com/elementalvoid/immich-sync/commit/b56efde9dfc46cff2e90d96e3d4bef6cc036a90e))


### Bug Fixes

* **a11y:** give every named element a role and unify the strip list ([#74](https://github.com/elementalvoid/immich-sync/issues/74)-[#82](https://github.com/elementalvoid/immich-sync/issues/82)) ([156f41e](https://github.com/elementalvoid/immich-sync/commit/156f41e0490ffb9e214060e0dbfd84a7cc0ab84f))
* **a11y:** state a table row's term once and group the description list ([13961eb](https://github.com/elementalvoid/immich-sync/commit/13961eb4985bdd7ac7df4797ea7d43df134c8198))
* **diagnostics:** bound the long value and name the term ([#80](https://github.com/elementalvoid/immich-sync/issues/80)) ([9c912aa](https://github.com/elementalvoid/immich-sync/commit/9c912aa11c440b55199247b9fc15aa3c805f028d))
* **folder-filter:** expose selection as selection and size the trigger from the theme ([#79](https://github.com/elementalvoid/immich-sync/issues/79)) ([4d50708](https://github.com/elementalvoid/immich-sync/commit/4d50708c3bfaccb29fdd4dff69a7c60b2088fe58))
* **folder-panel:** keep Enter alive and the detail list its old height ([#81](https://github.com/elementalvoid/immich-sync/issues/81)) ([59b2f50](https://github.com/elementalvoid/immich-sync/commit/59b2f50caac341ff1193ee666f6fd154d613d5eb))
* **folder-panel:** keep escape alive backwards and the actions on screen ([#81](https://github.com/elementalvoid/immich-sync/issues/81)) ([e3e6c3e](https://github.com/elementalvoid/immich-sync/commit/e3e6c3e64957adccbdf0b74e72c2c212267a1d34))
* **notices:** announce a copy politely and keep the row still ([#82](https://github.com/elementalvoid/immich-sync/issues/82)) ([267056c](https://github.com/elementalvoid/immich-sync/commit/267056c3ace79bae4aec7f0febc8b97197a35f6b))
* **settings:** expose the page strip as tabs and stack the numeric rows at 520 ([#78](https://github.com/elementalvoid/immich-sync/issues/78)) ([1761b73](https://github.com/elementalvoid/immich-sync/commit/1761b73f40f76b606db485bd27b9cd021a2270d6))
* **settings:** keep every page keyboard reachable and refilter on paste ([#78](https://github.com/elementalvoid/immich-sync/issues/78)) ([8d1453a](https://github.com/elementalvoid/immich-sync/commit/8d1453a0ba499b963f20f982228d143e9807ee0c))
* **shell:** announce a failed row once and keep the notice seam honest ([#76](https://github.com/elementalvoid/immich-sync/issues/76)) ([af3e681](https://github.com/elementalvoid/immich-sync/commit/af3e681c331796100fa3879e18834bb5778ae61e))
* **shell:** give the empty-state action its own name ([#77](https://github.com/elementalvoid/immich-sync/issues/77)) ([a0a5760](https://github.com/elementalvoid/immich-sync/commit/a0a576035592fe1c244423dbe306989b0cca66fd))
* **shell:** give the tab ring room to close ([#75](https://github.com/elementalvoid/immich-sync/issues/75)) ([8382b20](https://github.com/elementalvoid/immich-sync/commit/8382b20035cfe81c4d1d7c922bb230252e7e7227))
* **shell:** hold the a11y role assertion and correct the pin record ([#74](https://github.com/elementalvoid/immich-sync/issues/74)) ([78ac2da](https://github.com/elementalvoid/immich-sync/commit/78ac2dad3cfc9de17f7d21430c589ceda0e2abc0))
* **shell:** keep the shown surface's tab on the strip when it is clipped ([#75](https://github.com/elementalvoid/immich-sync/issues/75)) ([4abb821](https://github.com/elementalvoid/immich-sync/commit/4abb821a6e74150e5fea2ce96b4e7f4c3cff1be7))
* **shell:** mark the chosen folder with a word, not an icon ([#79](https://github.com/elementalvoid/immich-sync/issues/79)) ([574dd4c](https://github.com/elementalvoid/immich-sync/commit/574dd4c1ea9e2b1b8ab994eec58f472b21426f9c))
* **shell:** show focus and a strip affordance on the tab bar ([#75](https://github.com/elementalvoid/immich-sync/issues/75)) ([f290e1a](https://github.com/elementalvoid/immich-sync/commit/f290e1afb731fc3980f0eed1d12dc6db5b6fe5fd))

## [0.2.1](https://github.com/elementalvoid/immich-sync/compare/v0.2.0...v0.2.1) (2026-10-28)


### Bug Fixes

* **app:** open the Remove confirmation as an alert dialog ([#73](https://github.com/elementalvoid/immich-sync/issues/73)) ([bdd1c7f](https://github.com/elementalvoid/immich-sync/commit/bdd1c7fecf118fabf463757c48ce4e6ae87eab53))

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
