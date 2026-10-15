# Windows Tauri port status

The `rewrite/tauri-windows` branch keeps the Electron application available for rollback and builds a Windows Tauri 2 application from the existing React renderer. The app version remains `1.6.0` for local artifacts.

## Build

Use Node 24 from `.nvmrc`, pnpm 10.34.4, Rust MSVC for `x86_64-pc-windows-msvc`, Microsoft C++ Build Tools, and WebView2. From the repository root:

```powershell
pnpm run build:tauri:win
pnpm run build:tauri:win:replacement
```

Each command type-checks the Tauri renderer contracts, builds all six HTML entry pages, compiles Rust, creates an NSIS installer, and writes SHA-256 metadata under `release/tauri/<profile>`. The rewrite-branch Windows workflow builds both profiles and uploads installers as artifacts; it runs no tests. Legacy test, lint, and release jobs skip the rewrite branch.

Current local installers compile successfully and are unsigned. The generated NSIS script contains both preinstall and preuninstall hook call sites. The hook code was included in the installer builds, but neither installer has been executed; migration, registry restoration, and uninstall behavior still need user-led review:

- Preview: `release/tauri/preview/BSManager-Preview-1.6.0-x64-setup.exe`, SHA-256 `30b3906206d5a17ecd06ef2b56b4a9fa9c07967cd7246b6c9c66154021c3411c`.
- Replacement: `release/tauri/replacement/BSManager-1.6.0-x64-setup.exe`, SHA-256 `82a18b972f43877c61859c972647daf5a597cf40c1760d8244e332b956864a02`.

## Implemented in the Rust host

- Separate preview and replacement identifiers and settings directories. Tauri installer configuration does not claim protocols or file extensions. On first app start, replacement registers `bsmanager`; preview registers the separate `bsmanager-preview` launch scheme so its shortcuts cannot redirect existing BSManager links. Visible settings manage maps, playlists, and model links. Playlist settings also manage `.bplist`; handlers are backed up per user and restored on uninstall only while still owned by that profile. The replacement preinstall hook backs up legacy config and protocol/file-association registry keys, invokes the registered Electron uninstaller, and restores the backed-up config if that uninstaller removed it. It aborts on ambiguity, a running legacy process, or uninstall failure; it does not delete game libraries. Settings read/write, install-folder selection, and explicit legacy-settings import routes exist.
- Main window setup, native dialogs, window controls, drag/drop events, single-instance handling, protocol registration and routing, local version catalog, installation-folder operations, local version discovery/edit/clone/uninstall, shared-folder links, launch-shortcut creation, LIV registry shortcuts with saved launch options, and restart-after-reply.
- Steam sidecar download routes, Meta login/session routes, Meta downloads, game launch, Windows VR runtime and sideload controls, and supporter data. Close-on-launch tracks a newly created Beat Saber process by PID, creation time, and executable path.
- ModelSaber search and lookup; model download, one-click installation, local listing, export, and deletion.
- BeatSaver map and playlist search/details; local map listing, deletion, import/export, download, and one-click install. Map installs stage archive extraction, compute the Beat Saber hash, validate it against BeatSaver, then move the map into the managed folder.
- BeatSaver response caching for maps fetched during the current session. The scraped song-detail cache loads the existing replacement-profile protobuf file, refreshes it from the current sources with ETag support, stores it beside profile settings, and supplies detail data to local maps and playlists. The per-folder `map-info-cache.json` is loaded, reused, and debounced to disk in the same location; deleted map entries are removed. Preview caches remain separate.
- Playlist listing, file import, install, delete, archive export, download, and one-click install. Download progress keeps the renderer’s existing payload shape.
- BeatMods availability and health routes; installed-mod discovery, including library files matched to manifests, DLL/ZIP import, download and content-hash validation, installation, removal, and remove-all. Archive member paths reject traversal and symbolic links. IPA execution and network cancellation still need runtime review.
- Update checks report no update. Download/install routes return `UPDATES_DISABLED` until a signed Tauri update endpoint is configured. Taskbar progress follows renderer progress on the originating window.
- Tauri logs use info level and redact registered secret strings plus Oculus token patterns before writing to any target. Renderer `add-filter-string` and `add-filter-pattern` routes update the shared formatter filters.
- Static configuration commands preserve unrecognized fields during file round trips. Request streaming and cancellation are bound to the originating window.

These routes compile. Their runtime behavior has not yet passed the user-led acceptance scenarios.

## Remaining parity work

- All shared Windows IPC route names are registered; the only shared routes absent from Rust are Linux-specific. Deep-link delivery and some configuration or library behavior still need implementation or parity review. See `src/shared/models/ipc/ipc-routes.ts` and `src/main/ipcs` for the source inventory.
- Verify six-window behavior and window-specific capabilities. The shared renderer entry pages exist, but the auxiliary window lifecycle and remote authentication/webview isolation need acceptance review.
- First-run preview import copies legacy settings into preview storage and reuses imported library paths without copying game installations. Verify this flow and replacement install failure recovery in user-led review. If Electron uninstall succeeds and later Tauri installation fails, reinstall Electron manually and restore any needed registry backups. Replacement is not ready to replace an installed Electron build.
- Complete Meta and Steam credential migration, download resume/error paths, map and playlist cache parity, mod dependency and IPA behavior, content sharing, archive interruption recovery, relocation, proxy behavior, and signed update endpoint configuration. LIV registry sync, launch ownership, and close-on-launch still need user-led runtime review.
- Complete visual comparison at 100%, 125%, and 150% display scaling, in light and dark themes, with custom colors and default, minimum, and maximized window sizes. Build success does not establish visual or functional parity.

## Replacement rollback

The replacement preinstall hook stores a timestamped backup under `%LOCALAPPDATA%\BSManager\migration-backups\`. `migration.json` lists the saved Electron `config.json` and protocol/file-association registry exports. The hook restores `config.json` if Electron's uninstaller removes it. Game libraries are not removed.

If Electron uninstall succeeds but the Tauri install then fails, reinstall the retained Electron installer first. If settings are missing or damaged, restore the saved `config.json` to `%APPDATA%\bs-manager\config.json`. After Electron returns to its original install path, import the relevant registry exports listed in `migration.json` to restore prior protocol and `.bplist` handlers. Import HKCU exports as the same Windows user; HKLM exports may require elevation. Review each export before importing if another app has since claimed that handler.

Keep the existing Electron installer for rollback. Current Tauri installers are local build artifacts only. Do not distribute or install the replacement profile until migration, installer recovery, runtime parity, and visual review are complete.
