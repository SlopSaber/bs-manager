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

- Preview: `release/tauri/preview/BSManager-Preview-1.6.0-x64-setup.exe`, SHA-256 `3f5074719bf687db0646e662f410aef5f699a3af86487bb4f97964b29ad85e97`.
- Replacement: `release/tauri/replacement/BSManager-1.6.0-x64-setup.exe`, SHA-256 `bb24df36e498f5567ce5c4425337e87c3e712b6419ea360251d81fe3e07f371d`.

## Implemented in the Rust host

- Separate preview and replacement identifiers and settings directories. The preview installer does not claim protocols or file extensions. Explicit protocol registration backs up the current per-user handler; each profile’s NSIS uninstaller restores it only while the registry command still points to that profile. The replacement preinstall hook backs up legacy config and protocol/file-association registry keys, then invokes the registered Electron uninstaller. It aborts on ambiguity, a running legacy process, or uninstall failure; it does not delete game libraries. Settings read/write, install-folder selection, and explicit legacy-settings import routes exist.
- Main window setup, native dialogs, window controls, drag/drop events, single-instance handling, protocol registration and routing, local version catalog, installation-folder operations, local version discovery/edit/clone/uninstall, shared-folder links, launch-shortcut creation, and restart-after-reply.
- Steam sidecar download routes, Meta login/session routes, Meta downloads, game launch, Windows VR runtime and sideload controls, and supporter data.
- ModelSaber search and lookup; model download, one-click installation, local listing, export, and deletion.
- BeatSaver map and playlist search/details; local map listing, deletion, import/export, download, and one-click install. Map installs stage archive extraction, compute the Beat Saber hash, validate it against BeatSaver, then move the map into the managed folder.
- BeatSaver detail caching for maps fetched during the current session and the `get-maps-info-from-cache` route. Playlist details use cached song metadata for duration, mapper count, NPS, and song details when available; the bulk scraped cache is not yet loaded or persisted.
- Playlist listing, file import, install, delete, archive export, download, and one-click install. Download progress keeps the renderer’s existing payload shape.
- BeatMods availability and health routes; installed-mod discovery, DLL/ZIP import, download and content-hash validation, installation, removal, and remove-all. Archive member paths reject traversal and symbolic links. IPA execution and network cancellation still need runtime review.
- Update checks report no update. Download/install routes return `UPDATES_DISABLED` until a signed Tauri update endpoint is configured.
- Static configuration commands preserve unrecognized fields during file round trips. Request streaming and cancellation are bound to the originating window.

These routes compile. Their runtime behavior has not yet passed the user-led acceptance scenarios.

## Remaining parity work

- Complete route coverage is outstanding. Bulk map-cache loading and persistence, taskbar progress, several OS-control operations, deep-link delivery review, and some configuration or library routes still need implementation or parity review. See the route names in `src/shared/models/ipc/ipc-routes.ts` and `src/main/ipcs` as the source inventory.
- Verify six-window behavior and window-specific capabilities. The shared renderer entry pages exist, but the auxiliary window lifecycle and remote authentication/webview isolation need acceptance review.
- Finish first-run preview library import behavior and replacement install failure recovery. The replacement hook retains configuration and registry backups, but an uninstaller that succeeds before a later Tauri install failure still requires manual Electron reinstall for rollback. Exercise both hooks, protocol restoration, and rollback in user-led review. Replacement is not ready to replace an installed Electron build.
- Complete Meta and Steam credential migration, download resume/error paths, map and playlist cache parity, mod dependency and IPA behavior, content sharing, archive interruption recovery, relocation, shortcuts, LIV integration, proxy behavior, process tracking, and signed update endpoint configuration.
- Complete visual comparison at 100%, 125%, and 150% display scaling, in light and dark themes, with custom colors and default, minimum, and maximized window sizes. Build success does not establish visual or functional parity.

Keep the existing Electron installer for rollback. Current Tauri installers are local build artifacts only. Do not distribute or install the replacement profile until migration, installer recovery, runtime parity, and visual review are complete.
