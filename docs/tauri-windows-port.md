# Windows Tauri port status

The `rewrite/tauri-windows` branch keeps the Electron application available for rollback and builds a Windows Tauri 2 application from the existing React renderer. The app version remains `1.6.0` for local artifacts.

## Build

Use Node 24 from `.nvmrc`, pnpm 10.34.4, Rust MSVC for `x86_64-pc-windows-msvc`, Microsoft C++ Build Tools, and WebView2. From the repository root:

```powershell
pnpm run build:tauri:win
pnpm run build:tauri:win:replacement
```

Each command type-checks the Tauri renderer contracts, builds all six HTML entry pages, compiles Rust, creates an NSIS installer, and writes SHA-256 metadata under `release/tauri/<profile>`. The Windows workflow is build-only and runs no tests.

Current local installers compile successfully and are unsigned. The generated NSIS script contains both preinstall and preuninstall hook call sites. The hook code was included in the installer builds, but neither installer has been executed; migration, registry restoration, and uninstall behavior still need user-led review:

- Preview: `release/tauri/preview/BSManager-Preview-1.6.0-x64-setup.exe`, SHA-256 `5710d5c3ac3eb20baec875da381b9f252f569f39d485123c60bf6478028cad4a`.
- Replacement: `release/tauri/replacement/BSManager-1.6.0-x64-setup.exe`, SHA-256 `43279d8e7e35e9030a04aba1af29e1044b68fdbd6f5285a918f59afd8143d1c3`.

## Implemented in the Rust host

- Separate preview and replacement identifiers and settings directories. The preview installer does not claim protocols or file extensions. The playlist deep-link setting manages both the `bsplaylist` URI and `.bplist` file association, backing up existing per-user handlers; each profile’s NSIS uninstaller restores them only while the registry still points to that profile. The replacement preinstall hook backs up legacy config and protocol/file-association registry keys, then invokes the registered Electron uninstaller. It aborts on ambiguity, a running legacy process, or uninstall failure; it does not delete game libraries. Settings read/write, install-folder selection, and explicit legacy-settings import routes exist.
- Main window setup, native dialogs, window controls, drag/drop events, single-instance handling, protocol registration and routing, local version catalog, installation-folder operations, local version discovery/edit/clone/uninstall, shared-folder links, launch-shortcut creation, and restart-after-reply.
- Steam sidecar download routes, Meta login/session routes, Meta downloads, game launch, Windows VR runtime and sideload controls, and supporter data. Close-on-launch tracks a newly created Beat Saber process by PID, creation time, and executable path.
- ModelSaber search and lookup; model download, one-click installation, local listing, export, and deletion.
- BeatSaver map and playlist search/details; local map listing, deletion, import/export, download, and one-click install. Map installs stage archive extraction, compute the Beat Saber hash, validate it against BeatSaver, then move the map into the managed folder.
- BeatSaver response caching for maps fetched during the current session. The scraped song-detail cache loads the existing replacement-profile protobuf file, refreshes it from the current sources with ETag support, stores it beside profile settings, and supplies detail data to local maps and playlists. The per-folder `map-info-cache.json` is loaded, reused, and debounced to disk in the same location; deleted map entries are removed. Preview caches remain separate.
- Playlist listing, file import, install, delete, archive export, download, and one-click install. Download progress keeps the renderer’s existing payload shape.
- BeatMods availability and health routes; installed-mod discovery, DLL/ZIP import, download and content-hash validation, installation, removal, and remove-all. Archive member paths reject traversal and symbolic links. IPA execution and network cancellation still need runtime review.
- Update checks report no update. Download/install routes return `UPDATES_DISABLED` until a signed Tauri update endpoint is configured. Taskbar progress follows renderer progress on the originating window.
- Tauri logs use info level and redact registered secret strings plus Oculus token patterns before writing to any target. Renderer `add-filter-string` and `add-filter-pattern` routes update the shared formatter filters.
- Static configuration commands preserve unrecognized fields during file round trips. Request streaming and cancellation are bound to the originating window.

These routes compile. Their runtime behavior has not yet passed the user-led acceptance scenarios.

## Remaining parity work

- All shared Windows IPC route names are registered; the only shared routes absent from Rust are Linux-specific. Deep-link delivery and some configuration or library behavior still need implementation or parity review. See `src/shared/models/ipc/ipc-routes.ts` and `src/main/ipcs` for the source inventory.
- Verify six-window behavior and window-specific capabilities. The shared renderer entry pages exist, but the auxiliary window lifecycle and remote authentication/webview isolation need acceptance review.
- Finish first-run preview library import behavior and replacement install failure recovery. The replacement hook retains configuration and registry backups, but an uninstaller that succeeds before a later Tauri install failure still requires manual Electron reinstall for rollback. Exercise both hooks, protocol restoration, and rollback in user-led review. Replacement is not ready to replace an installed Electron build.
- Complete Meta and Steam credential migration, download resume/error paths, map and playlist cache parity, mod dependency and IPA behavior, content sharing, archive interruption recovery, relocation, shortcuts, LIV integration, proxy behavior, and signed update endpoint configuration. Launch ownership and close-on-launch still need user-led runtime review.
- Complete visual comparison at 100%, 125%, and 150% display scaling, in light and dark themes, with custom colors and default, minimum, and maximized window sizes. Build success does not establish visual or functional parity.

Keep the existing Electron installer for rollback. Current Tauri installers are local build artifacts only. Do not distribute or install the replacement profile until migration, installer recovery, runtime parity, and visual review are complete.
