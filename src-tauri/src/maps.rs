use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    sync::{Arc, OnceLock, atomic::{AtomicBool, Ordering}},
    time::Duration,
};

use chrono::Local;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use sha1::{Digest, Sha1};
use tauri::{AppHandle, Runtime, WebviewWindow};
use tokio::io::AsyncWriteExt;
use url::Url;
use zip::{CompressionMethod, ZipArchive, write::{SimpleFileOptions, ZipWriter}};

use crate::{SerializedError, linker, serialized_error, versions};

const LEVELS_FOLDER: &str = "Beat Saber_Data";
const CUSTOM_LEVELS_FOLDER: &str = "CustomLevels";
const SHARED_MAPS_FOLDER: &str = "SharedMaps";
const METADATA_FILE: &str = "metadata.json";
static LAST_DOWNLOADED: OnceLock<tokio::sync::broadcast::Sender<Value>> = OnceLock::new();

pub async fn load_version_maps<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    version: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let _ = tokio::time::timeout(Duration::from_secs(30), crate::song_cache::initialize(app)).await;
    let root = map_folder(app, version)?;
    if version.is_null() {
        tokio::fs::create_dir_all(&root).await
            .map_err(|error| serialized_error(error.to_string(), "MAP_FOLDER_FAILED"))?;
    }
    let mut entries = match tokio::fs::read_dir(&root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Value::Null),
        Err(error) => return Err(serialized_error(error.to_string(), "MAP_FOLDER_FAILED")),
    };
    let mut folders = Vec::new();
    while let Some(entry) = entries.next_entry().await
        .map_err(|error| serialized_error(error.to_string(), "MAP_FOLDER_FAILED"))? {
        if entry.metadata().await.map(|metadata| metadata.is_dir()).unwrap_or(false) {
            folders.push(entry.path());
        }
    }

    let total = folders.len() as u64;
    let mut maps = Vec::new();
    emit_load_progress(window, response_channel, total, 0, &[])?;
    for folder in folders {
        if cancelled.load(Ordering::Relaxed) {
            return Err(serialized_error("Map listing was cancelled", "REQUEST_CANCELLED"));
        }
        let map = tokio::task::spawn_blocking(move || load_map_info(&folder)).await
            .map_err(|error| serialized_error(error.to_string(), "MAP_LOAD_FAILED"))?;
        match map {
            Ok(Some(map)) => {
                maps.push(map);
                emit_load_progress(window, response_channel, total, maps.len() as u64, &[])?;
            }
            Ok(None) => {}
            Err(error) => log::warn!("Could not load local Beat Saber map: {error}"),
        }
    }
    emit_load_progress(window, response_channel, total, maps.len() as u64, &maps)?;
    Ok(Value::Null)
}

pub async fn delete_maps<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    args: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let maps = args.as_array()
        .ok_or_else(|| serialized_error("Maps must be an array", "INVALID_MAPS"))?;
    let roots = managed_map_roots(app)?;
    let total = maps.len() as u64;
    let mut deleted = 0_u64;
    emit_delete_progress(window, response_channel, total, deleted)?;
    for map in maps {
        if cancelled.load(Ordering::Relaxed) {
            return Err(serialized_error("Map deletion was cancelled", "REQUEST_CANCELLED"));
        }
        let Some(path) = map.get("path").and_then(Value::as_str) else {
            emit_delete_progress(window, response_channel, total, deleted)?;
            continue;
        };
        let path = PathBuf::from(path);
        if let Ok(canonical) = fs::canonicalize(&path) {
            let allowed = roots.iter().any(|root| canonical.parent() == Some(root.as_path()));
            if !allowed || !canonical.is_dir() {
                return Err(serialized_error("Map folder is outside managed content", "INVALID_PATH"));
            }
            fs::remove_dir_all(&canonical)
                .map_err(|error| serialized_error(error.to_string(), "MAP_DELETE_FAILED"))?;
            if let Some(dirname) = canonical.file_name().and_then(|name| name.to_str()) {
                crate::map_info_cache::remove(dirname);
            }
            deleted += 1;
        }
        emit_delete_progress(window, response_channel, total, deleted)?;
    }
    Ok(Value::Null)
}

pub fn validate_managed_map_path<R: Runtime>(app: &AppHandle<R>, path: &Path) -> Result<PathBuf, SerializedError> {
    let path = fs::canonicalize(path).map_err(|error| serialized_error(error.to_string(), "INVALID_PATH"))?;
    let roots = managed_map_roots(app)?;
    if path.is_dir() && roots.iter().any(|root| path.parent() == Some(root.as_path())) {
        Ok(path)
    } else {
        Err(serialized_error("Map folder is outside managed content", "INVALID_PATH"))
    }
}

pub async fn copy_managed_map_to_version<R: Runtime>(
    app: &AppHandle<R>,
    source: &Path,
    version: &Value,
) -> Result<PathBuf, SerializedError> {
    let source = validate_managed_map_path(app, source)?;
    let root = map_folder(app, version)?;
    tokio::fs::create_dir_all(&root).await
        .map_err(|error| serialized_error(error.to_string(), "MAP_FOLDER_FAILED"))?;
    let root = tokio::fs::canonicalize(&root).await
        .map_err(|error| serialized_error(error.to_string(), "MAP_FOLDER_FAILED"))?;
    if source.parent() == Some(root.as_path()) { return Ok(source); }
    let destination = root.join(source.file_name().unwrap_or_default());
    if fs::symlink_metadata(&destination).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(serialized_error("Map destination cannot be a symbolic link", "INVALID_PATH"));
    }
    let source_for_copy = source.clone();
    let destination_for_copy = destination.clone();
    tokio::task::spawn_blocking(move || copy_map_directory(&source_for_copy, &destination_for_copy)).await
        .map_err(|error| serialized_error(error.to_string(), "MAP_COPY_FAILED"))??;
    Ok(destination)
}

pub async fn delete_maps_by_hashes<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    version: &Value,
    hashes: &[String],
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let root = map_folder(app, version)?;
    let root = match fs::canonicalize(&root) {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            emit_hash_delete_progress(window, response_channel, 0, hashes.len() as u64)?;
            return Ok(Value::Null);
        }
        Err(error) => return Err(serialized_error(error.to_string(), "MAP_FOLDER_FAILED")),
    };
    let folders = fs::read_dir(&root).map_err(|error| serialized_error(error.to_string(), "MAP_FOLDER_FAILED"))?
        .filter_map(Result::ok).map(|entry| entry.path()).filter(|path| path.is_dir()).collect::<Vec<_>>();
    let total = hashes.len() as u64;
    let mut current = 0_u64;
    emit_hash_delete_progress(window, response_channel, current, total)?;
    for folder in folders {
        if cancelled.load(Ordering::Relaxed) {
            return Err(serialized_error("Playlist map deletion was cancelled", "REQUEST_CANCELLED"));
        }
        let folder_for_load = folder.clone();
        let map = tokio::task::spawn_blocking(move || load_map_info(&folder_for_load)).await
            .map_err(|error| serialized_error(error.to_string(), "MAP_LOAD_FAILED"))?;
        if let Ok(Some(map)) = map {
            if map.get("hash").and_then(Value::as_str).is_some_and(|hash| hashes.iter().any(|wanted| wanted.eq_ignore_ascii_case(hash))) {
                let canonical = validate_managed_map_path(app, &folder)?;
                tokio::fs::remove_dir_all(canonical).await
                    .map_err(|error| serialized_error(error.to_string(), "MAP_DELETE_FAILED"))?;
                if let Some(dirname) = folder.file_name().and_then(|name| name.to_str()) {
                    crate::map_info_cache::remove(dirname);
                }
                current = current.saturating_add(1);
            }
        }
        emit_hash_delete_progress(window, response_channel, current, total)?;
    }
    Ok(Value::Null)
}

pub async fn export_maps<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    args: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let version = args.get("version").filter(|value| !value.is_null()).unwrap_or(&Value::Null);
    let root = map_folder(app, version)?;
    let root = fs::canonicalize(&root).map_err(|error| serialized_error(error.to_string(), "MAP_FOLDER_FAILED"))?;
    let output = args.get("outPath").and_then(Value::as_str).filter(|path| !path.trim().is_empty())
        .ok_or_else(|| serialized_error("Archive output path is required", "INVALID_PATH"))?;
    let output = PathBuf::from(output);
    if !output.is_absolute() {
        return Err(serialized_error("Archive output path must be absolute", "INVALID_PATH"));
    }
    let output_parent = output.parent().ok_or_else(|| serialized_error("Archive output folder is invalid", "INVALID_PATH"))?;
    let output_parent = fs::canonicalize(output_parent)
        .map_err(|error| serialized_error(error.to_string(), "INVALID_PATH"))?;
    let output_file_name = output.file_name().ok_or_else(|| serialized_error("Archive filename is invalid", "INVALID_PATH"))?;
    let output = output_parent.join(output_file_name);
    if output.starts_with(&root) {
        return Err(serialized_error("Archive output cannot be inside the map library", "INVALID_PATH"));
    }

    let selected = args.get("maps").and_then(Value::as_array).filter(|maps| !maps.is_empty());
    let mut files = Vec::new();
    if let Some(selected) = selected {
        for map in selected {
            let path = map.get("path").and_then(Value::as_str)
                .ok_or_else(|| serialized_error("Map path is required", "INVALID_PATH"))?;
            let path = fs::canonicalize(path).map_err(|error| serialized_error(error.to_string(), "INVALID_PATH"))?;
            if path.parent() != Some(root.as_path()) || !path.is_dir() {
                return Err(serialized_error("Map folder is outside the selected version", "INVALID_PATH"));
            }
            collect_map_archive_files(&path, &path, Path::new(path.file_name().unwrap_or_default()), &mut files)
                .map_err(|error| serialized_error(error.to_string(), "MAP_EXPORT_FAILED"))?;
        }
    } else {
        collect_map_archive_files(&root, &root, Path::new(""), &mut files)
            .map_err(|error| serialized_error(error.to_string(), "MAP_EXPORT_FAILED"))?;
    }

    let total = files.len() as u64;
    let output_text = output.to_string_lossy().to_string();
    emit_archive_progress(window, response_channel, 0, total, &output_text)?;
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<u64>();
    let worker_cancelled = cancelled.clone();
    let worker_output = output.clone();
    let worker = tokio::task::spawn_blocking(move || write_map_archive(&worker_output, files, worker_cancelled, sender));
    let mut progress_error = None;
    while let Some(current) = receiver.recv().await {
        if let Err(error) = emit_archive_progress(window, response_channel, current, total, &output_text) {
            cancelled.store(true, Ordering::Relaxed);
            progress_error = Some(error);
        }
    }
    let result = worker.await.map_err(|error| serialized_error(error.to_string(), "MAP_EXPORT_FAILED"))?;
    if let Some(error) = progress_error { return Err(error); }
    if let Err(error) = result {
        if cancelled.load(Ordering::Relaxed) {
            return Err(serialized_error("Map export was cancelled", "REQUEST_CANCELLED"));
        }
        return Err(serialized_error(error, "MAP_EXPORT_FAILED"));
    }
    Ok(Value::Null)
}

pub async fn import_maps<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    args: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let paths = args.get("paths").and_then(Value::as_array)
        .ok_or_else(|| serialized_error("Map archive paths must be an array", "INVALID_PATHS"))?;
    let version = args.get("version").filter(|value| !value.is_null()).unwrap_or(&Value::Null);
    let root = map_folder(app, version)?;
    tokio::fs::create_dir_all(&root).await
        .map_err(|error| serialized_error(error.to_string(), "MAP_FOLDER_FAILED"))?;
    let mut imported = 0_u64;
    for archive_path in paths.iter().filter_map(Value::as_str) {
        if cancelled.load(Ordering::Relaxed) {
            return Err(serialized_error("Map import was cancelled", "REQUEST_CANCELLED"));
        }
        let archive_path = PathBuf::from(archive_path);
        emit_import_progress(window, response_channel, 0, 0, None)?;
        if !archive_path.is_file() { continue; }
        let scan_path = archive_path.clone();
        let folders = tokio::task::spawn_blocking(move || scan_map_archive(&scan_path)).await
            .map_err(|error| serialized_error(error.to_string(), "MAP_IMPORT_FAILED"))?
            .map_err(|error| serialized_error(error.to_string(), "INVALID_MAP_ARCHIVE"))?;
        if folders.is_empty() {
            emit_import_progress(window, response_channel, 1, 1, None)?;
            continue;
        }
        let total = folders.len() as u64;
        emit_import_progress(window, response_channel, 0, total, None)?;
        let root_folder = folders.len() == 1 && folders[0] == ".";
        let mut current = 0_u64;
        for folder in folders {
            if cancelled.load(Ordering::Relaxed) {
                return Err(serialized_error("Map import was cancelled", "REQUEST_CANCELLED"));
            }
            let staging = root.join(format!(".bsmanager-import-{}", uuid::Uuid::new_v4()));
            let archive_for_worker = archive_path.clone();
            let staging_for_worker = staging.clone();
            let cancelled_for_worker = cancelled.clone();
            let folder_for_worker = folder.clone();
            let extracted = tokio::task::spawn_blocking(move || {
                extract_map_archive_folder(&archive_for_worker, &staging_for_worker, &folder_for_worker, &cancelled_for_worker)
            }).await.map_err(|error| serialized_error(error.to_string(), "MAP_IMPORT_FAILED"))?;
            if let Err(error) = extracted {
                let _ = tokio::fs::remove_dir_all(&staging).await;
                if cancelled.load(Ordering::Relaxed) {
                    return Err(serialized_error("Map import was cancelled", "REQUEST_CANCELLED"));
                }
                return Err(serialized_error(error, "MAP_IMPORT_FAILED"));
            }
            if cancelled.load(Ordering::Relaxed) {
                let _ = tokio::fs::remove_dir_all(&staging).await;
                return Err(serialized_error("Map import was cancelled", "REQUEST_CANCELLED"));
            }
            let staging_for_load = staging.clone();
            let parsed = tokio::task::spawn_blocking(move || load_map_info(&staging_for_load)).await
                .map_err(|error| serialized_error(error.to_string(), "MAP_LOAD_FAILED"))?
                .map_err(|error| serialized_error(error, "MAP_LOAD_FAILED"))?;
            let Some(_) = parsed else {
                let _ = tokio::fs::remove_dir_all(&staging).await;
                continue;
            };
            let destination = if root_folder {
                let name = archive_path.file_stem().unwrap_or_default().to_string_lossy();
                root.join(sanitize_folder_name(&name))
            } else {
                let relative = safe_archive_relative(&folder)
                    .map_err(|error| serialized_error(error, "INVALID_MAP_ARCHIVE"))?;
                root.join(relative)
            };
            if let Some(parent) = destination.parent() {
                tokio::fs::create_dir_all(parent).await
                    .map_err(|error| serialized_error(error.to_string(), "MAP_FOLDER_FAILED"))?;
            }
            if let Err(error) = install_staged_map(&staging, &destination).await {
                let _ = tokio::fs::remove_dir_all(&staging).await;
                return Err(error);
            }
            let installed_path = destination.clone();
            let local_map = tokio::task::spawn_blocking(move || load_map_info(&installed_path)).await
                .map_err(|error| serialized_error(error.to_string(), "MAP_LOAD_FAILED"))?
                .map_err(|error| serialized_error(error, "MAP_LOAD_FAILED"))?
                .ok_or_else(|| serialized_error("Imported map has no Info.dat", "MAP_LOAD_FAILED"))?;
            imported += 1;
            current += 1;
            emit_import_progress(window, response_channel, current, total, Some(local_map))?;
        }
    }
    if imported == 0 {
        return Err(serialized_error("No Info.dat file found in selected archives", "INVALID_MAP_ARCHIVE"));
    }
    Ok(Value::Null)
}

pub async fn download_map<R: Runtime>(
    app: &AppHandle<R>,
    args: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let map = args.get("map").ok_or_else(|| serialized_error("Map details are required", "INVALID_MAP"))?;
    let version = args.get("version").filter(|version| !version.is_null());
    download_and_install_map(app, map, version, cancelled).await
}

pub async fn one_click_install_map<R: Runtime>(
    app: &AppHandle<R>,
    map: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let mut installed = versions::get_installed_versions(app, false)?
        .as_array().cloned().unwrap_or_default();
    let target_version = installed.pop();
    let downloaded = download_and_install_map(app, map, target_version.as_ref(), cancelled.clone()).await?;
    let source = downloaded.get("path").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Downloaded map path is missing", "MAP_DOWNLOAD_FAILED"))?;
    let source = PathBuf::from(source);
    let source_parent = source.parent().and_then(|path| fs::canonicalize(path).ok())
        .ok_or_else(|| serialized_error("Downloaded map folder is unavailable", "MAP_DOWNLOAD_FAILED"))?;
    let app = app.clone();
    tokio::task::spawn_blocking(move || -> Result<(), SerializedError> {
        for version in installed {
            if cancelled.load(Ordering::Relaxed) {
                return Err(serialized_error("Map installation was cancelled", "REQUEST_CANCELLED"));
            }
            let root = map_folder(&app, &version)?;
            fs::create_dir_all(&root).map_err(|error| serialized_error(error.to_string(), "MAP_FOLDER_FAILED"))?;
            let real_root = fs::canonicalize(&root).map_err(|error| serialized_error(error.to_string(), "MAP_FOLDER_FAILED"))?;
            if real_root == source_parent { continue; }
            let destination = real_root.join(source.file_name().unwrap_or_default());
            copy_map_directory_cancellable(&source, &destination, &cancelled)?;
        }
        Ok(())
    }).await.map_err(|error| serialized_error(error.to_string(), "MAP_COPY_FAILED"))??;
    Ok(Value::Null)
}

pub async fn stream_last_downloaded<R: Runtime>(
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let Some(channel) = response_channel else { return Ok(Value::Null); };
    let mut receiver = LAST_DOWNLOADED.get_or_init(|| tokio::sync::broadcast::channel(32).0).subscribe();
    while !cancelled.load(Ordering::Relaxed) {
        match tokio::time::timeout(Duration::from_millis(250), receiver.recv()).await {
            Ok(Ok(value)) => crate::emit_response_value(window, Some(channel), value)?,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) | Err(_) => {},
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
        }
    }
    Ok(Value::Null)
}

async fn download_and_install_map<R: Runtime>(
    app: &AppHandle<R>,
    map: &Value,
    version: Option<&Value>,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let first_version = map.pointer("/versions/0")
        .ok_or_else(|| serialized_error("Map has no downloadable version", "INVALID_MAP"))?;
    if first_version.get("hash").and_then(Value::as_str).filter(|hash| !hash.is_empty()).is_none() {
        return Err(serialized_error("Map version hash is missing", "INVALID_MAP"));
    }
    let expected_hash = first_version.get("hash").and_then(Value::as_str).unwrap_or_default().to_lowercase();
    let download_url = first_version.get("downloadURL").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Map download URL is missing", "INVALID_MAP"))?;
    let download_url = Url::parse(download_url)
        .map_err(|error| serialized_error(error.to_string(), "INVALID_MAP_URL"))?;
    if download_url.scheme() != "https" {
        return Err(serialized_error("Map downloads require HTTPS", "INVALID_MAP_URL"));
    }
    let id = map.get("id").and_then(Value::as_str).unwrap_or("map");
    let song = map.pointer("/metadata/songName").and_then(Value::as_str).unwrap_or("Unknown song");
    let author = map.pointer("/metadata/levelAuthorName").and_then(Value::as_str).unwrap_or("Unknown mapper");
    let folder_name = sanitize_folder_name(&format!("{id} ({song} - {author})"));
    let root = map_folder(app, version.unwrap_or(&Value::Null))?;
    tokio::fs::create_dir_all(&root).await
        .map_err(|error| serialized_error(error.to_string(), "MAP_FOLDER_FAILED"))?;
    let root = tokio::fs::canonicalize(&root).await
        .map_err(|error| serialized_error(error.to_string(), "MAP_FOLDER_FAILED"))?;
    let destination = root.join(&folder_name);
    if fs::symlink_metadata(&destination).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(serialized_error("Map destination cannot be a symbolic link", "INVALID_PATH"));
    }
    if destination.exists() {
        if let Ok(Some(existing)) = load_map_info(&destination) {
            let existing_hash = existing.get("hash").and_then(Value::as_str);
            let same_versions = map.get("versions").and_then(Value::as_array).is_some_and(|versions| {
                !versions.is_empty() && versions.iter().all(|version| version.get("hash").and_then(Value::as_str) == existing_hash)
            });
            if same_versions { return Ok(existing); }
        }
    }

    let id = uuid::Uuid::new_v4().to_string();
    let staging = root.join(format!(".bsmanager-map-{id}"));
    let archive_path = root.join(format!(".bsmanager-map-{id}.zip"));
    let backup = root.join(format!(".bsmanager-map-backup-{id}"));
    let result = async {
        download_map_archive(&download_url, &archive_path, &cancelled).await?;
        let archive_for_worker = archive_path.clone();
        let staging_for_worker = staging.clone();
        let cancelled_for_worker = cancelled.clone();
        let extracted = tokio::task::spawn_blocking(move || {
            extract_map_archive(&archive_for_worker, &staging_for_worker, &cancelled_for_worker)
        }).await.map_err(|error| serialized_error(error.to_string(), "MAP_EXTRACT_FAILED"))?;
        if let Err(error) = extracted {
            if cancelled.load(Ordering::Relaxed) {
                return Err(serialized_error("Map download was cancelled", "REQUEST_CANCELLED"));
            }
            return Err(serialized_error(error, "MAP_EXTRACT_FAILED"));
        }
        let staging_for_load = staging.clone();
        let extracted = tokio::task::spawn_blocking(move || load_map_info(&staging_for_load)).await
            .map_err(|error| serialized_error(error.to_string(), "MAP_LOAD_FAILED"))?
            .map_err(|error| serialized_error(error, "MAP_LOAD_FAILED"))?
            .ok_or_else(|| serialized_error("Downloaded archive has no Info.dat", "INVALID_MAP_ARCHIVE"))?;
        let actual_hash = extracted.get("hash").and_then(Value::as_str).unwrap_or_default();
        if !actual_hash.eq_ignore_ascii_case(&expected_hash) {
            return Err(serialized_error("Downloaded map hash does not match BeatSaver", "MAP_HASH_MISMATCH"));
        }
        if cancelled.load(Ordering::Relaxed) {
            return Err(serialized_error("Map download was cancelled", "REQUEST_CANCELLED"));
        }
        let had_destination = destination.exists();
        if had_destination {
            tokio::fs::rename(&destination, &backup).await
                .map_err(|error| serialized_error(error.to_string(), "MAP_REPLACE_FAILED"))?;
        }
        if let Err(error) = tokio::fs::rename(&staging, &destination).await {
            if had_destination { let _ = tokio::fs::rename(&backup, &destination).await; }
            return Err(serialized_error(error.to_string(), "MAP_REPLACE_FAILED"));
        }
        if had_destination {
            let _ = tokio::fs::remove_dir_all(&backup).await;
        }
        let final_path = destination.clone();
        let local_map = tokio::task::spawn_blocking(move || load_map_info(&final_path)).await
            .map_err(|error| serialized_error(error.to_string(), "MAP_LOAD_FAILED"))?
            .map_err(|error| serialized_error(error, "MAP_LOAD_FAILED"))?
            .ok_or_else(|| serialized_error("Installed map has no Info.dat", "MAP_LOAD_FAILED"))?;
        let event = json!({ "map": local_map, "version": version.cloned().unwrap_or(Value::Null) });
        let _ = LAST_DOWNLOADED.get_or_init(|| tokio::sync::broadcast::channel(32).0).send(event);
        Ok(local_map)
    }.await;
    let _ = tokio::fs::remove_file(&archive_path).await;
    if staging.exists() { let _ = tokio::fs::remove_dir_all(&staging).await; }
    result
}

async fn download_map_archive(url: &Url, destination: &Path, cancelled: &AtomicBool) -> Result<(), SerializedError> {
    let response = crate::proxy::client_builder().connect_timeout(Duration::from_secs(20)).timeout(Duration::from_secs(1800))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 || attempt.url().scheme() != "https" { attempt.stop() } else { attempt.follow() }
        }))
        .user_agent(format!("BSManager/{}", env!("CARGO_PKG_VERSION"))).build()
        .map_err(|error| serialized_error(error.to_string(), "MAP_DOWNLOAD_FAILED"))?
        .get(url.clone()).send().await.and_then(reqwest::Response::error_for_status)
        .map_err(|error| serialized_error(error.to_string(), "MAP_DOWNLOAD_FAILED"))?;
    let expected = response.content_length();
    let mut received = 0_u64;
    let mut stream = response.bytes_stream();
    let mut file = tokio::fs::File::create(destination).await
        .map_err(|error| serialized_error(error.to_string(), "MAP_DOWNLOAD_FAILED"))?;
    while let Some(chunk) = stream.next().await {
        if cancelled.load(Ordering::Relaxed) {
            drop(file);
            let _ = tokio::fs::remove_file(destination).await;
            return Err(serialized_error("Map download was cancelled", "REQUEST_CANCELLED"));
        }
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                drop(file);
                let _ = tokio::fs::remove_file(destination).await;
                return Err(serialized_error(error.to_string(), "MAP_DOWNLOAD_FAILED"));
            }
        };
        file.write_all(&chunk).await
            .map_err(|error| serialized_error(error.to_string(), "MAP_DOWNLOAD_FAILED"))?;
        received = received.saturating_add(chunk.len() as u64);
    }
    file.flush().await.map_err(|error| serialized_error(error.to_string(), "MAP_DOWNLOAD_FAILED"))?;
    drop(file);
    if expected.is_some_and(|expected| expected != received) {
        let _ = tokio::fs::remove_file(destination).await;
        return Err(serialized_error("Map archive size did not match the server response", "MAP_DOWNLOAD_TRUNCATED"));
    }
    Ok(())
}

fn extract_map_archive(archive_path: &Path, destination: &Path, cancelled: &AtomicBool) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    let file = File::open(archive_path).map_err(|error| error.to_string())?;
    let mut archive = ZipArchive::new(file).map_err(|error| error.to_string())?;
    for index in 0..archive.len() {
        if cancelled.load(Ordering::Relaxed) { return Err("Map extraction was cancelled".to_owned()); }
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        if entry.is_symlink() {
            return Err("Map archive contains a symbolic link".to_owned());
        }
        let relative = entry.enclosed_name().ok_or_else(|| "Map archive contains an unsafe path".to_owned())?.to_path_buf();
        let output = destination.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&output).map_err(|error| error.to_string())?;
        } else {
            if let Some(parent) = output.parent() { fs::create_dir_all(parent).map_err(|error| error.to_string())?; }
            let mut file = File::create(output).map_err(|error| error.to_string())?;
            copy_archive_entry(&mut entry, &mut file, cancelled)?;
        }
    }
    Ok(())
}

fn scan_map_archive(archive_path: &Path) -> io::Result<Vec<String>> {
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file).map_err(io::Error::other)?;
    let mut folders = Vec::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(io::Error::other)?;
        let name = entry.name().map_err(io::Error::other)?.replace('\\', "/");
        let Some(file_name) = name.rsplit('/').next() else { continue; };
        if !file_name.eq_ignore_ascii_case("info.dat") || entry.enclosed_name().is_none() { continue; }
        let folder = name.rsplit_once('/').map(|(folder, _)| folder).unwrap_or(".");
        if !folders.iter().any(|existing| existing == folder) {
            folders.push(folder.to_owned());
        }
    }
    Ok(folders)
}

fn extract_map_archive_folder(archive_path: &Path, destination: &Path, folder: &str, cancelled: &AtomicBool) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    let file = File::open(archive_path).map_err(|error| error.to_string())?;
    let mut archive = ZipArchive::new(file).map_err(|error| error.to_string())?;
    let prefix = if folder == "." { String::new() } else { format!("{}/", folder.trim_end_matches('/')) };
    for index in 0..archive.len() {
        if cancelled.load(Ordering::Relaxed) { return Err("Map extraction was cancelled".to_owned()); }
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        let Some(enclosed) = entry.enclosed_name() else { return Err("Map archive contains an unsafe path".to_owned()); };
        if entry.is_symlink() { return Err("Map archive contains a symbolic link".to_owned()); }
        let entry_name = entry.name().map_err(|error| error.to_string())?.replace('\\', "/");
        let relative = if folder == "." {
            enclosed.to_path_buf()
        } else if entry_name.starts_with(&prefix) {
            safe_archive_relative(&entry_name[prefix.len()..])?
        } else {
            continue;
        };
        if relative.as_os_str().is_empty() { continue; }
        let output = destination.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&output).map_err(|error| error.to_string())?;
        } else {
            if let Some(parent) = output.parent() { fs::create_dir_all(parent).map_err(|error| error.to_string())?; }
            let mut file = File::create(output).map_err(|error| error.to_string())?;
            copy_archive_entry(&mut entry, &mut file, cancelled)?;
        }
    }
    Ok(())
}

fn copy_archive_entry<R: Read, W: io::Write>(reader: &mut R, writer: &mut W, cancelled: &AtomicBool) -> Result<(), String> {
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if cancelled.load(Ordering::Relaxed) { return Err("Map extraction was cancelled".to_owned()); }
        let read = reader.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 { return Ok(()); }
        writer.write_all(&buffer[..read]).map_err(|error| error.to_string())?;
    }
}

fn safe_archive_relative(path: &str) -> Result<PathBuf, String> {
    let path = Path::new(path);
    if path.as_os_str().is_empty() || path.components().any(|component| !matches!(component, Component::Normal(_))) {
        return Err("Map archive contains an invalid map folder".to_owned());
    }
    Ok(path.to_path_buf())
}

async fn install_staged_map(staging: &Path, destination: &Path) -> Result<(), SerializedError> {
    if fs::symlink_metadata(destination).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(serialized_error("Map destination cannot be a symbolic link", "INVALID_PATH"));
    }
    let backup = destination.with_file_name(format!(".bsmanager-backup-{}", uuid::Uuid::new_v4()));
    let had_destination = destination.exists();
    if had_destination {
        tokio::fs::rename(destination, &backup).await
            .map_err(|error| serialized_error(error.to_string(), "MAP_REPLACE_FAILED"))?;
    }
    if let Err(error) = tokio::fs::rename(staging, destination).await {
        if had_destination { let _ = tokio::fs::rename(&backup, destination).await; }
        return Err(serialized_error(error.to_string(), "MAP_REPLACE_FAILED"));
    }
    if had_destination { let _ = tokio::fs::remove_dir_all(backup).await; }
    Ok(())
}

fn emit_import_progress<R: Runtime>(
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    current: u64,
    total: u64,
    data: Option<Value>,
) -> Result<(), SerializedError> {
    if let Some(channel) = response_channel {
        let mut value = json!({ "current": current, "total": total });
        if let (Some(object), Some(data)) = (value.as_object_mut(), data) {
            object.insert("data".into(), data);
        }
        crate::emit_response_value(window, Some(channel), value)?;
    }
    Ok(())
}

fn collect_map_archive_files(
    root: &Path,
    directory: &Path,
    archive_prefix: &Path,
    files: &mut Vec<(PathBuf, String)>,
) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_symlink() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Map library contains a symbolic link"));
        }
        if kind.is_dir() {
            collect_map_archive_files(root, &path, archive_prefix, files)?;
        } else if kind.is_file() {
            let relative = path.strip_prefix(root).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let name = archive_prefix.join(relative).to_string_lossy().replace('\\', "/");
            files.push((path, name));
        }
    }
    Ok(())
}

fn write_map_archive(
    output: &Path,
    files: Vec<(PathBuf, String)>,
    cancelled: Arc<AtomicBool>,
    progress: tokio::sync::mpsc::UnboundedSender<u64>,
) -> Result<(), String> {
    let result = (|| {
        let file = File::create(output).map_err(|error| error.to_string())?;
        let mut archive = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated).compression_level(Some(9));
        for (index, (path, name)) in files.into_iter().enumerate() {
            if cancelled.load(Ordering::Relaxed) {
                return Err("Map export was cancelled".to_owned());
            }
            archive.start_file(name, options).map_err(|error| error.to_string())?;
            let mut input = File::open(path).map_err(|error| error.to_string())?;
            crate::archive_io::copy_cancellable(&mut input, &mut archive, &cancelled)
                .map_err(|error| error.to_string())?;
            let _ = progress.send(index as u64 + 1);
        }
        archive.finish().map_err(|error| error.to_string())?;
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(output); }
    result
}

fn emit_archive_progress<R: Runtime>(
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    current: u64,
    total: u64,
    output: &str,
) -> Result<(), SerializedError> {
    if let Some(channel) = response_channel {
        crate::emit_response_value(window, Some(channel), json!({ "current": current, "total": total, "data": output }))?;
    }
    Ok(())
}

fn copy_map_directory(source: &Path, destination: &Path) -> Result<(), SerializedError> {
    copy_map_directory_inner(source, destination, None)
}

fn copy_map_directory_cancellable(source: &Path, destination: &Path, cancelled: &AtomicBool) -> Result<(), SerializedError> {
    copy_map_directory_inner(source, destination, Some(cancelled))
}

fn copy_map_directory_inner(
    source: &Path,
    destination: &Path,
    cancelled: Option<&AtomicBool>,
) -> Result<(), SerializedError> {
    if fs::symlink_metadata(destination).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(serialized_error("Map destination cannot be a symbolic link", "INVALID_PATH"));
    }
    if destination.exists() && !destination.is_dir() {
        return Err(serialized_error("Map destination must be a folder", "INVALID_PATH"));
    }
    let parent = destination.parent()
        .ok_or_else(|| serialized_error("Map destination has no parent folder", "INVALID_PATH"))?;
    let staging = parent.join(format!(".bsmanager-map-copy-{}", uuid::Uuid::new_v4()));
    let backup = parent.join(format!(".bsmanager-map-copy-backup-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        if destination.is_dir() {
            copy_map_tree(destination, &staging, cancelled)?;
        } else {
            fs::create_dir_all(&staging)?;
        }
        copy_map_tree(source, &staging, cancelled)?;
        check_map_copy_cancelled(cancelled)?;

        let had_destination = destination.exists();
        if had_destination {
            fs::rename(destination, &backup)?;
        }
        if let Err(error) = fs::rename(&staging, destination) {
            if had_destination { let _ = fs::rename(&backup, destination); }
            return Err(error);
        }
        if had_destination { let _ = fs::remove_dir_all(&backup); }
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_dir_all(&staging); }
    result.map_err(|error: io::Error| {
        if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Relaxed)) {
            serialized_error("Map copy was cancelled", "REQUEST_CANCELLED")
        } else {
            serialized_error(error.to_string(), "MAP_COPY_FAILED")
        }
    })
}

fn check_map_copy_cancelled(cancelled: Option<&AtomicBool>) -> io::Result<()> {
    if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Relaxed)) {
        Err(io::Error::new(io::ErrorKind::Interrupted, "Map copy was cancelled"))
    } else {
        Ok(())
    }
}

fn copy_map_tree(source: &Path, destination: &Path, cancelled: Option<&AtomicBool>) -> io::Result<()> {
    check_map_copy_cancelled(cancelled)?;
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        check_map_copy_cancelled(cancelled)?;
        let entry = entry?;
        let kind = entry.file_type()?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        if kind.is_symlink() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Map contains a symbolic link"));
        }
        if kind.is_dir() {
            copy_map_tree(&from, &to, cancelled)?;
        } else if kind.is_file() {
            copy_map_file(&from, &to, cancelled)?;
        }
    }
    Ok(())
}

fn copy_map_file(source: &Path, destination: &Path, cancelled: Option<&AtomicBool>) -> io::Result<()> {
    let mut input = File::open(source)?;
    let mut output = File::create(destination)?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        check_map_copy_cancelled(cancelled)?;
        let read = input.read(&mut buffer)?;
        if read == 0 { return Ok(()); }
        io::Write::write_all(&mut output, &buffer[..read])?;
    }
}

fn sanitize_folder_name(value: &str) -> String {
    let sanitized = value.chars().map(|character| {
        if character.is_control() || "<>:\"/\\|?*".contains(character) { '_' } else { character }
    }).collect::<String>();
    let sanitized = sanitized.trim().trim_end_matches(['.', ' ']);
    if sanitized.is_empty() { "map".to_owned() } else { sanitized.to_owned() }
}

fn map_folder<R: Runtime>(app: &AppHandle<R>, version: &Value) -> Result<PathBuf, SerializedError> {
    if version.is_null() {
        return Ok(linker::shared_folder(app)?.join(SHARED_MAPS_FOLDER).join(CUSTOM_LEVELS_FOLDER));
    }
    Ok(versions::version_path(app, version)?.join(LEVELS_FOLDER).join(CUSTOM_LEVELS_FOLDER))
}

fn managed_map_roots<R: Runtime>(app: &AppHandle<R>) -> Result<Vec<PathBuf>, SerializedError> {
    let mut candidates = vec![linker::shared_folder(app)?.join(SHARED_MAPS_FOLDER).join(CUSTOM_LEVELS_FOLDER)];
    let installed = versions::get_installed_versions(app, false)?;
    if let Some(versions) = installed.as_array() {
        for version in versions {
            candidates.push(map_folder(app, version)?);
        }
    }
    let mut roots = Vec::new();
    for candidate in candidates {
        if let Ok(root) = fs::canonicalize(candidate) {
            if root.is_dir() && !roots.contains(&root) { roots.push(root); }
        }
    }
    Ok(roots)
}

fn load_map_info(path: &Path) -> Result<Option<Value>, String> {
    let dirname = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    let cacheable = !dirname.starts_with(".bsmanager-");
    let cached = cacheable.then(|| crate::map_info_cache::get(dirname)).flatten();
    let (map_info, hash) = if let Some(cached) = cached {
        (cached.map_info, cached.hash)
    } else {
        let entries = fs::read_dir(path).map_err(|error| error.to_string())?;
        let info_path = entries.filter_map(Result::ok).map(|entry| entry.path())
            .find(|entry| entry.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.eq_ignore_ascii_case("info.dat")));
        let Some(info_path) = info_path else { return Ok(None); };
        let raw_text = fs::read_to_string(&info_path).map_err(|error| error.to_string())?;
        let raw: Value = serde_json::from_str(&raw_text).map_err(|error| error.to_string())?;
        let map_info = parse_map_info(&raw)?;
        let hash = compute_map_hash(path, &raw_text, &map_info)?;
        if cacheable {
            crate::map_info_cache::remember(dirname, map_info.clone(), &hash);
        }
        (map_info, hash)
    };
    let metadata = load_or_create_metadata(path)?;
    let song_filename = map_info.get("songFilename").and_then(Value::as_str)
        .ok_or_else(|| "Map song filename is missing".to_owned())?;
    let cover_filename = map_info.get("coverImageFilename").and_then(Value::as_str)
        .ok_or_else(|| "Map cover filename is missing".to_owned())?;
    let song_path = safe_child(path, song_filename)?;
    let cover_path = safe_child(path, cover_filename)?;
    let song_url = Url::from_file_path(&song_path).map_err(|_| "Could not create song file URL".to_owned())?;
    let cover_url = Url::from_file_path(&cover_path).map_err(|_| "Could not create cover file URL".to_owned())?;
    let mut result = json!({
        "mapInfo": map_info,
        "coverUrl": cover_url,
        "songUrl": song_url,
        "hash": hash,
        "path": path,
        "metadata": metadata,
    }).as_object().cloned().ok_or_else(|| "Could not construct local map details".to_owned())?;
    if let Some(song_details) = crate::song_cache::by_hash(&hash) {
        result.insert("songDetails".to_owned(), song_details);
    }
    Ok(Some(Value::Object(result)))
}

fn parse_map_info(raw: &Value) -> Result<Value, String> {
    let version = raw.get("_version").or_else(|| raw.get("version"))
        .and_then(Value::as_str).ok_or_else(|| "Cannot determine info.dat version".to_owned())?;
    if version.starts_with("2.") || version.starts_with("3.") {
        return parse_legacy_map_info(raw, version);
    }
    if version.starts_with("4.") {
        return parse_v4_map_info(raw, version);
    }
    Err(format!("Unsupported info.dat version: {version}"))
}

fn parse_legacy_map_info(raw: &Value, version: &str) -> Result<Value, String> {
    let mut info = Map::new();
    info.insert("version".into(), json!(version));
    info.insert("songName".into(), required(raw, "_songName")?.clone());
    put_optional(&mut info, "songSubName", raw.get("_songSubName"));
    info.insert("songAuthorName".into(), required(raw, "_songAuthorName")?.clone());
    info.insert("levelMappers".into(), json!([required(raw, "_levelAuthorName")?]));
    info.insert("levelLighters".into(), json!([]));
    info.insert("beatsPerMinute".into(), required(raw, "_beatsPerMinute")?.clone());
    put_optional(&mut info, "shuffle", raw.get("_shuffle"));
    put_optional(&mut info, "shufflePeriod", raw.get("_shufflePeriod"));
    info.insert("previewStartTime".into(), required(raw, "_previewStartTime")?.clone());
    info.insert("previewDuration".into(), required(raw, "_previewDuration")?.clone());
    let song_filename = required(raw, "_songFilename")?.clone();
    info.insert("songFilename".into(), song_filename.clone());
    info.insert("songPreviewFilename".into(), song_filename);
    info.insert("coverImageFilename".into(), required(raw, "_coverImageFilename")?.clone());
    let environments = raw.get("_environmentNames").and_then(Value::as_array).cloned()
        .or_else(|| raw.get("_environmentName").cloned().map(|value| vec![value]))
        .unwrap_or_default();
    info.insert("environmentNames".into(), json!(environments));
    let sets = raw.get("_difficultyBeatmapSets").and_then(Value::as_array)
        .ok_or_else(|| "Map difficulty sets are missing".to_owned())?;
    let mut difficulties = Vec::new();
    for set in sets {
        let characteristic = required(set, "_beatmapCharacteristicName")?.clone();
        let entries = set.get("_difficultyBeatmaps").and_then(Value::as_array)
            .ok_or_else(|| "Map difficulty entries are missing".to_owned())?;
        for entry in entries {
            let mut difficulty = Map::new();
            difficulty.insert("characteristic".into(), characteristic.clone());
            difficulty.insert("difficulty".into(), required(entry, "_difficulty")?.clone());
            put_optional(&mut difficulty, "difficultyLabel", entry.pointer("/_customData/_difficultyLabel"));
            difficulty.insert("beatmapFilename".into(), required(entry, "_beatmapFilename")?.clone());
            difficulty.insert("noteJumpMovementSpeed".into(), required(entry, "_noteJumpMovementSpeed")?.clone());
            difficulty.insert("noteJumpStartBeatOffset".into(), required(entry, "_noteJumpStartBeatOffset")?.clone());
            put_optional(&mut difficulty, "beatmapColorSchemeIdx", entry.get("_beatmapColorSchemeIdx"));
            put_optional(&mut difficulty, "environmentNameIdx", entry.get("_environmentNameIdx"));
            difficulties.push(Value::Object(difficulty));
        }
    }
    info.insert("difficulties".into(), json!(difficulties));
    Ok(Value::Object(info))
}

fn parse_v4_map_info(raw: &Value, version: &str) -> Result<Value, String> {
    let song = required(raw, "song")?;
    let audio = required(raw, "audio")?;
    let mut info = Map::new();
    info.insert("version".into(), json!(version));
    info.insert("songName".into(), required(song, "title")?.clone());
    put_optional(&mut info, "songSubName", song.get("subTitle"));
    info.insert("songAuthorName".into(), required(song, "author")?.clone());
    let beatmaps = raw.get("difficultyBeatmaps").and_then(Value::as_array)
        .ok_or_else(|| "Map difficulty entries are missing".to_owned())?;
    let mappers = beatmaps.iter().filter_map(|difficulty| difficulty.pointer("/beatmapAuthors/mappers").and_then(Value::as_array))
        .flatten().cloned().collect::<Vec<_>>();
    let lighters = beatmaps.iter().filter_map(|difficulty| difficulty.pointer("/beatmapAuthors/lighters").and_then(Value::as_array))
        .flatten().cloned().collect::<Vec<_>>();
    info.insert("levelMappers".into(), json!(mappers));
    info.insert("levelLighters".into(), json!(lighters));
    info.insert("beatsPerMinute".into(), required(audio, "bpm")?.clone());
    info.insert("previewStartTime".into(), required(audio, "previewStartTime")?.clone());
    info.insert("previewDuration".into(), required(audio, "previewDuration")?.clone());
    info.insert("songFilename".into(), required(audio, "songFilename")?.clone());
    info.insert("songPreviewFilename".into(), required(raw, "songPreviewFilename")?.clone());
    info.insert("coverImageFilename".into(), required(raw, "coverImageFilename")?.clone());
    info.insert("environmentNames".into(), required(raw, "environmentNames")?.clone());
    let mut difficulties = Vec::new();
    for entry in beatmaps {
        let mut difficulty = Map::new();
        for (target, source) in [
            ("characteristic", "characteristic"), ("difficulty", "difficulty"),
            ("beatmapFilename", "beatmapDataFilename"),
            ("noteJumpMovementSpeed", "noteJumpMovementSpeed"),
            ("noteJumpStartBeatOffset", "noteJumpStartBeatOffset"),
            ("beatmapColorSchemeIdx", "beatmapColorSchemeIdx"),
            ("environmentNameIdx", "environmentNameIdx"),
        ] {
            difficulty.insert(target.into(), required(entry, source)?.clone());
        }
        put_optional(&mut difficulty, "beatmapAuthors", entry.get("beatmapAuthors"));
        put_optional(&mut difficulty, "lightshowDataFilename", entry.get("lightshowDataFilename"));
        difficulties.push(Value::Object(difficulty));
    }
    info.insert("difficulties".into(), json!(difficulties));
    Ok(Value::Object(info))
}

fn compute_map_hash(path: &Path, raw_info: &str, map_info: &Value) -> Result<String, String> {
    let mut hasher = Sha1::new();
    hasher.update(raw_info.as_bytes());
    let difficulties = map_info.get("difficulties").and_then(Value::as_array)
        .ok_or_else(|| "Map difficulties are missing".to_owned())?;
    for difficulty in difficulties {
        for key in ["beatmapFilename", "lightshowDataFilename"] {
            if let Some(filename) = difficulty.get(key).and_then(Value::as_str).filter(|value| !value.is_empty()) {
                let file_path = safe_child(path, filename)?;
                let mut file = File::open(file_path).map_err(|error| error.to_string())?;
                let mut buffer = [0_u8; 64 * 1024];
                loop {
                    let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
                    if read == 0 { break; }
                    hasher.update(&buffer[..read]);
                }
            }
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn safe_child(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let relative = Path::new(relative);
    if relative.components().any(|component| !matches!(component, Component::Normal(_))) {
        return Err("Map file path is invalid".to_owned());
    }
    Ok(root.join(relative))
}

fn load_or_create_metadata(path: &Path) -> Result<Value, String> {
    let metadata_path = path.join(METADATA_FILE);
    if metadata_path.is_file() {
        let contents = fs::read(&metadata_path).map_err(|error| error.to_string())?;
        return serde_json::from_slice(&contents).map_err(|error| error.to_string());
    }
    let metadata = json!({ "addedDate": Local::now().format("%Y-%m-%dT%H:%M:%S%.3f").to_string() });
    fs::write(&metadata_path, serde_json::to_vec_pretty(&metadata).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    Ok(metadata)
}

fn required<'a>(value: &'a Value, key: &str) -> Result<&'a Value, String> {
    value.get(key).filter(|value| !value.is_null())
        .ok_or_else(|| format!("Map info field '{key}' is missing"))
}

fn put_optional(output: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(value) = value.filter(|value| !value.is_null()) {
        output.insert(key.to_owned(), value.clone());
    }
}

fn emit_load_progress<R: Runtime>(
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    total: u64,
    loaded: u64,
    maps: &[Value],
) -> Result<(), SerializedError> {
    if let Some(channel) = response_channel {
        crate::emit_response_value(window, Some(channel), json!({ "total": total, "loaded": loaded, "maps": maps }))?;
    }
    Ok(())
}

fn emit_delete_progress<R: Runtime>(
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    total: u64,
    deleted: u64,
) -> Result<(), SerializedError> {
    if let Some(channel) = response_channel {
        crate::emit_response_value(window, Some(channel), json!({ "total": total, "deleted": deleted }))?;
    }
    Ok(())
}

fn emit_hash_delete_progress<R: Runtime>(
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    current: u64,
    total: u64,
) -> Result<(), SerializedError> {
    if let Some(channel) = response_channel {
        crate::emit_response_value(window, Some(channel), json!({ "current": current, "total": total }))?;
    }
    Ok(())
}
