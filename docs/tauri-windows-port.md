# Windows Tauri port status

The `rewrite/tauri-windows` branch keeps the Electron application as the rollback build and adds a Tauri 2 Windows build. The React renderer and all six HTML entry pages are shared. Version remains `1.6.0`.

## Local builds

Use Node 24 from `.nvmrc`, pnpm 10.34.4, Rust MSVC for `x86_64-pc-windows-msvc`, Microsoft C++ Build Tools, and WebView2. From the repository root, run:

```powershell
pnpm run build:tauri:win
pnpm run build:tauri:win:replacement
```

The commands build the renderer, compile Rust, create NSIS installers, and write SHA-256 metadata under `release/tauri/preview` and `release/tauri/replacement`. Current local artifacts are unsigned. The Windows GitHub workflow builds only the preview profile and runs no tests.

## Ported foundation

- Preview and replacement have separate Tauri identifiers. Preview settings use Tauri's preview app configuration directory. Replacement uses a separate identifier from preview.
- Main window dimensions, minimum size, frameless title bar, six renderer entry pages, single-instance callback, custom file-drop events, local version catalog, static setting read and write, native dialogs, protocol registration calls, and window controls compile in the Rust host.
- Preview installer does not declare protocol or file associations. Protocol registration happens only when the renderer calls the registration routes.
- ModelSaber lookup/search, model download, one-click install, local model listing/hash lookup, export, and deletion are implemented in Rust.
- BeatSaver map search, map lookup by ID or hashes, playlist search, and playlist details use the existing renderer request shapes. Map hash lookups retain an in-memory cache and tolerate failed hash batches as the Electron service does.
- Local map listing supports Info.dat versions 2, 3, and 4, computes Beat Saber SHA-1 hashes, records added dates, and streams the existing listing progress shape. Map deletion is limited to direct children of managed CustomLevels folders.
- BeatSaver map downloads and one-click installs validate HTTPS redirects, extract into a staging folder, verify the Beat Saber hash, and then install. Local map export preserves the current archive layout and streams file progress.
- Build metadata records profile, identifier, version, installer name, hash, and signing status.

## Parity still required

This branch is an early port foundation. It does not provide full BSManager parity and is not ready to replace an installed Electron build. Rust currently returns `NOT_IMPLEMENTED` for game operations it does not own.

- Steam and Meta discovery, login, download, verification, cancellation, game launch, map archive import, playlist operations, mod management, sharing, junctions, cache operations, relocation, shortcuts, LIV, proxy support, process tracking, and updater behavior still need Rust implementations.
- Installed-version discovery currently scans only `BSInstances` under the selected BSManager folder. Steam and Meta library discovery and legacy metadata import remain outstanding.
- Preview has no explicit importer for Electron settings. Replacement does not back up settings, invoke the Electron uninstaller, or install transactionally. Do not distribute or install the replacement artifact until those steps exist.
- Map and model deep links can reach their one-click Rust routes. Playlist install and `.bplist` file operations remain outstanding.
- Local media path authorization, complete IPC route coverage, progress streaming, cancellation ownership, and visual comparison at 100%, 125%, and 150% scaling remain outstanding.
- Build success proves frontend and Rust compilation plus installer packaging. It does not prove runtime behavior or UI parity. No tests or interactive desktop sessions were run.

Keep the existing Electron installer for rollback. Preview and replacement artifacts are build outputs only until the missing migration and runtime paths are implemented and reviewed.
