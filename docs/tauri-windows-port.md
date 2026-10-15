# Windows Tauri port status

The `rewrite/tauri-windows` branch keeps the Electron application available for rollback and builds a Windows Tauri 2 application from the existing React renderer. The app version remains `1.6.0` for local artifacts.

## Build

Use Node 24 from `.nvmrc`, pnpm 10.34.4, Rust MSVC for `x86_64-pc-windows-msvc`, Microsoft C++ Build Tools, and WebView2. From the repository root:

```powershell
pnpm run build:tauri:win
pnpm run build:tauri:win:replacement
```

Each command type-checks the Tauri renderer contracts, builds all six HTML entry pages, compiles Rust, creates an NSIS installer, and writes SHA-256 metadata under `release/tauri/<profile>`. The Windows workflow is build-only and runs no tests.

Current local installers compile successfully and are unsigned:

- Preview: `release/tauri/preview/BSManager-Preview-1.6.0-x64-setup.exe`, SHA-256 `e32e3cc2ed0834e6fb816baee6b6c80d8c9f327cec8e18543acf4008c94fddb1`.
- Replacement: `release/tauri/replacement/BSManager-1.6.0-x64-setup.exe`, SHA-256 `8824506328847f99d36872d847334b1a3ab28911b27f412c39a7fb37c6f62948`.

## Implemented in the Rust host

- Separate preview and replacement identifiers and settings directories. The preview installer does not claim protocols or file extensions. Settings read/write, install-folder selection, and explicit legacy-settings import routes exist.
- Main window setup, native dialogs, window controls, drag/drop events, single-instance handling, protocol registration, local version catalog, installation-folder operations, local version discovery/edit/clone, shared-folder links, and launch-shortcut creation.
- Steam sidecar download routes, Meta login/session routes, Meta downloads, game launch, Windows VR runtime and sideload controls, and supporter data.
- ModelSaber search and lookup; model download, one-click installation, local listing, export, and deletion.
- BeatSaver map and playlist search/details; local map listing, deletion, import/export, download, and one-click install. Map installs stage archive extraction, compute the Beat Saber hash, validate it against BeatSaver, then move the map into the managed folder.
- Playlist listing, file import, install, delete, archive export, download, and one-click install. Download progress keeps the renderer’s existing payload shape. Local playlist statistics and cached song detail enrichment remain incomplete.
- Static configuration commands preserve unrecognized fields during file round trips. Request streaming and cancellation are bound to the originating window.

These routes compile. Their runtime behavior has not yet passed the user-led acceptance scenarios.

## Remaining parity work

- Complete route coverage is outstanding. Mod management, map-details cache reads, update download/install, uninstall, several OS-control operations, deep-link delivery for every scheme, and some configuration or library routes still need implementation or parity review. See the route names in `src/shared/models/ipc/ipc-routes.ts` and `src/main/ipcs` as the source inventory.
- Verify six-window behavior and window-specific capabilities. The shared renderer entry pages exist, but the auxiliary window lifecycle and remote authentication/webview isolation need acceptance review.
- Finish preview settings and library import behavior, replacement backup and legacy uninstallation, installer failure recovery, protocol ownership restoration, and rollback instructions. Replacement is not ready to replace an installed Electron build.
- Complete Meta and Steam credential migration, download resume/error paths, map and playlist cache parity, mods, content sharing, archive safety, relocation, shortcuts, LIV integration, proxy behavior, process tracking, and update signing configuration.
- Complete visual comparison at 100%, 125%, and 150% display scaling, in light and dark themes, with custom colors and default, minimum, and maximized window sizes. Build success does not establish visual or functional parity.

Keep the existing Electron installer for rollback. Current Tauri installers are local build artifacts only. Do not distribute or install the replacement profile until migration, installer recovery, runtime parity, and visual review are complete.
