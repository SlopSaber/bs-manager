use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, atomic::{AtomicBool, Ordering}},
    time::Duration,
};

use futures_util::StreamExt;
use serde_json::{Value, json};
use tauri::{AppHandle, Runtime, WebviewWindow};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};
use zip::ZipArchive;

use crate::{SerializedError, serialized_error, versions};

const BEATMODS: &str = "https://beatmods.com";
const PENDING: &str = "IPA/Pending";
static MODS_BY_HASH: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();
static VERSION_MODS: OnceLock<Mutex<HashMap<String, Vec<Value>>>> = OnceLock::new();

pub async fn handle<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    channel: &str,
    args: &Value,
    response_channel: Option<&str>,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    match channel {
        "bs-mods.get-available-mods" => get_available_mods(args).await,
        "bs-mods.get-installed-mods" => get_installed_mods(app, args).await,
        "bs-mods.import-mods" => import_mods(app, window, response_channel, args, cancelled).await,
        "bs-mods.install-mods" => install_mods(app, window, response_channel, args, cancelled).await,
        "bs-mods.uninstall-mods" => uninstall_mods(app, window, response_channel, args, cancelled).await,
        "bs-mods.uninstall-all-mods" => uninstall_all_mods(app, window, response_channel, args, cancelled).await,
        "bs-mods.beatmods-up" => Ok(json!(is_up().await)),
        _ => Err(serialized_error("Unknown mod-management route", "UNKNOWN_ROUTE")),
    }
}

async fn get_available_mods(version: &Value) -> Result<Value, SerializedError> {
    let game_version = version.get("BSVersion").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Beat Saber version is missing", "INVALID_VERSION"))?;
    let key = game_version.to_owned();
    if let Some(cached) = VERSION_MODS.get_or_init(Default::default).lock().ok().and_then(|cache| cache.get(&key).cloned()) {
        return Ok(json!(cached));
    }
    let platform = if version.get("oculus").and_then(Value::as_bool).unwrap_or(false)
        || version.pointer("/metadata/store").and_then(Value::as_str) == Some("oculus") { "oculuspc" } else { "steampc" };
    let mut url = reqwest::Url::parse(&format!("{BEATMODS}/api/mods"))
        .map_err(|error| serialized_error(error.to_string(), "BEATMODS_URL_FAILED"))?;
    url.query_pairs_mut()
        .append_pair("status", "verified")
        .append_pair("gameVersion", game_version)
        .append_pair("gameName", "BeatSaber")
        .append_pair("platform", platform);
    let response = get_json(url).await?;
    let mods = response.get("mods").and_then(Value::as_array).into_iter().flatten()
        .filter_map(|item| Some(json!({ "mod": item.get("mod")?, "version": item.get("latest")? })))
        .collect::<Vec<_>>();
    update_hash_cache(mods.iter().filter_map(|item| item.get("version")).cloned());
    VERSION_MODS.get_or_init(Default::default).lock()
        .map_err(|_| serialized_error("Mod cache is unavailable", "CACHE_FAILED"))?
        .insert(key, mods.clone());
    Ok(json!(mods))
}

async fn get_installed_mods<R: Runtime>(app: &AppHandle<R>, version: &Value) -> Result<Value, SerializedError> {
    let root = versions::version_path(app, version)?;
    let bsipa_path = root.join("Beat Saber_Data/Managed/IPA.Injector.dll");
    let mut candidates = Vec::new();
    if bsipa_path.is_file() { candidates.push((bsipa_path, true, false)); }
    let dirs = [
        ("IPA/Pending/Plugins", true, false), ("Plugins", true, false),
        ("IPA/Pending/Libs", false, true), ("Libs", false, true),
    ];
    for (relative, is_plugin, is_lib) in dirs {
        collect_mod_files(&root.join(relative), &mut candidates, is_plugin, is_lib)?;
    }
    let lookups = candidates.iter().map(|(path, _, _)| async move {
        let hash = md5_file(path).await?;
        Ok::<_, SerializedError>((lookup_hash(&hash).await, path.clone()))
    });
    let lookup_results = futures_util::future::join_all(lookups).await;
    let mut found = HashMap::<String, Value>::new();
    let mut manifest_matches = Vec::<Value>::new();
    for (candidate, lookup) in candidates.into_iter().zip(lookup_results) {
        let (path, is_plugin, is_lib) = candidate;
        let Ok((Some(mod_version), _)) = lookup else { continue; };
        let is_bsipa_injector = path == root.join("Beat Saber_Data/Managed/IPA.Injector.dll");
        if !is_bsipa_injector && mod_version.get("contentHashes").and_then(Value::as_array).is_some_and(|hashes| {
            hashes.iter().any(|content| content.get("path").and_then(Value::as_str)
                .is_some_and(|path| path.contains("IPA.exe")))
        }) {
            continue;
        }
        if path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("manifest")) {
            manifest_matches.push(mod_version);
            continue;
        }
        let Some(id) = mod_version.get("id") else { continue; };
        if is_plugin {
            found.entry(id.to_string()).or_insert(mod_version);
        } else if is_lib {
            if let Some(index) = manifest_matches.iter().position(|manifest| manifest.get("id") == Some(id)) {
                manifest_matches.remove(index);
                found.entry(id.to_string()).or_insert(mod_version);
            }
        }
    }
    Ok(json!(found.into_values().collect::<Vec<_>>()))
}

fn collect_mod_files(root: &Path, output: &mut Vec<(PathBuf, bool, bool)>, is_plugin: bool, is_lib: bool) -> Result<(), SerializedError> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(serialized_error(error.to_string(), "MOD_SCAN_FAILED")),
    };
    for entry in entries {
        let entry = entry.map_err(|error| serialized_error(error.to_string(), "MOD_SCAN_FAILED"))?;
        let kind = entry.file_type().map_err(|error| serialized_error(error.to_string(), "MOD_SCAN_FAILED"))?;
        if kind.is_symlink() { continue; }
        if kind.is_dir() {
            collect_mod_files(&entry.path(), output, is_plugin, is_lib)?;
        } else if kind.is_file() && entry.path().extension().and_then(|value| value.to_str())
            .is_some_and(|extension| ["dll", "exe", "manifest"].iter().any(|accepted| extension.eq_ignore_ascii_case(accepted))) {
            output.push((entry.path(), is_plugin, is_lib));
        }
    }
    Ok(())
}

async fn import_mods<R: Runtime>(
    app: &AppHandle<R>, window: &WebviewWindow<R>, response_channel: Option<&str>, args: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let paths = args.get("paths").and_then(Value::as_array)
        .ok_or_else(|| serialized_error("Mod paths must be an array", "INVALID_PATHS"))?;
    let version = args.get("version").unwrap_or(&Value::Null);
    let root = versions::version_path(app, version)?;
    let destination = root.join(PENDING);
    let paths = paths.iter().filter_map(Value::as_str).map(PathBuf::from).collect::<Vec<_>>();
    let total = paths.len() as u64;
    let mut current = 0_u64;
    let mut imported = 0_u64;
    let mut file_count = 0_u64;
    emit_progress(window, response_channel, current, total, Value::Null, None)?;
    for source in paths {
        if cancelled.load(Ordering::Relaxed) { return Err(serialized_error("Mod import was cancelled", "REQUEST_CANCELLED")); }
        let mut files = Vec::new();
        if source.is_file() {
            match source.extension().and_then(|value| value.to_str()).unwrap_or_default().to_ascii_lowercase().as_str() {
                "dll" => {
                    let name = source.file_name().unwrap_or_default();
                    let relative = PathBuf::from("Plugins").join(name);
                    let target = destination.join(&relative);
                    tokio::fs::create_dir_all(target.parent().unwrap_or(&destination)).await
                        .map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))?;
                    tokio::fs::copy(&source, &target).await
                        .map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))?;
                    files.push(relative.to_string_lossy().replace('\\', "/"));
                }
                "zip" => {
                    let source_for_extract = source.clone();
                    let destination_for_extract = destination.clone();
                    let cancelled_for_extract = cancelled.clone();
                    files = tokio::task::spawn_blocking(move || extract_external_zip(&source_for_extract, &destination_for_extract, &cancelled_for_extract)).await
                        .map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))??;
                }
                _ => {}
            }
        }
        if !files.is_empty() {
            imported += 1;
            file_count += files.len() as u64;
            emit_progress(window, response_channel, current + 1, total, json!({
                "name": source.file_stem().unwrap_or_default().to_string_lossy(), "files": files,
            }), None)?;
        } else {
            emit_progress(window, response_channel, current + 1, total, Value::Null, None)?;
        }
        current += 1;
    }
    if imported == 0 {
        return Err(serialized_error("No DLL was found in the selected files", "no-dlls"));
    }
    log::info!("Imported {imported} mods from {} paths, adding {file_count} files", total);
    Ok(Value::Null)
}

fn extract_external_zip(source: &Path, destination: &Path, cancelled: &AtomicBool) -> Result<Vec<String>, SerializedError> {
    let input = File::open(source).map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))?;
    let mut archive = ZipArchive::new(input).map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))?;
    let mut structured = false;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))?;
        let name = entry.name().map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))?
            .replace('\\', "/");
        if name.starts_with("Plugins/") || name.starts_with("Libs/") { structured = true; }
    }
    let staging = destination.join(format!(".import-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&staging).map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))?;
    let result = (|| {
        let mut added = Vec::new();
        for index in 0..archive.len() {
            if cancelled.load(Ordering::Relaxed) { return Err(serialized_error("Mod import was cancelled", "REQUEST_CANCELLED")); }
            let mut entry = archive.by_index(index).map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))?;
            if entry.is_dir() { continue; }
            if entry.unix_mode().is_some_and(|mode| mode & 0o170000 == 0o120000) {
                return Err(serialized_error("Mod archive contains a symbolic link", "INVALID_ARCHIVE"));
            }
            let name = entry.name().map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))?;
            let relative = safe_relative_path(&name)?;
            let output_relative = if structured { relative.clone() } else { PathBuf::from("Plugins").join(relative.file_name().unwrap_or_default()) };
            let output = staging.join(&output_relative);
            fs::create_dir_all(output.parent().unwrap_or(&staging)).map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))?;
            let mut file = File::create(&output).map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))?;
            io::copy(&mut entry, &mut file).map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))?;
            added.push(output_relative);
        }
        if !added.iter().any(|path| path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("dll"))) {
            return Err(serialized_error("No DLL was found in the archive", "NO_MOD_DLL"));
        }
        move_staged_files(&staging, destination, &added)?;
        Ok(added.into_iter().map(|path| path.to_string_lossy().replace('\\', "/")).collect())
    })();
    let _ = fs::remove_dir_all(&staging);
    result
}

async fn install_mods<R: Runtime>(
    app: &AppHandle<R>, window: &WebviewWindow<R>, response_channel: Option<&str>, args: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let mods = args.get("mods").and_then(Value::as_array)
        .ok_or_else(|| serialized_error("Mods must be an array", "INVALID_MODS"))?;
    if mods.is_empty() { return Err(serialized_error("No mods to install", "no-mods")); }
    let version = args.get("version").unwrap_or(&Value::Null);
    let root = versions::version_path(app, version)?;
    let mut ordered = mods.clone();
    if let Some(index) = ordered.iter().position(|item| item.pointer("/mod/name").and_then(Value::as_str).is_some_and(|name| name.eq_ignore_ascii_case("bsipa"))) {
        if index > 0 { let bsipa = ordered.remove(index); ordered.insert(0, bsipa); }
    }
    let total = ordered.len() as u64;
    let mut current = 0_u64;
    emit_progress(window, response_channel, current, total, Value::Null, None)?;
    for full_mod in ordered {
        if cancelled.load(Ordering::Relaxed) { return Err(serialized_error("Mod installation was cancelled", "REQUEST_CANCELLED")); }
        let mod_version = full_mod.get("version").ok_or_else(|| serialized_error("Mod version is missing", "INVALID_MOD"))?;
        let is_bsipa = full_mod.pointer("/mod/name").and_then(Value::as_str).is_some_and(|name| name.eq_ignore_ascii_case("bsipa"));
        let files = download_verified_mod(mod_version, &cancelled).await?;
        let destination = if is_bsipa { root.clone() } else { root.join(PENDING) };
        let destination_for_install = destination.clone();
        let root_for_install = root.clone();
        let files_for_install = files.clone();
        let install_result = tokio::task::spawn_blocking(move || {
            if is_bsipa { clear_ipa_folder(&root_for_install)?; }
            fs::create_dir_all(&destination_for_install).map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
            move_staged_files(&files_for_install.0, &destination_for_install, &files_for_install.1)
        }).await.map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
        if install_result.is_err() { let _ = tokio::fs::remove_dir_all(&files.0).await; }
        install_result?;
        if is_bsipa && !root.join("winhttp.dll").is_file() && !execute_ipa(&root, &["-n"]).await? {
            return Err(serialized_error("BSIPA installation command failed", "cannot-install-bsipa"));
        }
        current += 1;
        emit_progress(window, response_channel, current, total, Value::Null, None)?;
    }
    Ok(Value::Null)
}

async fn download_verified_mod(version: &Value, cancelled: &AtomicBool) -> Result<(PathBuf, Vec<PathBuf>), SerializedError> {
    let zip_hash = version.get("zipHash").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Mod archive hash is missing", "INVALID_MOD"))?;
    let url = format!("{BEATMODS}/cdn/mod/{}.zip", urlencoding::encode(zip_hash));
    let client = crate::proxy::client_builder().connect_timeout(Duration::from_secs(20)).timeout(Duration::from_secs(180))
        .user_agent(format!("BSManager/{}", env!("CARGO_PKG_VERSION"))).build()
        .map_err(|error| serialized_error(error.to_string(), "MOD_DOWNLOAD_FAILED"))?;
    if cancelled.load(Ordering::Relaxed) {
        return Err(serialized_error("Mod download was cancelled", "REQUEST_CANCELLED"));
    }
    let response = tokio::select! {
        response = client.get(url).send() => response
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| serialized_error(error.to_string(), "MOD_DOWNLOAD_FAILED"))?,
        _ = wait_for_cancellation(cancelled) => {
            return Err(serialized_error("Mod download was cancelled", "REQUEST_CANCELLED"));
        }
    };
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::select! {
            chunk = stream.next() => chunk,
            _ = wait_for_cancellation(cancelled) => {
                return Err(serialized_error("Mod download was cancelled", "REQUEST_CANCELLED"));
            }
        };
        let Some(chunk) = chunk else { break; };
        bytes.extend_from_slice(&chunk.map_err(|error| serialized_error(error.to_string(), "MOD_DOWNLOAD_FAILED"))?);
    }
    let expected = version.get("contentHashes").and_then(Value::as_array).cloned().unwrap_or_default();
    let staging = std::env::temp_dir().join(format!("bsmanager-mod-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&staging).map_err(|error| serialized_error(error.to_string(), "MOD_DOWNLOAD_FAILED"))?;
    let result = (|| {
        let mut archive = ZipArchive::new(io::Cursor::new(bytes))
            .map_err(|error| serialized_error(error.to_string(), "MOD_ARCHIVE_FAILED"))?;
        let mut matches = 0_usize;
        let mut paths = Vec::new();
        for index in 0..archive.len() {
            if cancelled.load(Ordering::Relaxed) { return Err(serialized_error("Mod installation was cancelled", "REQUEST_CANCELLED")); }
            let mut entry = archive.by_index(index).map_err(|error| serialized_error(error.to_string(), "MOD_ARCHIVE_FAILED"))?;
            if entry.is_dir() { continue; }
            if entry.unix_mode().is_some_and(|mode| mode & 0o170000 == 0o120000) {
                return Err(serialized_error("Mod archive contains a symbolic link", "INVALID_ARCHIVE"));
            }
            let name = entry.name().map_err(|error| serialized_error(error.to_string(), "MOD_ARCHIVE_FAILED"))?;
            let relative = safe_relative_path(&name)?;
            let output = staging.join(&relative);
            fs::create_dir_all(output.parent().unwrap_or(&staging)).map_err(|error| serialized_error(error.to_string(), "MOD_ARCHIVE_FAILED"))?;
            let mut file = File::create(&output).map_err(|error| serialized_error(error.to_string(), "MOD_ARCHIVE_FAILED"))?;
            let mut buffer = Vec::new();
            entry.read_to_end(&mut buffer).map_err(|error| serialized_error(error.to_string(), "MOD_ARCHIVE_FAILED"))?;
            let hash = format!("{:x}", md5::compute(&buffer));
            if expected.iter().any(|item| item.get("hash").and_then(Value::as_str).is_some_and(|expected| expected.eq_ignore_ascii_case(&hash))) {
                matches += 1;
            }
            io::Write::write_all(&mut file, &buffer).map_err(|error| serialized_error(error.to_string(), "MOD_ARCHIVE_FAILED"))?;
            paths.push(relative);
        }
        if matches != expected.len() {
            return Err(serialized_error("Mod archive contents do not match BeatMods", "MOD_HASH_MISMATCH"));
        }
        Ok(paths)
    })();
    match result {
        Ok(paths) => Ok((staging, paths)),
        Err(error) => { let _ = fs::remove_dir_all(staging); Err(error) }
    }
}

async fn wait_for_cancellation(cancelled: &AtomicBool) {
    while !cancelled.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn uninstall_mods<R: Runtime>(
    app: &AppHandle<R>, window: &WebviewWindow<R>, response_channel: Option<&str>, args: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let mods = args.get("mods").and_then(Value::as_array)
        .ok_or_else(|| serialized_error("Mods must be an array", "INVALID_MODS"))?;
    if mods.is_empty() { return Err(serialized_error("No mods to uninstall", "no-mods")); }
    let version = args.get("version").unwrap_or(&Value::Null);
    let root = versions::version_path(app, version)?;
    let total = mods.len() as u64;
    let mut current = 0_u64;
    emit_progress(window, response_channel, current, total, Value::Null, None)?;
    for full_mod in mods {
        if cancelled.load(Ordering::Relaxed) { return Err(serialized_error("Mod removal was cancelled", "REQUEST_CANCELLED")); }
        uninstall_one(&root, full_mod).await?;
        current += 1;
        emit_progress(window, response_channel, current, total, Value::Null, None)?;
    }
    Ok(Value::Null)
}

async fn uninstall_all_mods<R: Runtime>(
    app: &AppHandle<R>, window: &WebviewWindow<R>, response_channel: Option<&str>, version: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let available = get_available_mods(version).await?.as_array().cloned().unwrap_or_default();
    let installed = get_installed_mods(app, version).await?.as_array().cloned().unwrap_or_default();
    let mods = installed.into_iter().filter_map(|installed| {
        let mod_id = installed.get("modId")?;
        available.iter().find(|available| available.pointer("/mod/id") == Some(mod_id))
            .cloned().or_else(|| {
                let name = installed.get("name")?.as_str()?;
                Some(json!({ "mod": { "name": name }, "version": installed }))
            })
    }).collect::<Vec<_>>();
    let root = versions::version_path(app, version)?;
    let total = mods.len() as u64;
    let mut current = 0_u64;
    emit_progress(window, response_channel, current, total, Value::Null, None)?;
    for full_mod in &mods {
        if cancelled.load(Ordering::Relaxed) { return Err(serialized_error("Mod removal was cancelled", "REQUEST_CANCELLED")); }
        uninstall_one(&root, full_mod).await?;
        current += 1;
        emit_progress(window, response_channel, current, total, Value::Null, None)?;
    }
    for relative in ["Plugins", "Libs", "IPA"] {
        let path = root.join(relative);
        if path.exists() {
            tokio::fs::remove_dir_all(&path).await
                .map_err(|error| serialized_error(error.to_string(), "MOD_UNINSTALL_FAILED"))?;
        }
    }
    Ok(Value::Null)
}

async fn uninstall_one(root: &Path, full_mod: &Value) -> Result<(), SerializedError> {
    let version = full_mod.get("version").unwrap_or(&Value::Null);
    let is_bsipa = full_mod.pointer("/mod/name").and_then(Value::as_str)
        .is_some_and(|name| name.eq_ignore_ascii_case("bsipa"));
    if is_bsipa {
        if root.join("IPA").is_dir() && root.join("IPA.exe").is_file() {
            let _ = execute_ipa(root, &["--revert", "-n"]).await?;
        }
    }
    let hashes = version.get("contentHashes").and_then(Value::as_array).cloned().unwrap_or_default();
    for item in hashes {
        let Some(relative) = item.get("path").and_then(Value::as_str) else { continue; };
        let relative = if is_bsipa {
            relative.replace("IPA/", "").replace("Data", "Beat Saber_Data")
        } else { relative.to_owned() };
        let relative_path = match safe_relative_path(&relative) { Ok(path) => path, Err(_) => continue };
        for candidate in [root.join(&relative_path), root.join(PENDING).join(&relative_path)] {
            if candidate.is_file() {
                tokio::fs::remove_file(candidate).await
                    .map_err(|error| serialized_error(error.to_string(), "MOD_UNINSTALL_FAILED"))?;
            } else if candidate.is_dir() {
                tokio::fs::remove_dir_all(candidate).await
                    .map_err(|error| serialized_error(error.to_string(), "MOD_UNINSTALL_FAILED"))?;
            }
        }
    }
    Ok(())
}

async fn execute_ipa(root: &Path, args: &[&str]) -> Result<bool, SerializedError> {
    let ipa = root.join("IPA.exe");
    let game = root.join("Beat Saber.exe");
    if !ipa.is_file() || !game.is_file() { return Ok(false); }
    let child = Command::new(ipa).arg(game).args(args).current_dir(root).kill_on_drop(true).spawn()
        .map_err(|error| serialized_error(error.to_string(), "IPA_START_FAILED"))?;
    match timeout(Duration::from_secs(30), child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(output.status.success()),
        Ok(Err(error)) => Err(serialized_error(error.to_string(), "IPA_FAILED")),
        Err(_) => Ok(false),
    }
}

fn clear_ipa_folder(root: &Path) -> Result<(), SerializedError> {
    let ipa = root.join("IPA");
    let entries = match fs::read_dir(&ipa) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(serialized_error(error.to_string(), "MOD_INSTALL_FAILED")),
    };
    for entry in entries {
        let entry = entry.map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
        if matches!(entry.file_name().to_str(), Some("Backups" | "Pending")) { continue; }
        let kind = entry.file_type().map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
        if kind.is_symlink() { return Err(serialized_error("IPA folder contains a symbolic link", "INVALID_PATH")); }
        if kind.is_dir() { fs::remove_dir_all(entry.path()) } else { fs::remove_file(entry.path()) }
            .map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
    }
    Ok(())
}

fn move_staged_files(staging: &Path, destination: &Path, files: &[PathBuf]) -> Result<(), SerializedError> {
    fs::create_dir_all(destination).map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
    for relative in files {
        let relative = safe_relative_path(&relative.to_string_lossy())?;
        let source = staging.join(&relative);
        let target = destination.join(&relative);
        if !source.is_file() { continue; }
        fs::create_dir_all(target.parent().unwrap_or(destination))
            .map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
        if target.exists() {
            if target.is_dir() { fs::remove_dir_all(&target) } else { fs::remove_file(&target) }
                .map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
        }
        fs::rename(&source, &target).or_else(|_| fs::copy(&source, &target).map(|_| ()))
            .map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
    }
    let _ = fs::remove_dir_all(staging);
    Ok(())
}

fn safe_relative_path(value: &str) -> Result<PathBuf, SerializedError> {
    let path = Path::new(value);
    if path.is_absolute() {
        return Err(serialized_error("Mod path escapes the game folder", "INVALID_PATH"));
    }
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => safe.push(part),
            Component::CurDir => {},
            _ => return Err(serialized_error("Mod path escapes the game folder", "INVALID_PATH")),
        }
    }
    if safe.as_os_str().is_empty() {
        return Err(serialized_error("Mod path is empty", "INVALID_PATH"));
    }
    Ok(safe)
}

async fn md5_file(path: &Path) -> Result<String, SerializedError> {
    let mut file = tokio::fs::File::open(path).await.map_err(|error| serialized_error(error.to_string(), "MOD_HASH_FAILED"))?;
    let mut context = md5::Context::new();
    let mut buffer = vec![0_u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer).await.map_err(|error| serialized_error(error.to_string(), "MOD_HASH_FAILED"))?;
        if read == 0 { break; }
        context.consume(&buffer[..read]);
    }
    Ok(format!("{:x}", context.compute()))
}

async fn lookup_hash(hash: &str) -> Option<Value> {
    if let Some(cached) = MODS_BY_HASH.get_or_init(Default::default).lock().ok().and_then(|cache| cache.get(hash).cloned()) {
        return Some(cached);
    }
    let url = reqwest::Url::parse_with_params(&format!("{BEATMODS}/api/hashlookup"), &[("hash", hash)]).ok()?;
    let response = get_json(url).await.ok()?;
    let versions = response.get("modVersions").and_then(Value::as_array)?;
    update_hash_cache(versions.iter().cloned());
    versions.first().cloned()
}

fn update_hash_cache(versions: impl IntoIterator<Item = Value>) {
    let cache = MODS_BY_HASH.get_or_init(Default::default);
    if let Ok(mut cache) = cache.lock() {
        for version in versions {
            if let Some(contents) = version.get("contentHashes").and_then(Value::as_array) {
                for content in contents {
                    if let (Some(hash), Some(_)) = (content.get("hash").and_then(Value::as_str), version.get("id")) {
                        cache.insert(hash.to_ascii_lowercase(), version.clone());
                    }
                }
            }
        }
    }
}

async fn get_json(url: reqwest::Url) -> Result<Value, SerializedError> {
    let response = crate::proxy::client_builder().connect_timeout(Duration::from_secs(20)).timeout(Duration::from_secs(30))
        .user_agent(format!("BSManager/{}", env!("CARGO_PKG_VERSION"))).build()
        .map_err(|error| serialized_error(error.to_string(), "BEATMODS_REQUEST_FAILED"))?
        .get(url).send().await.and_then(reqwest::Response::error_for_status)
        .map_err(|error| serialized_error(error.to_string(), "BEATMODS_REQUEST_FAILED"))?;
    let body = response.text().await.map_err(|error| serialized_error(error.to_string(), "BEATMODS_RESPONSE_FAILED"))?;
    serde_json::from_str(&body).map_err(|error| serialized_error(error.to_string(), "BEATMODS_RESPONSE_FAILED"))
}

async fn is_up() -> bool {
    let Ok(url) = reqwest::Url::parse(&format!("{BEATMODS}/api/status")) else { return false; };
    get_json(url).await.is_ok()
}

fn emit_progress<R: Runtime>(
    window: &WebviewWindow<R>, channel: Option<&str>, current: u64, total: u64, data: Value, last_error: Option<Value>,
) -> Result<(), SerializedError> {
    if let Some(channel) = channel {
        let mut value = json!({ "current": current, "total": total });
        if let Some(object) = value.as_object_mut() {
            if !data.is_null() { object.insert("data".into(), data); }
            if let Some(error) = last_error { object.insert("lastError".into(), error); }
        }
        crate::emit_response_value(window, Some(channel), value)?;
    }
    Ok(())
}
