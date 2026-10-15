# Windows Tauri port status

The `rewrite/tauri-windows` branch builds a Windows Tauri 2 application from the existing React renderer. The Electron main process, preload bridge, and Electron packaging hooks are removed. Its package manifest has no Electron runtime or packager dependencies; Node is build tooling. Rollback uses a separately retained Electron installer, and the original source remains on baseline commit `00380c798ea3a8447270e8bbd367e921d03d154b`. The app version remains `1.6.0` for local artifacts.

## Build

Use Node 24 from `.nvmrc`, pnpm 10.34.4, Rust MSVC for `x86_64-pc-windows-msvc`, Microsoft C++ Build Tools, and WebView2. From the repository root:

```powershell
pnpm run build:tauri:win
pnpm run build:tauri:win:replacement
```

Each command builds the locked Rust helpers, type-checks the Tauri renderer contracts, builds all six HTML entry pages, compiles the Tauri host, creates an NSIS installer, and writes SHA-256 metadata under `release/tauri/<profile>`. The rewrite-branch Windows workflow provisions Rust, builds both profiles, and uploads installers as artifacts; it runs no tests. Legacy test, lint, and release jobs skip the rewrite branch.

Signed remote updates are opt-in at compile time. Preview reads `BSM_TAURI_PREVIEW_UPDATE_ENDPOINT` and `BSM_TAURI_PREVIEW_UPDATE_PUBLIC_KEY`; replacement reads `BSM_TAURI_REPLACEMENT_UPDATE_ENDPOINT` and `BSM_TAURI_REPLACEMENT_UPDATE_PUBLIC_KEY`. Set both values for a profile before its build. Endpoint must use HTTPS and return Tauri updater metadata for that profile's channel. Tauri verifies downloaded package signatures before installation. With either value absent, that profile reports no updates and keeps remote updates disabled. Do not reuse Electron update feeds.

Current local installers compile successfully under Node 24.21.0 and are unsigned. All profiles keep Tauri's configured window list empty so startup can choose a target before creating any window: a supported protocol URL or `.bplist` opens its corresponding window without first creating the main window; otherwise the `auto-update` setting selects the launcher when a signed update source is configured, falling back to the main window when updates are disabled or unavailable. The generated NSIS script contains both preinstall and preuninstall hook call sites. The hook code was included in the installer builds, but neither installer has been executed; migration, registry restoration, and uninstall behavior still need user-led review:

- Preview: `release/tauri/preview/BSManager-Preview-1.6.0-x64-setup.exe`, SHA-256 `fefc09e707911229f6cd4bc24a7a6e90778cd33aa8861a913929fa7fd01e9a15`.
- Replacement: `release/tauri/replacement/BSManager-1.6.0-x64-setup.exe`, SHA-256 `250e9d8ea0257ec541bde7eb8310dd9586a696057d7b2e6308e21044b9b0f577`.

## Implemented in the Rust host

- Separate preview and replacement identifiers and settings directories. Preview blocks library/content mutations while legacy `BSManager.exe` runs; operations serialize within the app and across Preview/Replacement processes with a Windows mutex keyed by normalized library root. Relocation locks both source and destination roots. Tauri installer configuration does not claim protocols or file extensions. On first app start, replacement registers `bsmanager`; preview registers the separate `bsmanager-preview` launch scheme so its shortcuts cannot redirect existing BSManager links. Visible settings manage maps, playlists, and model links. Playlist settings also manage `.bplist`; handlers are backed up per user and restored on uninstall only while still owned by that profile. The replacement preinstall hook backs up legacy config and protocol/file-association registry keys, invokes the registered Electron uninstaller, and restores the backed-up config if that uninstaller removed it. It aborts on ambiguity, a running legacy process, or uninstall failure; it does not delete game libraries. Settings read/write, install-folder selection, and explicit legacy-settings import routes exist.
- Main window setup and startup routing, native dialogs, window controls, drag/drop events, single-instance handling, protocol registration and routing, local version catalog, installation-folder operations, local version discovery/edit/clone/uninstall, shared-folder links, launch-shortcut creation, LIV registry shortcuts with saved launch options, and restart-after-reply.
- Steam sidecar download routes, Meta login/session routes, Meta downloads, game launch, Windows VR runtime and sideload controls, and supporter data. Close-on-launch tracks a newly created Beat Saber process by PID, creation time, and executable path.
- ModelSaber search and lookup; model download, one-click installation, local listing, export, and deletion.
- BeatSaver map and playlist search/details; local map listing, deletion, import/export, download, and one-click install. Map installs stage archive extraction, compute the Beat Saber hash, validate it against BeatSaver, then move the map into the managed folder.
- BeatSaver response caching for maps fetched during the current session. The scraped song-detail cache loads the existing replacement-profile protobuf file, refreshes it from the current sources with ETag support, stores it beside profile settings, and supplies detail data to local maps and playlists. The per-folder `map-info-cache.json` is loaded, reused, and debounced to disk in the same location; deleted map entries are removed. Preview caches remain separate.
- Playlist listing, file import, install, delete, archive export, download, and one-click install. Download progress keeps the renderer’s existing payload shape.
- BeatMods availability and health routes; installed-mod discovery, including library files matched to manifests, DLL/ZIP import, download and content-hash validation, installation, removal, and remove-all. Archive member paths reject traversal and symbolic links. Mod download and IPA process waits honor cancellation; both still need runtime review.
- Tauri updater checks use profile-specific endpoint/public-key build settings. Downloads stream progress to the requesting window, support request cancellation, verify signatures before installation, and install through the Tauri Windows updater. Updates stay disabled when signed profile settings are absent. Taskbar progress follows renderer progress on the originating window.
- Tauri logs use info level and redact registered secret strings plus Oculus token patterns before writing to any target. Renderer `add-filter-string` and `add-filter-pattern` routes update the shared formatter filters.
- Static configuration commands preserve unrecognized fields during file round trips. The system-proxy setting disables OS proxy use by default and applies Windows proxy settings to newly created Rust HTTP clients when enabled, including after a settings change. Tauri Channels carry ordered request streams; `cancel_request` and window destruction cancel work belonging to that window, and cancellation from another window is rejected.

These routes compile. Their runtime behavior has not yet passed the user-led acceptance scenarios.

## Windows parity checklist

The complete route-by-route, event, window, settings, failure, installer, and visual review matrix is in [the Windows parity checklist](tauri-windows-parity-checklist.md). Route entries show source coverage; unchecked boxes require user-led runtime review.

- [x] Renderer contract type-check and production build include all six entry pages: main, update launcher, shortcut launcher, and map, playlist, and model one-click windows.
- [x] Source inventory maps every non-Linux route in `src/shared/models/ipc/ipc-routes.ts` to the Rust dispatcher or a delegated Rust handler. Linux Proton routes remain outside this Windows rewrite.
- [x] Preview and replacement x64 NSIS profiles compile and emit local installer artifacts.
- [ ] Exercise every route's result, progress, error, cancellation, and event behavior in the Windows app; track each in the linked matrix.
- [ ] Review every settings control, dialog, window lifecycle, file-drop target, deep link, shortcut, and library operation against the Electron baseline.
- [ ] Complete installer migration/recovery checks and 100%, 125%, and 150% visual comparisons across themes and window sizes.

## Remaining parity work

- Deep-link and `.bplist` startup routing is implemented but still needs runtime parity review. Some settings/library behavior also needs review. See `src/shared/models/ipc/ipc-routes.ts`, `src-tauri/src/lib.rs`, and the linked parity matrix for the route inventory.
- Verify six-window behavior and window-specific capabilities. The shared renderer entry pages exist, but the auxiliary window lifecycle and remote authentication/webview isolation need acceptance review.
- First-run preview import copies legacy settings into preview storage and reuses imported library paths without copying game installations. Verify this flow and replacement install failure recovery in user-led review. Preview process detection and mutation blocking also need acceptance review. If Electron uninstall succeeds and later Tauri installation fails, reinstall Electron manually and restore any needed registry backups. Replacement is not ready to replace an installed Electron build.
- Verify Meta and Steam login, DPAPI-backed remembered sessions, and re-login when legacy credentials cannot be imported. Review download resume/error paths, map and playlist cache parity, mod dependency and IPA behavior, content sharing, archive interruption recovery, relocation, the system-proxy toggle, LIV registry sync, launch ownership, close-on-launch, and each configured signed updater channel. Remote updates stay disabled in current local artifacts because no endpoint/key pair was supplied.
- Complete visual comparison at 100%, 125%, and 150% display scaling, in light and dark themes, with custom colors and default, minimum, and maximized window sizes. Build success does not establish visual or functional parity.

## Replacement rollback

The replacement preinstall hook stores a timestamped backup under `%LOCALAPPDATA%\BSManager\migration-backups\`. `migration.json` lists the saved Electron `config.json` and protocol/file-association registry exports. The hook restores `config.json` if Electron's uninstaller removes it. Game libraries are not removed.

If Electron uninstall succeeds but the Tauri install then fails, reinstall the retained Electron installer first. If settings are missing or damaged, restore the saved `config.json` to `%APPDATA%\bs-manager\config.json`. After Electron returns to its original install path, import the relevant registry exports listed in `migration.json` to restore prior protocol and `.bplist` handlers. Import HKCU exports as the same Windows user; HKLM exports may require elevation. Review each export before importing if another app has since claimed that handler.

Keep the existing Electron installer for rollback. Current Tauri installers are local build artifacts only. Do not distribute or install the replacement profile until migration, installer recovery, runtime parity, and visual review are complete.
