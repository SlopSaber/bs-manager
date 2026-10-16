# Windows Tauri parity review checklist

This checklist distinguishes source coverage from verified behavior. Route entries are present in the Windows Tauri dispatcher or delegated Rust handler; each runtime item remains unchecked until reviewed in the packaged Windows app against the Electron baseline. This is a user-led review record, not a claim that compilation proves parity.

## IPC routes

Review each route's payload shape, result/error behavior, progress sequence, cancellation where applicable, and effects on the correct library/profile. One-way routes should be checked for their expected side effects and error logging.

### Download and authentication

- [ ] `import-version`
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

- [ ] Steam remembered-session logout and login state.
- [ ] Meta remembered-session logout and login state.
- [ ] Default download store: Steam, Meta/Oculus, and ask each time.
- [ ] Theme: dark, light, and operating-system preference.
- [ ] First and second custom colors, including reset-to-default.
- [ ] Preview import v3 restores legacy local preferences, launch options, download-store selection, audio level, and dismissal choices; preserves settings already changed in Preview; and refreshes installed versions. Legacy encrypted Meta tokens prompt for login again.
- [ ] Custom version colors remain available from the shared library `config.cfg` in Preview and Replacement, including Preview while legacy BSManager is open; version, map, playlist, and model scans must not create or rewrite shared library files during that session.
- [ ] Installation path edit, browse, validation, apply, and move existing installation.
- [ ] Deep-link selection: all, Maps, Playlists plus `.bplist`, and Models; registration changes persist and uninstall restores only registrations still owned by profile.
- [ ] Language selection and persistence.
- [ ] Supporter list visibility; Patreon, Discord, Twitter, GitHub, feature request, bug report, and open logs actions.
- [ ] Auto-update preference and disabled-state behavior when signed profile endpoint/key are absent.
- [ ] Close BSManager on launch, hardware acceleration, symlink use, and system proxy settings.
- [ ] Settings retain unknown configuration fields through read/write and survive app restart.

## Feature and failure scenarios

- [ ] Steam password, QR, Steam Guard, remembered session, resume, CDN failover, verification, progress, input, cancel, and retry.
- [ ] Meta login cookie capture, remembered session, expired credential, logout, manifest download, decompression, integrity failure, resume, and cancel.
- [ ] Steam and Meta launch, VR detection/warning, arguments/environment, elevation denied, process tracking, close-on-launch, and LIV integration.
- [ ] Version import, edit, clone, verify, relocation, delete, shared junctions/symlinks, backup/restore, and shortcut creation.
- [ ] Mod dependencies, import/install/uninstall, maps and playlists, model operations, search/filter/sort, archives, sharing, caches, and audio previews.
- [ ] Deep links, `.bplist`, native file drop, playlist drag-and-drop, and external links.
- [ ] Offline services, corrupt archive, inaccessible destination, interrupted operation, network retry, and app/window closure during a cancellable operation.
- [ ] Preview first-run and v3 settings import copies configuration only, reuses selected library paths, keeps separate settings, and blocks shared-content mutations while legacy Electron runs. Version colors stay in shared library `config.cfg`; Preview scans read it without writing or creating missing content folders while Electron is open.
- [ ] Preview Steam password/QR/Steam Guard download works while legacy Electron BSManager remains open, uses a separate staging and final version folder, and resumes an interrupted staged download after Electron closes.
- [ ] Version catalog begins with the bundled upstream database, refreshes from the upstream URL, caches a valid response, and falls back offline without losing the bundled version list.
- [ ] Preview installer does not claim existing protocols or file associations; explicit settings claim records prior owner and uninstall restores only while preview remains owner.
- [ ] Preview and Replacement claim shared protocols and `.bplist`, then uninstall in both orders; remaining registrations never point at a removed profile, and original handlers return when stack has no live profile owner.
- [ ] Replacement detects legacy install, backs up settings and registrations, aborts on running app or uninstall failure, preserves game data, and restores on rollback.
- [ ] Replacement updater rejects missing/invalid signatures; preview and replacement update channels remain separate.
- [ ] Uninstall of either profile preserves game libraries; rollback by reinstalling retained Electron installer and restoring configuration/registration backups.

## Visual comparison

- [ ] Compare baseline and Tauri at 100%, 125%, and 150% Windows display scaling.
- [ ] Compare light, dark, OS theme, and custom colors.
- [ ] Compare main window at default, minimum, and maximized sizes; review all six app windows.
- [ ] Confirm Preview displays the Preview title-bar tag and Replacement does not.
- [ ] Compare setup, library, version tabs, shared content, settings, dialogs, animations, fonts, icons, spacing, typography, and scroll behavior.
- [ ] Record any remaining WebView2 rendering differences with matching baseline and Tauri captures supplied or initiated by the user.

## Installer and artifact review

- [ ] Confirm preview and replacement NSIS install/uninstall, WebView2 bootstrapper, shortcuts, and elevation behavior.
- [ ] Confirm replacement rollback backup paths and `migration.json` contents before relying on installer recovery.
- [ ] Confirm artifact hashes and signing status against files delivered to reviewer.
- [x] Confirmed both 2.0.0 NSIS archives contain the Tauri host and Rust helpers; no Electron or Node runtime was found.
