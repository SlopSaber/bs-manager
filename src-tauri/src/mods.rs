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
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, process::Command, time::timeout};
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
        "bs-mods.get-available-mods" => get_available_mods(args, &cancelled).await,
        "bs-mods.get-installed-mods" => get_installed_mods(app, args, &cancelled).await,
        "bs-mods.import-mods" => import_mods(app, window, response_channel, args, cancelled).await,
        "bs-mods.install-mods" => install_mods(app, window, response_channel, args, cancelled).await,
        "bs-mods.uninstall-mods" => uninstall_mods(app, window, response_channel, args, cancelled).await,
        "bs-mods.uninstall-all-mods" => uninstall_all_mods(app, window, response_channel, args, cancelled).await,
        "bs-mods.beatmods-up" => Ok(json!(is_up(&cancelled).await)),
        _ => Err(serialized_error("Unknown mod-management route", "UNKNOWN_ROUTE")),
    }
}

async fn get_available_mods(version: &Value, cancelled: &AtomicBool) -> Result<Value, SerializedError> {
    if cancelled.load(Ordering::Relaxed) { return Err(serialized_error("Mod lookup was cancelled", "REQUEST_CANCELLED")); }
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
    let response = get_json(url, cancelled).await?;
    if cancelled.load(Ordering::Relaxed) { return Err(serialized_error("Mod lookup was cancelled", "REQUEST_CANCELLED")); }
    let mods = response.get("mods").and_then(Value::as_array).into_iter().flatten()
        .filter_map(|item| Some(json!({ "mod": item.get("mod")?, "version": item.get("latest")? })))
        .collect::<Vec<_>>();
    update_hash_cache(mods.iter().filter_map(|item| item.get("version")).cloned());
    VERSION_MODS.get_or_init(Default::default).lock()
        .map_err(|_| serialized_error("Mod cache is unavailable", "CACHE_FAILED"))?
        .insert(key, mods.clone());
    Ok(json!(mods))
}

async fn get_installed_mods<R: Runtime>(app: &AppHandle<R>, version: &Value, cancelled: &AtomicBool) -> Result<Value, SerializedError> {
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
        Ok::<_, SerializedError>((lookup_hash(&hash, cancelled).await, path.clone()))
    });
    let lookup_results = futures_util::future::join_all(lookups).await;
    if cancelled.load(Ordering::Relaxed) { return Err(serialized_error("Mod lookup was cancelled", "REQUEST_CANCELLED")); }
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
                    copy_raw_dll(&source, &target, &cancelled).await?;
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

async fn copy_raw_dll(source: &Path, target: &Path, cancelled: &AtomicBool) -> Result<(), SerializedError> {
    let staging = target.with_file_name(format!(".bsmanager-import-{}", uuid::Uuid::new_v4()));
    let backup = target.with_file_name(format!(".bsmanager-import-backup-{}", uuid::Uuid::new_v4()));
    let result = async {
        let mut input = tokio::fs::File::open(source).await
            .map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))?;
        let mut output = tokio::fs::OpenOptions::new().write(true).create_new(true).open(&staging).await
            .map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return Err(serialized_error("Mod import was cancelled", "REQUEST_CANCELLED"));
            }
            let read = input.read(&mut buffer).await
                .map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))?;
            if read == 0 { break; }
            output.write_all(&buffer[..read]).await
                .map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))?;
        }
        output.flush().await
            .map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))?;
        drop(output);
        if cancelled.load(Ordering::Relaxed) {
            return Err(serialized_error("Mod import was cancelled", "REQUEST_CANCELLED"));
        }
        let had_target = match tokio::fs::symlink_metadata(target).await {
            Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_dir() => {
                return Err(serialized_error("Mod destination cannot be a symbolic link or folder", "INVALID_PATH"));
            }
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(serialized_error(error.to_string(), "MOD_IMPORT_FAILED")),
        };
        if had_target {
            tokio::fs::rename(target, &backup).await
                .map_err(|error| serialized_error(error.to_string(), "MOD_IMPORT_FAILED"))?;
        }
        if let Err(error) = tokio::fs::rename(&staging, target).await {
            if had_target { let _ = tokio::fs::rename(&backup, target).await; }
            return Err(serialized_error(error.to_string(), "MOD_IMPORT_FAILED"));
        }
        if had_target { let _ = tokio::fs::remove_file(backup).await; }
        Ok(())
    }.await;
    if result.is_err() { let _ = tokio::fs::remove_file(staging).await; }
    result
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
            crate::archive_io::copy_cancellable(&mut entry, &mut file, cancelled).map_err(|error| {
                if cancelled.load(Ordering::Relaxed) {
                    serialized_error("Mod import was cancelled", "REQUEST_CANCELLED")
                } else {
                    serialized_error(error.to_string(), "MOD_IMPORT_FAILED")
                }
            })?;
            added.push(output_relative);
        }
        if !added.iter().any(|path| path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("dll"))) {
            return Err(serialized_error("No DLL was found in the archive", "NO_MOD_DLL"));
        }
        move_staged_files(&staging, destination, &added, cancelled)?;
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
        let cancelled_for_install = cancelled.clone();
        let install_result = tokio::task::spawn_blocking(move || {
            let ipa_backup = if is_bsipa { stage_ipa_contents(&root_for_install)? } else { None };
            match move_staged_files(&files_for_install.0, &destination_for_install, &files_for_install.1, &cancelled_for_install) {
                Ok(()) => {
                    if let Some(backup) = ipa_backup {
                        if let Err(error) = fs::remove_dir_all(&backup) {
                            log::warn!("Could not remove temporary IPA backup {}: {error}", backup.display());
                        }
                    }
                    Ok(())
                }
                Err(error) => {
                    if let Some(backup) = ipa_backup {
                        if let Err(restore_error) = restore_ipa_contents(&root_for_install, &backup) {
                            return Err(serialized_error(
                                format!("{}; previous IPA files remain in {}: {}", error.message, backup.display(), restore_error.message),
                                "MOD_INSTALL_FAILED",
                            ));
                        }
                    }
                    Err(error)
                }
            }
        }).await.map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
        if install_result.is_err() { let _ = tokio::fs::remove_dir_all(&files.0).await; }
        install_result?;
        if is_bsipa && !root.join("winhttp.dll").is_file() && !execute_ipa(&root, &["-n"], &cancelled).await? {
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
    let archive_path = std::env::temp_dir().join(format!("bsmanager-mod-{}.zip", uuid::Uuid::new_v4()));
    let mut stream = response.bytes_stream();
    let download_result = async {
        let mut output = tokio::fs::OpenOptions::new().write(true).create_new(true).open(&archive_path).await
            .map_err(|error| serialized_error(error.to_string(), "MOD_DOWNLOAD_FAILED"))?;
        loop {
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                _ = wait_for_cancellation(cancelled) => {
                    return Err(serialized_error("Mod download was cancelled", "REQUEST_CANCELLED"));
                }
            };
            let Some(chunk) = chunk else { break; };
            let chunk = chunk.map_err(|error| serialized_error(error.to_string(), "MOD_DOWNLOAD_FAILED"))?;
            output.write_all(&chunk).await
                .map_err(|error| serialized_error(error.to_string(), "MOD_DOWNLOAD_FAILED"))?;
        }
        output.flush().await
            .map_err(|error| serialized_error(error.to_string(), "MOD_DOWNLOAD_FAILED"))?;
        output.sync_all().await
            .map_err(|error| serialized_error(error.to_string(), "MOD_DOWNLOAD_FAILED"))?;
        Ok::<(), SerializedError>(())
    }.await;
    if let Err(error) = download_result {
        let _ = tokio::fs::remove_file(&archive_path).await;
        return Err(error);
    }
    let expected = version.get("contentHashes").and_then(Value::as_array).cloned().unwrap_or_default();
    let staging = std::env::temp_dir().join(format!("bsmanager-mod-{}", uuid::Uuid::new_v4()));
    if let Err(error) = fs::create_dir_all(&staging) {
        let _ = fs::remove_file(&archive_path);
        return Err(serialized_error(error.to_string(), "MOD_DOWNLOAD_FAILED"));
    }
    let result = (|| {
        let archive_file = File::open(&archive_path).map_err(|error| serialized_error(error.to_string(), "MOD_ARCHIVE_FAILED"))?;
        let mut archive = ZipArchive::new(archive_file)
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
            let mut hasher = md5::Context::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                if cancelled.load(Ordering::Relaxed) {
                    return Err(serialized_error("Mod installation was cancelled", "REQUEST_CANCELLED"));
                }
                let read = entry.read(&mut buffer)
                    .map_err(|error| serialized_error(error.to_string(), "MOD_ARCHIVE_FAILED"))?;
                if read == 0 { break; }
                hasher.consume(&buffer[..read]);
                io::Write::write_all(&mut file, &buffer[..read])
                    .map_err(|error| serialized_error(error.to_string(), "MOD_ARCHIVE_FAILED"))?;
            }
            let hash = format!("{:x}", hasher.compute());
            if expected.iter().any(|item| item.get("hash").and_then(Value::as_str).is_some_and(|expected| expected.eq_ignore_ascii_case(&hash))) {
                matches += 1;
            }
            paths.push(relative);
        }
        if matches != expected.len() {
            return Err(serialized_error("Mod archive contents do not match BeatMods", "MOD_HASH_MISMATCH"));
        }
        Ok(paths)
    })();
    let _ = fs::remove_file(&archive_path);
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
        uninstall_one(&root, full_mod, &cancelled).await?;
        current += 1;
        emit_progress(window, response_channel, current, total, Value::Null, None)?;
    }
    Ok(Value::Null)
}

async fn uninstall_all_mods<R: Runtime>(
    app: &AppHandle<R>, window: &WebviewWindow<R>, response_channel: Option<&str>, version: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    if cancelled.load(Ordering::Relaxed) { return Err(serialized_error("Mod removal was cancelled", "REQUEST_CANCELLED")); }
    let available = get_available_mods(version, &cancelled).await?.as_array().cloned().unwrap_or_default();
    if cancelled.load(Ordering::Relaxed) { return Err(serialized_error("Mod removal was cancelled", "REQUEST_CANCELLED")); }
    let installed = get_installed_mods(app, version, &cancelled).await?.as_array().cloned().unwrap_or_default();
    let mods = installed.into_iter().filter_map(|installed| {
        let mod_id = installed.get("modId")?;
        if let Some(available) = available.iter().find(|available| available.pointer("/mod/id") == Some(mod_id)) {
            return Some(available.clone());
        }

        let display_name = installed.get("name").and_then(Value::as_str).map(str::to_owned)
            .or_else(|| installed.get("contentHashes").and_then(Value::as_array).and_then(|hashes| {
                hashes.iter().find_map(|hash| {
                    let path = hash.get("path")?.as_str()?;
                    let filename = path.rsplit(|character| character == '/' || character == '\\').next()?;
                    let is_dll = filename.to_ascii_lowercase().ends_with(".dll");
                    is_dll.then(|| filename[..filename.len() - 4].to_owned())
                })
            }))
            .unwrap_or_else(|| format!("Unknown mod ({mod_id})"));
        Some(json!({ "mod": { "name": display_name }, "version": installed }))
    }).collect::<Vec<_>>();
    let root = versions::version_path(app, version)?;
    let total = mods.len() as u64;
    let mut current = 0_u64;
    emit_progress(window, response_channel, current, total, Value::Null, None)?;
    for full_mod in &mods {
        if cancelled.load(Ordering::Relaxed) { return Err(serialized_error("Mod removal was cancelled", "REQUEST_CANCELLED")); }
        uninstall_one(&root, full_mod, &cancelled).await?;
        current += 1;
        emit_progress(window, response_channel, current, total, Value::Null, None)?;
    }
    for relative in ["Plugins", "Libs", "IPA"] {
        if cancelled.load(Ordering::Relaxed) { return Err(serialized_error("Mod removal was cancelled", "REQUEST_CANCELLED")); }
        let path = root.join(relative);
        if path.exists() {
            tokio::fs::remove_dir_all(&path).await
                .map_err(|error| serialized_error(error.to_string(), "MOD_UNINSTALL_FAILED"))?;
        }
    }
    Ok(Value::Null)
}

async fn uninstall_one(root: &Path, full_mod: &Value, cancelled: &AtomicBool) -> Result<(), SerializedError> {
    if cancelled.load(Ordering::Relaxed) { return Err(serialized_error("Mod removal was cancelled", "REQUEST_CANCELLED")); }
    let version = full_mod.get("version").unwrap_or(&Value::Null);
    let is_bsipa = full_mod.pointer("/mod/name").and_then(Value::as_str)
        .is_some_and(|name| name.eq_ignore_ascii_case("bsipa"));
    if is_bsipa {
        if root.join("IPA").is_dir() && root.join("IPA.exe").is_file() {
            let _ = execute_ipa(root, &["--revert", "-n"], cancelled).await?;
        }
    }
    let hashes = version.get("contentHashes").and_then(Value::as_array).cloned().unwrap_or_default();
    for item in hashes {
        if cancelled.load(Ordering::Relaxed) { return Err(serialized_error("Mod removal was cancelled", "REQUEST_CANCELLED")); }
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

async fn execute_ipa(root: &Path, args: &[&str], cancelled: &AtomicBool) -> Result<bool, SerializedError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(serialized_error("Mod operation was cancelled", "REQUEST_CANCELLED"));
    }
    let ipa = root.join("IPA.exe");
    let game = root.join("Beat Saber.exe");
    if !ipa.is_file() || !game.is_file() { return Ok(false); }
    let child = Command::new(ipa).arg(game).args(args).current_dir(root).kill_on_drop(true).spawn()
        .map_err(|error| serialized_error(error.to_string(), "IPA_START_FAILED"))?;
    let output = tokio::select! {
        output = timeout(Duration::from_secs(30), child.wait_with_output()) => output,
        _ = wait_for_cancellation(cancelled) => {
            return Err(serialized_error("Mod operation was cancelled", "REQUEST_CANCELLED"));
        }
    };
    match output {
        Ok(Ok(output)) => Ok(output.status.success()),
        Ok(Err(error)) => Err(serialized_error(error.to_string(), "IPA_FAILED")),
        Err(_) => Ok(false),
    }
}

fn stage_ipa_contents(root: &Path) -> Result<Option<PathBuf>, SerializedError> {
    let ipa = root.join("IPA");
    match fs::symlink_metadata(&ipa) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(serialized_error("IPA path is not a regular folder", "INVALID_PATH"));
        }
        Ok(_) => {},
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(serialized_error(error.to_string(), "MOD_INSTALL_FAILED")),
    }
    let backup = root.join(format!(".bsmanager-ipa-backup-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&backup).map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
    let mut moved = Vec::new();
    let result = (|| {
        for entry in fs::read_dir(&ipa).map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))? {
            let entry = entry.map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
            let name = entry.file_name();
            if matches!(name.to_str(), Some("Backups" | "Pending")) { continue; }
            let kind = entry.file_type().map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
            if kind.is_symlink() { return Err(serialized_error("IPA folder contains a symbolic link", "INVALID_PATH")); }
            let staged = backup.join(&name);
            fs::rename(entry.path(), &staged).map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
            moved.push(name);
        }
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(restore_error) = restore_ipa_entries(&ipa, &backup, &moved) {
            return Err(serialized_error(
                format!("{}; IPA files remain backed up in {}: {}", error.message, backup.display(), restore_error.message),
                "MOD_INSTALL_FAILED",
            ));
        }
        let _ = fs::remove_dir(&backup);
        return Err(error);
    }
    if moved.is_empty() {
        let _ = fs::remove_dir(&backup);
        Ok(None)
    } else {
        Ok(Some(backup))
    }
}

fn restore_ipa_contents(root: &Path, backup: &Path) -> Result<(), SerializedError> {
    restore_ipa_entries(&root.join("IPA"), backup, &fs::read_dir(backup)
        .map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?
        .map(|entry| entry.map(|entry| entry.file_name()).map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED")))
        .collect::<Result<Vec<_>, _>>()?)?;
    fs::remove_dir(backup).map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))
}

fn restore_ipa_entries(ipa: &Path, backup: &Path, names: &[std::ffi::OsString]) -> Result<(), SerializedError> {
    fs::create_dir_all(ipa).map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
    for name in names.iter().rev() {
        let staged = backup.join(name);
        let target = ipa.join(name);
        match fs::symlink_metadata(&target) {
            Ok(_) => return Err(serialized_error(format!("Cannot restore IPA file over {}", target.display()), "MOD_INSTALL_FAILED")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => return Err(serialized_error(error.to_string(), "MOD_INSTALL_FAILED")),
        }
        fs::rename(staged, target).map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
    }
    Ok(())
}

struct ModFileSwap {
    target: PathBuf,
    backup: Option<PathBuf>,
}

fn move_staged_files(staging: &Path, destination: &Path, files: &[PathBuf], cancelled: &AtomicBool) -> Result<(), SerializedError> {
    let mut changes = Vec::new();
    let result = (|| {
        ensure_mod_directory(destination)?;
        for relative in files {
            if cancelled.load(Ordering::Relaxed) {
                return Err(serialized_error("Mod installation was cancelled", "REQUEST_CANCELLED"));
            }
            let relative = safe_relative_path(&relative.to_string_lossy())?;
            let source = staging.join(&relative);
            if !source.is_file() { continue; }
            ensure_mod_parent(destination, &relative)?;
            let target = destination.join(&relative);
            let staged = target.with_file_name(format!(".bsmanager-stage-{}", uuid::Uuid::new_v4()));
            let backup = target.with_file_name(format!(".bsmanager-backup-{}", uuid::Uuid::new_v4()));
            let copy_result = (|| {
                let mut input = File::open(&source).map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
                let mut output = fs::OpenOptions::new().write(true).create_new(true).open(&staged)
                    .map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
                crate::archive_io::copy_cancellable(&mut input, &mut output, cancelled).map_err(|error| {
                    if cancelled.load(Ordering::Relaxed) {
                        serialized_error("Mod installation was cancelled", "REQUEST_CANCELLED")
                    } else {
                        serialized_error(error.to_string(), "MOD_INSTALL_FAILED")
                    }
                })?;
                output.sync_all().map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))
            })();
            if let Err(error) = copy_result {
                let _ = fs::remove_file(&staged);
                return Err(error);
            }
            if cancelled.load(Ordering::Relaxed) {
                let _ = fs::remove_file(&staged);
                return Err(serialized_error("Mod installation was cancelled", "REQUEST_CANCELLED"));
            }
            let had_target = match fs::symlink_metadata(&target) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    let _ = fs::remove_file(&staged);
                    return Err(serialized_error("Mod destination contains a symbolic link", "INVALID_PATH"));
                }
                Ok(metadata) if metadata.is_dir() => {
                    let _ = fs::remove_file(&staged);
                    return Err(serialized_error("Mod destination is a folder", "INVALID_PATH"));
                }
                Ok(_) => true,
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(error) => {
                    let _ = fs::remove_file(&staged);
                    return Err(serialized_error(error.to_string(), "MOD_INSTALL_FAILED"));
                }
            };
            if had_target {
                if let Err(error) = fs::rename(&target, &backup) {
                    let _ = fs::remove_file(&staged);
                    return Err(serialized_error(error.to_string(), "MOD_INSTALL_FAILED"));
                }
            }
            if let Err(error) = fs::rename(&staged, &target) {
                if had_target {
                    if let Err(restore_error) = fs::rename(&backup, &target) {
                        let _ = fs::remove_file(&staged);
                        return Err(serialized_error(
                            format!("{}; previous mod file remains backed up at {}: {}", error, backup.display(), restore_error),
                            "MOD_INSTALL_FAILED",
                        ));
                    }
                }
                let _ = fs::remove_file(&staged);
                return Err(serialized_error(error.to_string(), "MOD_INSTALL_FAILED"));
            }
            changes.push(ModFileSwap { target, backup: had_target.then_some(backup) });
        }
        if cancelled.load(Ordering::Relaxed) {
            return Err(serialized_error("Mod installation was cancelled", "REQUEST_CANCELLED"));
        }
        Ok(())
    })();

    if let Err(error) = result {
        if let Err(rollback_error) = rollback_mod_files(&changes) {
            return Err(serialized_error(format!("{}; mod rollback was incomplete: {}", error.message, rollback_error), "MOD_INSTALL_FAILED"));
        }
        return Err(error);
    }
    for change in changes {
        if let Some(backup) = change.backup {
            if let Err(error) = remove_mod_path(&backup) {
                log::warn!("Could not remove replaced mod backup {}: {error}", backup.display());
            }
        }
    }
    let _ = fs::remove_dir_all(staging);
    Ok(())
}

fn ensure_mod_directory(path: &Path) -> Result<(), SerializedError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(serialized_error("Mod destination is not a regular folder", "INVALID_PATH"))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))
        }
        Err(error) => Err(serialized_error(error.to_string(), "MOD_INSTALL_FAILED")),
    }
}

fn ensure_mod_parent(destination: &Path, relative: &Path) -> Result<(), SerializedError> {
    ensure_mod_directory(destination)?;
    let mut parent = destination.to_path_buf();
    if let Some(relative_parent) = relative.parent() {
        for component in relative_parent.components() {
            let Component::Normal(name) = component else { continue; };
            parent.push(name);
            match fs::symlink_metadata(&parent) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    return Err(serialized_error("Mod destination contains a symbolic link or file", "INVALID_PATH"));
                }
                Ok(_) => {},
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(&parent).map_err(|error| serialized_error(error.to_string(), "MOD_INSTALL_FAILED"))?;
                }
                Err(error) => return Err(serialized_error(error.to_string(), "MOD_INSTALL_FAILED")),
            }
        }
    }
    Ok(())
}

fn rollback_mod_files(changes: &[ModFileSwap]) -> Result<(), String> {
    let mut failures = Vec::new();
    for change in changes.iter().rev() {
        if let Err(error) = remove_mod_path(&change.target) {
            if let Some(backup) = &change.backup {
                failures.push(format!("could not remove {}; previous file remains at {}: {error}", change.target.display(), backup.display()));
            } else {
                failures.push(format!("could not remove {}: {error}", change.target.display()));
            }
            continue;
        }
        if let Some(backup) = &change.backup {
            if let Err(error) = fs::rename(backup, &change.target) {
                failures.push(format!("could not restore {} from {}: {error}", change.target.display(), backup.display()));
            }
        }
    }
    if failures.is_empty() { Ok(()) } else { Err(failures.join("; ")) }
}

fn remove_mod_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Refusing to remove a non-file mod path",
        )),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
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

async fn lookup_hash(hash: &str, cancelled: &AtomicBool) -> Option<Value> {
    if let Some(cached) = MODS_BY_HASH.get_or_init(Default::default).lock().ok().and_then(|cache| cache.get(hash).cloned()) {
        return Some(cached);
    }
    let url = reqwest::Url::parse_with_params(&format!("{BEATMODS}/api/hashlookup"), &[("hash", hash)]).ok()?;
    let response = get_json(url, cancelled).await.ok()?;
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

async fn get_json(url: reqwest::Url, cancelled: &AtomicBool) -> Result<Value, SerializedError> {
    let client = crate::proxy::client_builder().connect_timeout(Duration::from_secs(20)).timeout(Duration::from_secs(30))
        .user_agent(format!("BSManager/{}", env!("CARGO_PKG_VERSION"))).build()
        .map_err(|error| serialized_error(error.to_string(), "BEATMODS_REQUEST_FAILED"))?;
    let mut request = client.get(url);
    for attempt in 0..4 {
        if cancelled.load(Ordering::Relaxed) { return Err(serialized_error("BeatMods request was cancelled", "REQUEST_CANCELLED")); }
        let retry = if attempt < 3 { request.try_clone() } else { None };
        let response = tokio::select! {
            response = request.send() => response,
            _ = wait_for_cancellation(cancelled) => {
                return Err(serialized_error("BeatMods request was cancelled", "REQUEST_CANCELLED"));
            }
        };
        match response {
            Ok(response) if response.status().is_success() => {
                let body = tokio::select! {
                    body = response.text() => body.map_err(|error| serialized_error(error.to_string(), "BEATMODS_RESPONSE_FAILED"))?,
                    _ = wait_for_cancellation(cancelled) => {
                        return Err(serialized_error("BeatMods request was cancelled", "REQUEST_CANCELLED"));
                    }
                };
                return serde_json::from_str(&body).map_err(|error| serialized_error(error.to_string(), "BEATMODS_RESPONSE_FAILED"));
            }
            Ok(response) if attempt < 3 && (response.status().as_u16() == 429 || response.status().is_server_error()) && retry.is_some() => {
                request = retry.expect("checked retry request");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))) => {},
                    _ = wait_for_cancellation(cancelled) => {
                        return Err(serialized_error("BeatMods request was cancelled", "REQUEST_CANCELLED"));
                    }
                }
            }
            Ok(response) => return Err(serialized_error(format!("BeatMods returned HTTP {}", response.status()), "BEATMODS_REQUEST_FAILED")),
            Err(error) if attempt < 3 && (error.is_timeout() || error.is_connect()) && retry.is_some() => {
                request = retry.expect("checked retry request");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))) => {},
                    _ = wait_for_cancellation(cancelled) => {
                        return Err(serialized_error("BeatMods request was cancelled", "REQUEST_CANCELLED"));
                    }
                }
            }
            Err(error) => return Err(serialized_error(error.to_string(), "BEATMODS_REQUEST_FAILED")),
        }
    }
    Err(serialized_error("BeatMods request failed", "BEATMODS_REQUEST_FAILED"))
}

async fn is_up(cancelled: &AtomicBool) -> bool {
    let Ok(url) = reqwest::Url::parse(&format!("{BEATMODS}/api/status")) else { return false; };
    get_json(url, cancelled).await.is_ok()
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
