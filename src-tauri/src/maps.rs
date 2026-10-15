use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    sync::{Arc, atomic::{AtomicBool, Ordering}},
};

use chrono::Local;
use serde_json::{Map, Value, json};
use sha1::{Digest, Sha1};
use tauri::{AppHandle, Emitter, Runtime, WebviewWindow};
use url::Url;

use crate::{SerializedError, linker, serialized_error, versions};

const LEVELS_FOLDER: &str = "Beat Saber_Data";
const CUSTOM_LEVELS_FOLDER: &str = "CustomLevels";
const SHARED_MAPS_FOLDER: &str = "SharedMaps";
const METADATA_FILE: &str = "metadata.json";

pub async fn load_version_maps<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    version: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
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
            deleted += 1;
        }
        emit_delete_progress(window, response_channel, total, deleted)?;
    }
    Ok(Value::Null)
}

fn map_folder<R: Runtime>(app: &AppHandle<R>, version: &Value) -> Result<PathBuf, SerializedError> {
    if version.is_null() {
        return Ok(linker::shared_folder(app)?.join(SHARED_MAPS_FOLDER).join(CUSTOM_LEVELS_FOLDER));
    }
    Ok(versions::version_path(app, version)?.join(LEVELS_FOLDER).join(CUSTOM_LEVELS_FOLDER))
}

fn managed_map_roots<R: Runtime>(app: &AppHandle<R>) -> Result<Vec<PathBuf>, SerializedError> {
    let mut candidates = vec![linker::shared_folder(app)?.join(SHARED_MAPS_FOLDER).join(CUSTOM_LEVELS_FOLDER)];
    let installed = versions::get_installed_versions(app)?;
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
    let entries = fs::read_dir(path).map_err(|error| error.to_string())?;
    let info_path = entries.filter_map(Result::ok).map(|entry| entry.path())
        .find(|entry| entry.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.eq_ignore_ascii_case("info.dat")));
    let Some(info_path) = info_path else { return Ok(None); };
    let raw_text = fs::read_to_string(&info_path).map_err(|error| error.to_string())?;
    let raw: Value = serde_json::from_str(&raw_text).map_err(|error| error.to_string())?;
    let map_info = parse_map_info(&raw)?;
    let hash = compute_map_hash(path, &raw_text, &map_info)?;
    let metadata = load_or_create_metadata(path)?;
    let song_filename = map_info.get("songFilename").and_then(Value::as_str)
        .ok_or_else(|| "Map song filename is missing".to_owned())?;
    let cover_filename = map_info.get("coverImageFilename").and_then(Value::as_str)
        .ok_or_else(|| "Map cover filename is missing".to_owned())?;
    let song_path = safe_child(path, song_filename)?;
    let cover_path = safe_child(path, cover_filename)?;
    let song_url = Url::from_file_path(&song_path).map_err(|_| "Could not create song file URL".to_owned())?;
    let cover_url = Url::from_file_path(&cover_path).map_err(|_| "Could not create cover file URL".to_owned())?;
    Ok(Some(json!({
        "mapInfo": map_info,
        "coverUrl": cover_url,
        "songUrl": song_url,
        "hash": hash,
        "path": path,
        "metadata": metadata,
    })))
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
        window.emit(channel, json!({ "total": total, "loaded": loaded, "maps": maps }))
            .map_err(|error| serialized_error(error.to_string(), "IPC_EMIT_FAILED"))?;
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
        window.emit(channel, json!({ "total": total, "deleted": deleted }))
            .map_err(|error| serialized_error(error.to_string(), "IPC_EMIT_FAILED"))?;
    }
    Ok(())
}
