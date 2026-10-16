# Windows Tauri parity review checklist

This checklist distinguishes source coverage from verified behavior. Route entries are present in the Windows Tauri dispatcher or delegated Rust handler; each runtime item remains unchecked until reviewed in the packaged Windows app against the Electron baseline. This is a user-led review record, not a claim that compilation proves parity.

## IPC routes

Review each route's payload shape, result/error behavior, progress sequence, cancellation where applicable, and effects on the correct library/profile. One-way routes should be checked for their expected side effects and error logging.

- [x] Static source comparison on 2026-10-16 found all 104 Windows routes from the baseline shared IPC mapping, including the one-shot `shortcut-launch-options` listener, and all 3 direct `ipcMain.on` event routes in the Rust dispatcher. Three Linux-only routes are out of scope. Three Tauri-only replacement renderer-import routes are added separately. Rust emits `window-arguments`; the renderer bridges native drops as `desktop-files-dropped`.

### Download and authentication

- [ ] `import-version`
- [ ] `bs-steam-download`
- [ ] `bs-steam-download-respond`
- [ ] `bs-steam-stop-download`
- [ ] `steam-session-exists`
- [ ] `steam-session-username`
- [ ] `delete-steam-session`
- [ ] `import-legacy-steam-username`
- [ ] `auto-download-bs-version`
- [ ] `download-bs-version`
- [ ] `download-bs-version-qr`
- [ ] `stop-download-bs-version`
- [ ] `send-input-bs-download`
- [ ] `bs-oculus-download`
- [ ] `bs-oculus-stop-download`
- [ ] `login-with-meta`
- [ ] `delete-meta-session`
- [ ] `meta-session-exists`

### BeatSaver API

- [ ] `bsv-search-map`
- [ ] `bsv-get-map-details-from-hashs`
- [ ] `bsv-get-map-details-by-id`
- [ ] `bsv-search-playlist`
- [ ] `bsv-get-playlist-details-by-id`

### Setup and preview import

- [ ] `bs-installer.folder-exists`
- [ ] `bs-installer.default-install-path`
- [ ] `bs-installer.install-path`
- [ ] `bs-installer.set-install-path`
- [ ] `preview.legacy-settings-import-needed`
- [ ] `preview.import-legacy-settings`
- [ ] `preview.complete-renderer-settings-import`
- [ ] `preview.skip-legacy-settings-import`
- [ ] `replacement.legacy-renderer-settings-import-needed`
- [ ] `replacement.import-legacy-renderer-settings`
- [ ] `replacement.complete-legacy-renderer-settings-import`

### Launch and shortcuts

- [ ] `create-launch-shortcut`
- [ ] `bs-launch.need-start-as-admin`
- [ ] `bs-launch.launch`
- [ ] `shortcut-launch-options`

### Maps

- [ ] `load-version-maps`
- [ ] `delete-maps`
- [ ] `export-maps`
- [ ] `bs-maps.import-maps`
- [ ] `bs-maps.download-map`
- [ ] `last-downloaded-map`
- [ ] `one-click-install-map`
- [ ] `register-maps-deep-link`
- [ ] `unregister-maps-deep-link`
- [ ] `is-map-deep-links-enabled`
- [ ] `get-maps-info-from-cache`

### Models

- [ ] `one-click-install-model`
- [ ] `register-models-deep-link`
- [ ] `unregister-models-deep-link`
- [ ] `is-models-deep-links-enabled`
- [ ] `download-model`
- [ ] `get-version-models`
- [ ] `export-models`
- [ ] `delete-models`

### Mods

- [ ] `bs-mods.get-available-mods`
- [ ] `bs-mods.get-installed-mods`
- [ ] `bs-mods.import-mods`
- [ ] `bs-mods.install-mods`
- [ ] `bs-mods.uninstall-mods`
- [ ] `bs-mods.uninstall-all-mods`
- [ ] `bs-mods.beatmods-up`

### Playlists

- [ ] `one-click-install-playlist`
- [ ] `register-playlists-deep-link`
- [ ] `unregister-playlists-deep-link`
- [ ] `is-playlists-deep-links-enabled`
- [ ] `download-playlist`
- [ ] `get-version-playlists-details`
- [ ] `delete-playlist`
- [ ] `export-playlists`
- [ ] `import-playlists`
- [ ] `install-playlist-file`

### Versions and library folders

- [ ] `bs.uninstall`
- [ ] `bs-version.get-version-dict`
- [ ] `bs-version.installed-versions`
- [ ] `bs-version.open-folder`
- [ ] `bs-version.edit`
- [ ] `bs-version.clone`
- [ ] `get-version-full-path`
- [ ] `full-version-path-to-relative`
- [ ] `get-linked-folders`
- [ ] `link-version-folder-action`
- [ ] `is-version-folder-linked`
- [ ] `relink-all-versions-folders`
- [ ] `get-shared-folder`

### Updates

- [ ] `download-update`
- [ ] `check-update`
- [ ] `get-available-update`
- [ ] `install-update`

### ModelSaber

- [ ] `ms-get-model-by-id`
- [ ] `search-models`

### OS and window controls

- [ ] `new-window`
- [ ] `open-dialog`
- [ ] `choose-folder`
- [ ] `choose-file`
- [ ] `choose-image`
- [ ] `window.progression`
- [ ] `save-file`
- [ ] `current-version`
- [ ] `is-preview-build`
- [ ] `open-logs`
- [ ] `notify-system`
- [ ] `view-path-in-explorer`
- [ ] `restart-app`
- [ ] `close-window`
- [ ] `maximise-window`
- [ ] `minimise-window`
- [ ] `unmaximise-window`
- [ ] `open-window-then-close-all`
- [ ] `open-window-or-focus`

### Supporters and configuration

- [ ] `get-supporters`
- [ ] `static-configuration.get`
- [ ] `static-configuration.set`
- [ ] `static-configuration.delete`
- [ ] `is-oculus-sideloaded-apps-enabled`
- [ ] `enable-oculus-sideloaded-apps`
- [ ] `vr-runtime.get-active`

Linux Proton routes are intentionally outside this Windows-only rewrite: `linux.set-proton-folder`, `linux.verify-proton-folder`, and `linux.get-wine-prefix-path`.

## Events and request streams

- [x] `request`, `send_lazy`, and `cancel_request` reject non-application window labels; only the six local app windows pass the Rust guard.
- [ ] Tauri Channel carries every progress/result item in order and preserves serialized field names and large manifest IDs.
- [ ] Channel completion closes observable after final value; stream errors preserve message and code.
- [ ] `cancel_request` cancels work only for originating window; closing that window cancels its outstanding work and releases request entries.
- [ ] Window arguments reach the intended app window on cold start, warm open, and reuse of an existing window.
- [ ] `desktop-files-dropped` reports native file paths to the correct window and preserves drop target semantics.
- [ ] Launch stream preserves `STEAM_LAUNCHING`, `STEAM_LAUNCHED`, `BS_LAUNCHING`, and `UNABLE_TO_LAUNCH_STEAM` payload behavior.
- [ ] Native dialogs, notifications, progress, external links, and local media URLs match renderer expectations.
- [ ] Remote Meta authentication and cookie-cleaner webviews cannot invoke app commands at runtime; their navigation, cookie, timeout, and destruction flows work.

## Windows and auxiliary windows

- [ ] Main window: opens at 1080×720 on each new main window, 900×500 minimum, frameless controls, resize/maximize/restore, drag region, startup focus.
- [ ] Update launcher: 380×500 fixed; update state, download progress, cancel, close, and reopen behavior.
- [ ] Map one-click window: 350×400 fixed; deep-link payload and installation result.
- [ ] Playlist one-click window: 350×400 fixed; deep-link and `.bplist` payloads.
- [ ] Model one-click window: 350×400 fixed; deep-link payload and installation result.
- [ ] Shortcut launcher: 600×500 default, 600×300 minimum height; launch arguments and close behavior.
- [ ] Every app window has correct command capability; closing any window cleans up its operations and listeners.
- [ ] Startup with each supported protocol and `.bplist` opens correct window without a stray main window; warm-instance routing behaves the same.

## Settings controls

- [ ] Steam remembered-session logout and login state; an imported username without a valid DPAPI-protected token shows as signed out.
- [ ] Meta remembered-session logout and login state.
- [ ] Default download store: Steam, Meta/Oculus, and ask each time.
- [ ] Theme: dark, light, and operating-system preference.
- [ ] First and second custom colors, including reset-to-default.
- [ ] Preview import v6 restores missing legacy static settings and renderer preferences, launch options, download-store selection, audio level, and dismissal choices from a v5 installation; preserves settings already changed in Preview; and refreshes installed versions. Last-launched version lookup treats missing store flags as false. Legacy encrypted Meta tokens prompt for login again.
- [ ] Custom version colors remain available from the shared library `config.cfg` in Preview and Replacement, including Preview while legacy BSManager is open; version, map, playlist, and model scans must not create or rewrite shared library files during that session.
- [ ] Installation path edit, browse, validation, apply, and move existing installation.
- [ ] Deep-link selection: all, Maps, Playlists plus `.bplist`, and Models; registration changes persist and uninstall restores only registrations still owned by profile.
- [ ] Language selection and persistence.
- [ ] Supporter list visibility; Patreon, Discord, Twitter, GitHub, feature request, bug report, and open logs actions.
- [ ] Auto-update preference and disabled-state behavior when signed profile endpoint/key are absent.
- [ ] Close BSManager on launch, hardware acceleration, symlink use, and system proxy settings.
- [ ] Settings retain unknown configuration fields through read/write and survive app restart.

## Feature and failure scenarios

- [x] Preview settings-import failures are logged and shown as an in-app error notification so a failed import is visible and can be retried on a later launch.
- [x] Read-only post-install audit on 2026-10-16 found local Preview executable `2.0.0-f77b1389` (SHA-256 `7FECF9BD82E275541F0D32FFD35AC5FD2B81CD1046E07355AE8811D41EF74FB9`). It predates the available `2.0.0-011ec9bd` installer. The app log records an earlier `steam-session-exists` `NOT_IMPLEMENTED` error at 06:59 UTC, then startup readiness at 08:13 UTC; the audit did not launch the app.
- [x] The post-install log records 14 non-secret legacy renderer preferences imported at 08:13 UTC. Preview settings show legacy import complete and renderer-import marker version 5. At the time of that audit, the installed `2.0.0-f77b1389` executable had not been replaced by the then-available `2.0.0-011ec9bd` installer. Current `2.0.0-c8038bb4` installers have since been built; runtime behavior remains unverified.
- [x] A read-only LevelDB key-name comparison found all 14 imported keys in both legacy and Preview storage, including `first-color`, `second-color`, launch settings, store selection, audio level, and dismissal choices. No stored values were recorded; runtime display remains pending.
- [x] Read-only local data audit found legacy `config.json` and renderer LevelDB present; Preview selects `D:\` and uses library `D:\BSManager`. `D:\BSManager\config.cfg` contains one custom color record for Beat Saber `1.45.2` (`#45ffac`, default folder name); matching `BSInstances\1.45.2` contains `Beat Saber.exe` and `Beat Saber_Data\globalgamemanagers` with version string `1.45.2`. Color source and scan inputs are present; display in packaged app remains pending.
- [x] Static migration-path audit confirms Preview stores settings separately, imports legacy `config.json` while retaining existing Preview values, and reads allow-listed renderer preferences from legacy LevelDB. Installed custom-version colors are read from the selected library's `config.cfg` by game-version/name. Runtime import and color display remain unverified.
- [x] Replacement renderer migration reads the same allow-listed legacy LevelDB preferences, fills only missing Tauri localStorage keys, stores its completion marker under the replacement profile's app-config directory, and leaves legacy `config.json` untouched. Import remains idempotent and retries if interrupted before marker completion; runtime behavior remains unchecked.
- [ ] Install `2.0.0-c8038bb4` and confirm Preview startup, Steam login state, imported settings, the `1.45.2` version color, and the Settings crash fix. The v5 Preview marker should trigger the v6 import prompt once.
- [ ] Steam password, QR, Steam Guard, remembered session, resume, CDN failover, verification, progress, input, cancel, and retry.
- [ ] Meta login cookie capture, remembered session, expired credential, logout, manifest download, decompression, integrity failure, resume, and cancel.
- [ ] Steam and Meta launch, VR detection/warning, arguments/environment, elevation denied, process tracking, close-on-launch, and LIV integration.
- [ ] Version import, edit, clone, verify, relocation, delete, shared junctions/symlinks, backup/restore, and shortcut creation.
- [ ] Mod dependencies, import/install/uninstall, maps and playlists, model operations, search/filter/sort, archives, sharing, caches, and audio previews.
- [ ] Deep links, `.bplist`, native file drop, playlist drag-and-drop, and external links.
- [ ] Offline services, corrupt archive, inaccessible destination, interrupted operation, network retry, and app/window closure during a cancellable operation.
- [ ] Preview first-run and v6 settings import copies configuration only, reuses selected library paths, keeps separate settings, and blocks shared-content mutations while legacy Electron runs. Version colors stay in shared library `config.cfg`; Preview scans read it without writing or creating missing content folders while Electron is open.
- [ ] Replacement first-run imports allow-listed legacy renderer preferences into its separate WebView storage, preserves any values already set there, leaves Electron `config.json` and LevelDB untouched, and retries an interrupted import without showing Preview's import modal.
- [ ] Preview Steam password/QR/Steam Guard download works while legacy Electron BSManager remains open, uses a separate staging and final version folder, and resumes an interrupted staged download after Electron closes.
- [x] On 2026-10-16, source comparison found all 119 bundled version records, fields, and ordering match [upstream `bs-versions.json`](https://raw.githubusercontent.com/Zagrios/bs-manager/master/assets/jsons/bs-versions.json).
- [ ] Packaged-app catalog starts with the bundled data, refreshes from the upstream URL, caches a valid response, and falls back offline without losing the bundled version list.
- [ ] Preview installer does not claim existing protocols or file associations; explicit settings claim records prior owner and uninstall restores only while preview remains owner.
- [ ] Preview and Replacement claim shared protocols and `.bplist`, then uninstall in both orders; remaining registrations never point at a removed profile, and original handlers return when stack has no live profile owner.
- [ ] Replacement detects legacy install, backs up settings and registrations, aborts on running app or uninstall failure, preserves game data, and restores on rollback.
- [ ] Replacement updater rejects missing/invalid signatures; preview and replacement update channels remain separate.
- [ ] Uninstall of either profile preserves game libraries; rollback by reinstalling retained Electron installer and restoring configuration/registration backups.

## Visual comparison

- [ ] Compare baseline and Tauri at 100%, 125%, and 150% Windows display scaling.
- [ ] Compare light, dark, OS theme, and custom colors.
- [ ] Compare main window at default, minimum, and maximized sizes; review all six app windows.
- [ ] Confirm Preview displays the Preview title-bar tag and `v2.0.0-c8038bb4` link, which opens commit `c8038bb4` on the fork; Replacement does not show the Preview tag.
- [ ] Compare setup, library, version tabs, shared content, settings, dialogs, animations, fonts, icons, spacing, typography, and scroll behavior.
- [ ] Record any remaining WebView2 rendering differences with matching baseline and Tauri captures supplied or initiated by the user.

## Installer and artifact review

- [ ] Confirm preview and replacement NSIS install/uninstall, WebView2 bootstrapper, shortcuts, and elevation behavior.
- [ ] Confirm replacement rollback backup paths and `migration.json` contents before relying on installer recovery.
- [x] Built Preview from commit `c8038bb4` with temporary version `2.0.0-c8038bb4`: `release/tauri/preview/BSManager-Preview-2.0.0-c8038bb4-x64-setup.exe` (10,733,433 bytes), SHA-256 `9CB004FF085C10857609E7C55CD234EC1BC60076AF7C9AB6F09CC9A1F893017D`, Authenticode `NotSigned`.
- [x] Built Replacement from commit `c8038bb4` with temporary version `2.0.0-c8038bb4`: `release/tauri/replacement/BSManager-2.0.0-c8038bb4-x64-setup.exe` (10,731,540 bytes), SHA-256 `5C2D36159848037BCEF6ECA25A23F73CC9F06C2B0240AA9E0A91A0639D7FBC25`, Authenticode `NotSigned`.
- [x] The Tauri UI contract TypeScript compilation, Vite production build, Rust helper builds, and Rust application builds completed for both profiles. The temporary version was removed from `package.json`, `Cargo.toml`, `Cargo.lock`, and `tauri.conf.json` after packaging; these source files remain at `2.0.0`.
- [x] Title-bar source displays the backend version and links a hash suffix to `https://github.com/SlopSaber/bs-manager/commit/<hash>`. The compiled executable reports product version `2.0.0-c8038bb4`; runtime display and link behavior remain unreviewed.
- [x] Inspected current `2.0.0-c8038bb4` NSIS archives with 7-Zip. Both contain `bs-manager-tauri.exe` and the three Rust helper executables under `_up_\assets\scripts`; neither contains Electron or Node runtime files. `bs-versions.json` is absent because Rust embeds the version catalog.
- [ ] User installs `2.0.0-c8038bb4` Preview and confirms installer writes the bundled catalog successfully, then reviews startup, settings migration, the Settings crash fix, and version colors.
