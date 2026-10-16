use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    sync::{Arc, atomic::{AtomicBool, Ordering}},
};

use serde_json::{Value, json};
use tauri::{AppHandle, Runtime, WebviewWindow};
use std::time::Duration;
use url::Url;
use zip::{CompressionMethod, write::{SimpleFileOptions, ZipWriter}};

use crate::{SerializedError, linker, maps, serialized_error, versions};

const PLAYLISTS_FOLDER: &str = "Playlists";
const PLAYLIST_EXTENSIONS: &[&str] = &["bplist", "json"];

pub fn install_playlist<R: Runtime>(app: &AppHandle<R>, args: &Value) -> Result<Value, SerializedError> {
    let playlist = normalize_playlist(args.get("bplist").cloned().unwrap_or(Value::Null))?;
    let version = args.get("version").filter(|version| !version.is_null()).unwrap_or(&Value::Null);
    let requested_path = args.get("dest").and_then(Value::as_str).map(PathBuf::from);
    let path = if let Some(path) = requested_path.filter(|path| path.is_absolute() && accept_playlist_file(path)) {
        if !path.parent().is_some_and(Path::is_dir) {
            return Err(serialized_error("Playlist destination folder does not exist", "PLAYLIST_PATH_FAILED"));
        }
        path
    } else {
        let folder = playlist_folder(app, version)?;
        fs::create_dir_all(&folder).map_err(|error| serialized_error(error.to_string(), "PLAYLIST_PATH_FAILED"))?;
        let title = playlist.get("playlistTitle").and_then(Value::as_str).unwrap_or("Playlist");
        unique_path(folder.join(format!("{}.bplist", sanitize_filename(title))))
    };
    write_playlist(&path, &playlist, true)?;
    Ok(local_playlist_details(playlist, &path))
}

pub async fn get_version_playlists<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    version: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let _ = tokio::time::timeout(Duration::from_secs(15), crate::song_cache::initialize(app)).await;
    let folder = playlist_folder(app, version)?;
    let folder_for_scan = folder.clone();
    let files = match tokio::task::spawn_blocking(move || playlist_files(&folder_for_scan)).await
        .map_err(|error| serialized_error(error.to_string(), "PLAYLIST_LIST_FAILED"))? {
        Ok(files) => files,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(serialized_error(error.to_string(), "PLAYLIST_LIST_FAILED")),
    };
    let total = files.len() as u64;
    let mut playlists = Vec::new();
    emit_list_progress(window, response_channel, 0, total, json!([]))?;
    for path in files {
        if cancelled.load(Ordering::Relaxed) {
            return Err(serialized_error("Playlist listing was cancelled", "REQUEST_CANCELLED"));
        }
        let path_for_read = path.clone();
        let parsed = tokio::task::spawn_blocking(move || read_playlist_file(&path_for_read)).await
            .map_err(|error| serialized_error(error.to_string(), "PLAYLIST_READ_FAILED"))?;
        if let Ok(playlist) = parsed {
            playlists.push(local_playlist_details(playlist, &path));
            emit_list_progress(window, response_channel, playlists.len() as u64, total, json!([]))?;
        }
    }
    emit_list_progress(window, response_channel, playlists.len() as u64, total, json!(playlists))?;
    Ok(Value::Null)
}

pub async fn import_playlists<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    args: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let paths = args.get("paths").and_then(Value::as_array)
        .ok_or_else(|| serialized_error("Playlist paths must be an array", "INVALID_PATHS"))?;
    let version = args.get("version").filter(|version| !version.is_null()).unwrap_or(&Value::Null);
    let paths = paths.iter().filter_map(Value::as_str).map(PathBuf::from)
        .filter(|path| accept_playlist_file(path)).collect::<Vec<_>>();
    let total = paths.len() as u64;
    let mut current = 0_u64;
    emit_import_progress(window, response_channel, current, total, None, None)?;
    for source in paths {
        if cancelled.load(Ordering::Relaxed) {
            return Err(serialized_error("Playlist import was cancelled", "REQUEST_CANCELLED"));
        }
        match import_playlist_file(app, &source, version) {
            Ok(details) => {
                current += 1;
                emit_import_progress(window, response_channel, current, total, Some(details), None)?;
            }
            Err(error) => {
                let last_error = json!({ "message": error.message, "code": error.code });
                emit_import_progress(window, response_channel, current, total, None, Some(last_error))?;
                log::error!("Playlist import failed: {}", error.message);
            }
        }
    }
    Ok(Value::Null)
}

pub async fn delete_playlist<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    args: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let version = args.get("version").filter(|version| !version.is_null()).unwrap_or(&Value::Null);
    let playlist = args.get("bpList").ok_or_else(|| serialized_error("Playlist is required", "INVALID_PLAYLIST"))?;
    let path = playlist.get("path").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Playlist path is required", "INVALID_PATH"))?;
    let root = playlist_folder(app, version)?;
    let root = fs::canonicalize(root).map_err(|error| serialized_error(error.to_string(), "PLAYLIST_PATH_FAILED"))?;
    let path = fs::canonicalize(path).map_err(|error| serialized_error(error.to_string(), "PLAYLIST_PATH_FAILED"))?;
    if !path.starts_with(&root) || !path.is_file() || !accept_playlist_file(&path) {
        return Err(serialized_error("Playlist file is outside the selected library", "INVALID_PATH"));
    }
    tokio::fs::remove_file(path).await
        .map_err(|error| serialized_error(error.to_string(), "PLAYLIST_DELETE_FAILED"))?;
    if args.get("deleteMaps").and_then(Value::as_bool).unwrap_or(false) {
        let hashes = playlist.pointer("/songs").and_then(Value::as_array).into_iter().flatten()
            .filter_map(|song| song.get("hash").and_then(Value::as_str).and_then(find_hash))
            .collect::<Vec<_>>();
        maps::delete_maps_by_hashes(app, window, response_channel, version, &hashes, cancelled).await?;
    } else {
        emit_delete_progress(window, response_channel, 0, 0)?;
    }
    Ok(Value::Null)
}

pub async fn export_playlists<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    args: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let selected = args.get("bpLists").and_then(Value::as_array)
        .ok_or_else(|| serialized_error("Playlists must be an array", "INVALID_PLAYLISTS"))?;
    if selected.is_empty() {
        return Err(serialized_error("No playlists to export", "NO_PLAYLISTS"));
    }
    let version = args.get("version").filter(|version| !version.is_null()).unwrap_or(&Value::Null);
    let root = fs::canonicalize(playlist_folder(app, version)?)
        .map_err(|error| serialized_error(error.to_string(), "PLAYLIST_PATH_FAILED"))?;
    let destination = args.get("dest").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Export destination is required", "INVALID_PATH"))?;
    let destination = PathBuf::from(destination);
    if !destination.is_dir() {
        return Err(serialized_error("Export destination folder does not exist", "DEST_ENOENT"));
    }
    let display_name = args.pointer("/version/name").and_then(Value::as_str)
        .or_else(|| args.pointer("/version/BSVersion").and_then(Value::as_str)).unwrap_or("Playlists");
    let archive_name = if version.is_null() { "Playlists.zip".to_owned() }
        else { format!("{} Playlists.zip", sanitize_filename(display_name)) };
    let output = destination.join(archive_name);
    let mut files = Vec::new();
    for playlist in selected {
        let Some(path) = playlist.get("path").and_then(Value::as_str) else { continue; };
        let Ok(path) = fs::canonicalize(path) else { continue; };
        if path.starts_with(&root) && path.is_file() && accept_playlist_file(&path) {
            let name = Path::new(PLAYLISTS_FOLDER).join(path.file_name().unwrap_or_default());
            files.push((path, name.to_string_lossy().replace('\\', "/")));
        }
    }
    if let Some(maps_list) = args.get("playlistsMaps").and_then(Value::as_array) {
        for map in maps_list {
            let Some(path) = map.get("path").and_then(Value::as_str) else { continue; };
            let path = match maps::validate_managed_map_path(app, Path::new(path)) {
                Ok(path) => path,
                Err(_) => continue,
            };
            let prefix = Path::new("Maps").join(path.file_name().unwrap_or_default());
            collect_export_files(&path, &path, &prefix, &mut files)
                .map_err(|error| serialized_error(error.to_string(), "PLAYLIST_EXPORT_FAILED"))?;
        }
    }
    let total = files.len() as u64;
    let output_text = output.to_string_lossy().to_string();
    emit_archive_progress(window, response_channel, 0, total, &output_text)?;
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<u64>();
    let worker_cancelled = cancelled.clone();
    let output_for_worker = output.clone();
    let worker = tokio::task::spawn_blocking(move || write_playlist_archive(&output_for_worker, files, worker_cancelled, sender));
    let mut emit_error = None;
    while let Some(current) = receiver.recv().await {
        if let Err(error) = emit_archive_progress(window, response_channel, current, total, &output_text) {
            cancelled.store(true, Ordering::Relaxed);
            emit_error = Some(error);
        }
    }
    let result = worker.await.map_err(|error| serialized_error(error.to_string(), "PLAYLIST_EXPORT_FAILED"))?;
    if let Some(error) = emit_error { return Err(error); }
    if let Err(error) = result {
        if cancelled.load(Ordering::Relaxed) {
            return Err(serialized_error("Playlist export was cancelled", "REQUEST_CANCELLED"));
        }
        return Err(serialized_error(error, "PLAYLIST_EXPORT_FAILED"));
    }
    Ok(Value::Null)
}

pub async fn download_playlist<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    args: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let source = args.get("downloadSource").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Playlist source is required", "INVALID_SOURCE"))?;
    let version = args.get("version").filter(|value| !value.is_null()).cloned().unwrap_or(Value::Null);
    let ignore_hashes = args.get("ignoreSongsHashs").and_then(Value::as_array).into_iter().flatten()
        .filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>();
    let destination = args.get("dest").and_then(Value::as_str);
    download_playlist_content(app, window, response_channel, source, &version, &ignore_hashes, destination, cancelled).await?;
    Ok(Value::Null)
}

pub async fn one_click_install_playlist<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    source: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let source = source.as_str().ok_or_else(|| serialized_error("Playlist source must be a string", "INVALID_SOURCE"))?;
    let mut installed = versions::get_installed_versions(app, false)?.as_array().cloned().unwrap_or_default();
    let target_version = installed.pop().unwrap_or(Value::Null);
    let (playlist, downloaded_maps) = download_playlist_content(
        app, window, response_channel, source, &target_version, &[], None, cancelled.clone(),
    ).await?;
    if downloaded_maps.is_empty() { return Ok(Value::Null); }
    let playlist_path = playlist.get("path").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Installed playlist path is missing", "PLAYLIST_WRITE_FAILED"))?;
    let filename = Path::new(playlist_path).file_name().unwrap_or_default().to_string_lossy().to_string();
    let playlist_data = playlist_source_data(&playlist);
    for version in installed {
        if cancelled.load(Ordering::Relaxed) {
            return Err(serialized_error("Playlist installation was cancelled", "REQUEST_CANCELLED"));
        }
        let _ = persist_playlist(app, &playlist_data, &filename, &version, None)?;
        for map in &downloaded_maps {
            let path = map.get("path").and_then(Value::as_str)
                .ok_or_else(|| serialized_error("Downloaded map path is missing", "MAP_DOWNLOAD_FAILED"))?;
            maps::copy_managed_map_to_version(app, Path::new(path), &version).await?;
        }
    }
    Ok(Value::Null)
}

async fn download_playlist_content<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    source: &str,
    version: &Value,
    ignore_hashes: &[String],
    destination: Option<&str>,
    cancelled: Arc<AtomicBool>,
) -> Result<(Value, Vec<Value>), SerializedError> {
    let (playlist, filename) = read_playlist_source(source).await?;
    let playlist = persist_playlist(app, &playlist, &filename, version, destination)?;
    let path = PathBuf::from(playlist.get("path").and_then(Value::as_str).unwrap_or_default());
    let details = local_playlist_details(playlist, &path);
    let songs = details.get("songs").and_then(Value::as_array).cloned().unwrap_or_default();
    let total = songs.len() as u64;
    let mut current = 0_u64;
    let mut downloaded_maps = Vec::new();
    let ignored_hashes = ignore_hashes.iter().map(|hash| hash.to_ascii_lowercase()).collect::<HashSet<_>>();
    let hashes = songs.iter().filter_map(|song| {
        song.get("hash").or_else(|| song.get("levelid")).and_then(Value::as_str).and_then(find_hash)
    }).map(|hash| hash.to_ascii_lowercase()).filter(|hash| !ignored_hashes.contains(hash)).collect::<HashSet<_>>();
    let prefetched = if hashes.is_empty() {
        HashMap::new()
    } else {
        match crate::beat_saver::dispatch("bsv-get-map-details-from-hashs", &json!(hashes.iter().collect::<Vec<_>>())).await {
            Ok(result) => result.as_array().into_iter().flatten().filter_map(|map| {
                map.pointer("/versions/0/hash").and_then(Value::as_str)
                    .map(|hash| (hash.to_ascii_lowercase(), map.clone()))
            }).collect(),
            Err(error) => {
                log::warn!("Could not prefetch playlist map details: {}", error.message);
                HashMap::new()
            }
        }
    };
    let mut progression_data = json!({
        "downloadedMaps": downloaded_maps,
        "currentDownload": Value::Null,
        "playlist": details,
    });
    emit_download_progress(window, response_channel, current, total, progression_data.clone())?;
    for song in songs {
        if cancelled.load(Ordering::Relaxed) {
            return Err(serialized_error("Playlist download was cancelled", "REQUEST_CANCELLED"));
        }
        let song_hash = song.get("hash").and_then(Value::as_str).and_then(find_hash);
        if song_hash.as_ref().is_some_and(|hash| ignored_hashes.contains(&hash.to_ascii_lowercase())) {
            current += 1;
            emit_download_progress(window, response_channel, current, total, progression_data.clone())?;
            continue;
        }
        let detail = playlist_song_map_detail(&song, &prefetched).await;
        if let Some(detail) = detail {
            let result = maps::download_map(app, &json!({ "map": detail, "version": version }), cancelled.clone()).await;
            match result {
                Ok(map) => {
                    downloaded_maps.push(map);
                    progression_data["downloadedMaps"] = json!(downloaded_maps);
                    progression_data["currentDownload"] = detail;
                }
                Err(error) => log::warn!("Could not download a playlist map: {}", error.message),
            }
        }
        current += 1;
        emit_download_progress(window, response_channel, current, total, progression_data.clone())?;
    }
    Ok((details, downloaded_maps))
}

async fn playlist_song_map_detail(song: &Value, prefetched: &HashMap<String, Value>) -> Option<Value> {
    if let Some(hash) = song.get("hash").and_then(Value::as_str).and_then(find_hash) {
        if let Some(map) = prefetched.get(&hash.to_ascii_lowercase()) { return Some(map.clone()); }
        if let Ok(result) = crate::beat_saver::dispatch("bsv-get-map-details-from-hashs", &json!([hash])).await {
            if let Some(map) = result.as_array().and_then(|maps| maps.first()) { return Some(map.clone()); }
        }
    }
    if let Some(key) = song.get("key").and_then(Value::as_str) {
        if let Ok(map) = crate::beat_saver::dispatch("bsv-get-map-details-by-id", &json!(key)).await { return Some(map); }
    }
    if let Some(hash) = song.get("levelid").and_then(Value::as_str).and_then(find_hash) {
        if let Some(map) = prefetched.get(&hash.to_ascii_lowercase()) { return Some(map.clone()); }
        if let Ok(result) = crate::beat_saver::dispatch("bsv-get-map-details-from-hashs", &json!([hash])).await {
            if let Some(map) = result.as_array().and_then(|maps| maps.first()) { return Some(map.clone()); }
        }
    }
    None
}

async fn read_playlist_source(source: &str) -> Result<(Value, String), SerializedError> {
    let path = PathBuf::from(source);
    if path.is_file() {
        let playlist = read_playlist_file(&path)?;
        let filename = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        return Ok((playlist, filename));
    }
    let url = Url::parse(source).map_err(|error| serialized_error(error.to_string(), "INVALID_SOURCE"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(serialized_error("Playlist URLs must use HTTP or HTTPS", "INVALID_SOURCE"));
    }
    let response = crate::proxy::client_builder().connect_timeout(Duration::from_secs(20)).timeout(Duration::from_secs(60))
        .user_agent(format!("BSManager/{}", env!("CARGO_PKG_VERSION"))).build()
        .map_err(|error| serialized_error(error.to_string(), "PLAYLIST_DOWNLOAD_FAILED"))?
        .get(url).send().await.and_then(reqwest::Response::error_for_status)
        .map_err(|error| serialized_error(error.to_string(), "PLAYLIST_DOWNLOAD_FAILED"))?;
    let header = response.headers().get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok()).map(str::to_owned);
    let text = response.text().await.map_err(|error| serialized_error(error.to_string(), "PLAYLIST_DOWNLOAD_FAILED"))?;
    let playlist = normalize_playlist(serde_json::from_str(&text)
        .map_err(|error| serialized_error(error.to_string(), "CANNOT_PARSE_PLAYLIST"))?)?;
    let filename = header.as_deref().and_then(content_disposition_filename)
        .unwrap_or_else(|| format!("{}.bplist", playlist.get("playlistTitle").and_then(Value::as_str).unwrap_or("Playlist")));
    Ok((playlist, filename))
}

fn persist_playlist<R: Runtime>(
    app: &AppHandle<R>,
    playlist: &Value,
    filename: &str,
    version: &Value,
    destination: Option<&str>,
) -> Result<Value, SerializedError> {
    if let Some(path) = destination.map(PathBuf::from).filter(|path| path.is_absolute() && accept_playlist_file(path)) {
        if !path.parent().is_some_and(Path::is_dir) {
            return Err(serialized_error("Playlist destination folder does not exist", "PLAYLIST_PATH_FAILED"));
        }
        write_playlist(&path, playlist, true)?;
        return Ok(local_playlist_details(playlist.clone(), &path));
    }
    let folder = playlist_folder(app, version)?;
    fs::create_dir_all(&folder).map_err(|error| serialized_error(error.to_string(), "PLAYLIST_PATH_FAILED"))?;
    let sync_url = playlist.pointer("/customData/syncURL").and_then(Value::as_str);
    let existing = if sync_url.is_some() {
        playlist_files(&folder).map_err(|error| serialized_error(error.to_string(), "PLAYLIST_LIST_FAILED"))?
            .into_iter().find(|path| read_playlist_file(path).ok()
                .and_then(|value| value.pointer("/customData/syncURL").and_then(Value::as_str).map(str::to_owned))
                .as_deref() == sync_url)
    } else { None };
    let path = existing.unwrap_or_else(|| unique_path(folder.join(sanitize_filename(filename))));
    write_playlist(&path, playlist, path.exists())?;
    Ok(local_playlist_details(playlist.clone(), &path))
}

fn content_disposition_filename(header: &str) -> Option<String> {
    for part in header.split(';').map(str::trim) {
        if let Some(value) = part.strip_prefix("filename*=UTF-8''").or_else(|| part.strip_prefix("filename=")) {
            let value = value.trim_matches(['\"', '\'']);
            let decoded = urlencoding::decode(value).ok()?.into_owned();
            if !decoded.trim().is_empty() { return Some(decoded); }
        }
    }
    None
}

fn playlist_source_data(details: &Value) -> Value {
    let mut playlist = details.as_object().cloned().unwrap_or_default();
    for key in ["path", "nbMaps", "id", "nbMappers", "duration", "minNps", "maxNps"] {
        playlist.remove(key);
    }
    Value::Object(playlist)
}

fn playlist_folder<R: Runtime>(app: &AppHandle<R>, version: &Value) -> Result<PathBuf, SerializedError> {
    let parent = if version.is_null() { linker::shared_folder(app)? } else { versions::version_path(app, version)? };
    Ok(parent.join(PLAYLISTS_FOLDER))
}

fn normalize_playlist(mut playlist: Value) -> Result<Value, SerializedError> {
    let object = playlist.as_object_mut()
        .ok_or_else(|| serialized_error("Playlist data must be an object", "INVALID_PLAYLIST"))?;
    if !object.get("playlistTitle").and_then(Value::as_str).is_some_and(|title| !title.trim().is_empty()) {
        return Err(serialized_error("Playlist title is missing", "INVALID_PLAYLIST_FILE"));
    }
    let songs = object.entry("songs").or_insert_with(|| json!([]));
    if !songs.is_array() {
        return Err(serialized_error("Playlist songs must be an array", "INVALID_PLAYLIST_FILE"));
    }
    for song in songs.as_array_mut().into_iter().flatten() {
        if let Some(hash) = song.get("hash").and_then(Value::as_str).and_then(find_hash) {
            if let Some(song) = song.as_object_mut() { song.insert("hash".into(), json!(hash)); }
        }
    }
    Ok(playlist)
}

fn import_playlist_file<R: Runtime>(app: &AppHandle<R>, source: &Path, version: &Value) -> Result<Value, SerializedError> {
    let playlist = read_playlist_file(source)?;
    let folder = playlist_folder(app, version)?;
    fs::create_dir_all(&folder).map_err(|error| serialized_error(error.to_string(), "PLAYLIST_PATH_FAILED"))?;
    let sync_url = playlist.pointer("/customData/syncURL").and_then(Value::as_str);
    let existing = if sync_url.is_some() {
        playlist_files(&folder).map_err(|error| serialized_error(error.to_string(), "PLAYLIST_LIST_FAILED"))?
            .into_iter().find(|path| read_playlist_file(path).ok()
                .and_then(|value| value.pointer("/customData/syncURL").and_then(Value::as_str).map(str::to_owned))
                .as_deref() == sync_url)
    } else { None };
    let destination = existing.unwrap_or_else(|| unique_path(folder.join(source.file_name().unwrap_or_default())));
    write_playlist(&destination, &playlist, destination.exists())?;
    Ok(local_playlist_details(playlist, &destination))
}

fn read_playlist_file(path: &Path) -> Result<Value, SerializedError> {
    let bytes = fs::read(path).map_err(|error| serialized_error(error.to_string(), "PLAYLIST_READ_FAILED"))?;
    let playlist = serde_json::from_slice(&bytes)
        .map_err(|error| serialized_error(error.to_string(), "CANNOT_PARSE_PLAYLIST"))?;
    normalize_playlist(playlist)
}

fn write_playlist(path: &Path, playlist: &Value, overwrite: bool) -> Result<(), SerializedError> {
    let bytes = serde_json::to_vec_pretty(playlist)
        .map_err(|error| serialized_error(error.to_string(), "PLAYLIST_WRITE_FAILED"))?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(overwrite).create_new(!overwrite);
    use std::io::Write;
    options.open(path).and_then(|mut file| file.write_all(&bytes))
        .map_err(|error| serialized_error(error.to_string(), "PLAYLIST_WRITE_FAILED"))
}

fn local_playlist_details(playlist: Value, path: &Path) -> Value {
    let mut songs = playlist.get("songs").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut mappers = HashSet::new();
    let mut duration = 0.0_f64;
    let mut min_nps = f64::INFINITY;
    let mut max_nps = f64::NEG_INFINITY;
    for song in &mut songs {
        let details = song.get("hash").and_then(Value::as_str).and_then(find_hash)
            .and_then(|hash| crate::beat_saver::song_details_from_cache(&hash))
            .or_else(|| song.get("levelid").and_then(Value::as_str).and_then(find_hash)
                .and_then(|hash| crate::beat_saver::song_details_from_cache(&hash)))
            .or_else(|| song.get("key").and_then(Value::as_str)
                .and_then(crate::beat_saver::song_details_by_id_from_cache));
        let Some(details) = details else { continue; };
        duration += details.get("duration").and_then(Value::as_f64).unwrap_or_default();
        if let Some(id) = details.pointer("/uploader/id").filter(|id| !id.is_null()) {
            mappers.insert(id.to_string());
        }
        for nps in details.get("difficulties").and_then(Value::as_array).into_iter().flatten()
            .filter_map(|difficulty| difficulty.get("nps").and_then(Value::as_f64)) {
            min_nps = min_nps.min(nps);
            max_nps = max_nps.max(nps);
        }
        if let Some(song) = song.as_object_mut() {
            song.insert("songDetails".into(), details);
        }
    }
    let sync_url = playlist.pointer("/customData/syncURL").and_then(Value::as_str).unwrap_or_default();
    let id = sync_url.split("/id/").nth(1).and_then(|value| value.split('/').next())
        .filter(|value| !value.is_empty() && value.chars().all(|character| character.is_ascii_digit()))
        .and_then(|value| value.parse::<u64>().ok());
    let mut details = playlist.as_object().cloned().unwrap_or_default();
    details.insert("path".into(), json!(path));
    details.insert("nbMaps".into(), json!(songs.len()));
    details.insert("songs".into(), json!(songs));
    details.insert("nbMappers".into(), json!(mappers.len()));
    details.insert("duration".into(), json!(duration));
    details.insert("minNps".into(), json!(if min_nps.is_finite() { min_nps } else { 0.0 }));
    details.insert("maxNps".into(), json!(if max_nps.is_finite() { max_nps } else { 0.0 }));
    if let Some(id) = id { details.insert("id".into(), json!(id)); }
    Value::Object(details)
}

fn playlist_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    collect_playlist_files(root, &mut paths)?;
    Ok(paths)
}

fn collect_playlist_files(directory: &Path, paths: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() { continue; }
        let path = entry.path();
        if kind.is_dir() {
            collect_playlist_files(&path, paths)?;
        } else if kind.is_file() && accept_playlist_file(&path) {
            paths.push(path);
        }
    }
    Ok(())
}

fn collect_export_files(root: &Path, directory: &Path, prefix: &Path, files: &mut Vec<(PathBuf, String)>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() { return Err(io::Error::new(io::ErrorKind::InvalidData, "Map contains a symbolic link")); }
        let path = entry.path();
        if kind.is_dir() {
            collect_export_files(root, &path, prefix, files)?;
        } else if kind.is_file() {
            let relative = path.strip_prefix(root).map_err(io::Error::other)?;
            let name = prefix.join(relative).to_string_lossy().replace('\\', "/");
            files.push((path, name));
        }
    }
    Ok(())
}

fn write_playlist_archive(
    output: &Path,
    files: Vec<(PathBuf, String)>,
    cancelled: Arc<AtomicBool>,
    progress: tokio::sync::mpsc::UnboundedSender<u64>,
) -> Result<(), String> {
    let result = (|| {
        let mut archive = ZipWriter::new(File::create(output).map_err(|error| error.to_string())?);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (index, (path, name)) in files.into_iter().enumerate() {
            if cancelled.load(Ordering::Relaxed) { return Err("Playlist export was cancelled".to_owned()); }
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

fn emit_list_progress<R: Runtime>(window: &WebviewWindow<R>, channel: Option<&str>, current: u64, total: u64, data: Value) -> Result<(), SerializedError> {
    if let Some(channel) = channel {
        crate::emit_response_value(window, Some(channel), json!({ "current": current, "total": total, "data": data }))?;
    }
    Ok(())
}

fn emit_import_progress<R: Runtime>(window: &WebviewWindow<R>, channel: Option<&str>, current: u64, total: u64, data: Option<Value>, last_error: Option<Value>) -> Result<(), SerializedError> {
    if let Some(channel) = channel {
        let mut value = json!({ "current": current, "total": total });
        if let Some(object) = value.as_object_mut() {
            if let Some(data) = data { object.insert("data".into(), data); }
            if let Some(last_error) = last_error { object.insert("lastError".into(), last_error); }
        }
        crate::emit_response_value(window, Some(channel), value)?;
    }
    Ok(())
}

fn emit_delete_progress<R: Runtime>(window: &WebviewWindow<R>, channel: Option<&str>, current: u64, total: u64) -> Result<(), SerializedError> {
    if let Some(channel) = channel {
        crate::emit_response_value(window, Some(channel), json!({ "current": current, "total": total }))?;
    }
    Ok(())
}

fn emit_archive_progress<R: Runtime>(window: &WebviewWindow<R>, channel: Option<&str>, current: u64, total: u64, output: &str) -> Result<(), SerializedError> {
    if let Some(channel) = channel {
        crate::emit_response_value(window, Some(channel), json!({ "current": current, "total": total, "data": output }))?;
    }
    Ok(())
}

fn emit_download_progress<R: Runtime>(window: &WebviewWindow<R>, channel: Option<&str>, current: u64, total: u64, data: Value) -> Result<(), SerializedError> {
    if let Some(channel) = channel {
        crate::emit_response_value(window, Some(channel), json!({ "current": current, "total": total, "data": data }))?;
    }
    Ok(())
}

fn accept_playlist_file(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str())
        .is_some_and(|extension| PLAYLIST_EXTENSIONS.iter().any(|accepted| extension.eq_ignore_ascii_case(accepted)))
}

fn unique_path(path: PathBuf) -> PathBuf {
    if !path.exists() { return path; }
    let parent = path.parent().unwrap_or(Path::new("."));
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let extension = path.extension().and_then(|extension| extension.to_str()).unwrap_or("bplist");
    for index in 1_u64.. {
        let candidate = parent.join(format!("{stem} ({index}).{extension}"));
        if !candidate.exists() { return candidate; }
    }
    unreachable!()
}

fn sanitize_filename(value: &str) -> String {
    let name = value.chars().map(|character| {
        if character.is_control() || "<>:\"/\\|?*".contains(character) { '_' } else { character }
    }).collect::<String>();
    let name = name.trim().trim_end_matches(['.', ' ']);
    if name.is_empty() { "Playlist".to_owned() } else { name.to_owned() }
}

fn find_hash(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    bytes.windows(40).find(|window| window.iter().all(u8::is_ascii_hexdigit))
        .and_then(|window| std::str::from_utf8(window).ok()).map(str::to_owned)
}
