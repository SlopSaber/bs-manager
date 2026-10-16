use std::{collections::HashSet, fs, io, path::{Component, Path, PathBuf}, sync::{Arc, atomic::{AtomicBool, Ordering}}};

use serde_json::{Map, Value, json};
use tauri::{AppHandle, Runtime, WebviewWindow};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::{library, SerializedError, serialized_error};

const INSTANCES_FOLDER: &str = "BSInstances";
const LIBRARY_CONFIG_FILE: &str = "config.cfg";
const VERSION_METADATA_FILE: &str = "metadata.config";
const CUSTOM_VERSIONS_KEY: &str = "custom-versions";

struct ImportFile {
    source: PathBuf,
    destination: PathBuf,
    size: u64,
}

pub async fn import_version<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    args: &Value,
    response_channel: Option<&str>,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let source = args.get("fromPath").and_then(Value::as_str).filter(|path| !path.trim().is_empty())
        .ok_or_else(|| serialized_error("Source folder is required", "INVALID_SOURCE"))?;
    let store = args.get("store").and_then(Value::as_str)
        .filter(|store| matches!(*store, "steam" | "oculus"))
        .ok_or_else(|| serialized_error("Store must be steam or oculus", "INVALID_STORE"))?;
    let source = super::library::canonicalize_existing_directory(Path::new(source))
        .map_err(|error| serialized_error(error, "INVALID_SOURCE"))?;
    let mut version = identify_version(&source)?;
    let base_path = version_path(app, &version)?;
    let mut destination = base_path.clone();
    let mut suffix = 0;
    while path_exists(&destination) {
        suffix += 1;
        destination = PathBuf::from(format!("{} ({suffix})", base_path.display()));
    }
    if destination != base_path {
        if let Some(object) = version.as_object_mut() {
            let name = destination.file_name().and_then(|name| name.to_str())
                .ok_or_else(|| serialized_error("Imported folder name is invalid", "INVALID_VERSION"))?;
            object.insert("name".into(), json!(name));
        }
    }

    if let Some(object) = version.as_object_mut() {
        object.insert("path".into(), json!(destination));
        object.insert("steam".into(), json!(false));
        object.insert("oculus".into(), json!(false));
        object.insert("metadata".into(), json!({ "id": "", "store": store }));
    }

    let source_for_scan = source.clone();
    let destination_for_scan = destination.clone();
    let cancellation_for_scan = cancelled.clone();
    let (files, directories, total) = tokio::task::spawn_blocking(move || {
        collect_import_files(&source_for_scan, &destination_for_scan, &cancellation_for_scan)
    }).await
        .map_err(|error| serialized_error(format!("Import scan worker failed: {error}"), "IMPORT_FAILED"))?
        .map_err(|error| {
            let code = if error.kind() == io::ErrorKind::Interrupted { "REQUEST_CANCELLED" } else { "IMPORT_FAILED" };
            serialized_error(error.to_string(), code)
        })?;

    let copy_result = async {
        emit_import_progress(window, response_channel, 0, total, &version)?;
        for directory in directories {
            tokio::fs::create_dir_all(directory).await
                .map_err(|error| serialized_error(error.to_string(), "IMPORT_FAILED"))?;
        }
        let mut current = 0_u64;
        for file in files {
            if cancelled.load(Ordering::Relaxed) {
                return Err(serialized_error("Import was cancelled", "REQUEST_CANCELLED"));
            }
            if let Some(parent) = file.destination.parent() {
                tokio::fs::create_dir_all(parent).await
                    .map_err(|error| serialized_error(error.to_string(), "IMPORT_FAILED"))?;
            }
            let mut input = tokio::fs::File::open(&file.source).await
                .map_err(|error| serialized_error(error.to_string(), "IMPORT_FAILED"))?;
            let mut output = tokio::fs::File::create(&file.destination).await
                .map_err(|error| serialized_error(error.to_string(), "IMPORT_FAILED"))?;
            let mut buffer = vec![0_u8; 2 * 1024 * 1024];
            loop {
                if cancelled.load(Ordering::Relaxed) {
                    return Err(serialized_error("Import was cancelled", "REQUEST_CANCELLED"));
                }
                let read = input.read(&mut buffer).await
                    .map_err(|error| serialized_error(error.to_string(), "IMPORT_FAILED"))?;
                if read == 0 {
                    break;
                }
                output.write_all(&buffer[..read]).await
                    .map_err(|error| serialized_error(error.to_string(), "IMPORT_FAILED"))?;
                current = current.saturating_add(read as u64).min(total);
                emit_import_progress(window, response_channel, current, total, &version)?;
            }
            if file.size == 0 {
                emit_import_progress(window, response_channel, current, total, &version)?;
            }
        }
        if cancelled.load(Ordering::Relaxed) {
            return Err(serialized_error("Import was cancelled", "REQUEST_CANCELLED"));
        }
        Ok::<u64, SerializedError>(current)
    }.await;
    let current = match copy_result {
        Ok(current) => current,
        Err(error) => {
            cleanup_import(&destination).await;
            return Err(error);
        }
    };

    let metadata = json!({ "id": Uuid::new_v4().to_string(), "store": store });
    let metadata_path = destination.join(VERSION_METADATA_FILE);
    if let Err(error) = tokio::fs::write(&metadata_path, serde_json::to_vec_pretty(&metadata).unwrap_or_default()).await {
        log::warn!("Unable to save imported version metadata {}: {error}", metadata_path.display());
    }
    if let Some(object) = version.as_object_mut() {
        object.insert("metadata".into(), metadata);
    }
    emit_import_progress(window, response_channel, current.max(total), total, &version)?;
    Ok(Value::Null)
}

fn identify_version(source: &Path) -> Result<Value, SerializedError> {
    identify_version_with_store(source, None)
}

fn identify_version_with_store(source: &Path, store: Option<&str>) -> Result<Value, SerializedError> {
    if !source.join("Beat Saber.exe").is_file() {
        return Err(serialized_error("Selected folder is not a Beat Saber installation", "NOT_BS_FOLDER"));
    }
    let version_text_path = source.join("Beat Saber_Data").join("globalgamemanagers");
    let version_text = fs::read(&version_text_path)
        .map_err(|error| serialized_error(error.to_string(), "NOT_BS_FOLDER"))?;
    let version_text = String::from_utf8_lossy(&version_text);
    let catalog = serde_json::from_str::<Vec<Value>>(super::VERSION_LIBRARY)
        .map_err(|error| serialized_error(error.to_string(), "VERSION_LIBRARY_READ_FAILED"))?;
    let mut version = catalog.iter().rev().find(|candidate| {
        candidate.get("BSVersion").and_then(Value::as_str).is_some_and(|value| version_text.contains(value))
    }).cloned().ok_or_else(|| serialized_error("Unable to detect Beat Saber version", "NOT_BS_FOLDER"))?;
    let object = version.as_object_mut().ok_or_else(|| serialized_error("Version catalog entry is invalid", "VERSION_LIBRARY_READ_FAILED"))?;
    let folder_name = source.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    let game_version = object.get("BSVersion").and_then(Value::as_str).unwrap_or_default();
    if store.is_none() && folder_name != game_version {
        object.insert("name".into(), json!(folder_name));
    }
    object.insert("path".into(), json!(source));
    object.insert("steam".into(), json!(store == Some("steam")));
    object.insert("oculus".into(), json!(store == Some("oculus")));
    Ok(version)
}

fn collect_import_files(source: &Path, destination: &Path, cancelled: &AtomicBool) -> io::Result<(Vec<ImportFile>, Vec<PathBuf>, u64)> {
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let mut total = 0_u64;
    let mut ancestors = HashSet::new();
    collect_import_entry(source, destination, cancelled, &mut ancestors, &mut files, &mut directories, &mut total)?;
    Ok((files, directories, total))
}

fn collect_import_entry(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
    ancestors: &mut HashSet<PathBuf>,
    files: &mut Vec<ImportFile>,
    directories: &mut Vec<PathBuf>,
    total: &mut u64,
) -> io::Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "Import was cancelled"));
    }
    let metadata = fs::metadata(source)?;
    if metadata.is_dir() {
        let identity = fs::canonicalize(source)?;
        if !ancestors.insert(identity.clone()) {
            return Ok(());
        }
        directories.push(destination.to_path_buf());
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            collect_import_entry(
                &entry.path(),
                &destination.join(entry.file_name()),
                cancelled,
                ancestors,
                files,
                directories,
                total,
            )?;
        }
        ancestors.remove(&identity);
    } else if metadata.is_file() {
        let size = metadata.len();
        *total = total.saturating_add(size);
        files.push(ImportFile { source: source.to_path_buf(), destination: destination.to_path_buf(), size });
    }
    Ok(())
}

fn emit_import_progress<R: Runtime>(
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    current: u64,
    total: u64,
    version: &Value,
) -> Result<(), SerializedError> {
    if let Some(channel) = response_channel {
        crate::emit_response_value(window, Some(channel), json!({ "current": current, "total": total, "data": version }))?;
    }
    Ok(())
}

async fn cleanup_import(path: &Path) {
    let path = path.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || super::library::remove_library(&path)).await;
}

pub fn get_installed_versions<R: Runtime>(app: &AppHandle<R>) -> Result<Value, SerializedError> {
    let library_root = super::install_directory(app)
        .map_err(|error| serialized_error(error, "INSTALL_PATH_FAILED"))?;
    let version_root = library_root.join(INSTANCES_FOLDER);
    let config_path = library_root.join(LIBRARY_CONFIG_FILE);
    let mut config = read_config(&config_path)
        .map_err(|error| serialized_error(error.to_string(), "VERSION_CONFIG_READ_FAILED"))?;
    let custom_versions = config.get(CUSTOM_VERSIONS_KEY).and_then(Value::as_array).cloned().unwrap_or_default();
    let catalog = serde_json::from_str::<Vec<Value>>(super::VERSION_LIBRARY)
        .map_err(|error| serialized_error(error.to_string(), "VERSION_LIBRARY_READ_FAILED"))?;

    let mut installed = Vec::new();
    for (store, path) in [
        ("steam", super::stores::steam_game_folder()),
        ("oculus", super::stores::oculus_game_folder()),
    ] {
        if let Some(path) = path {
            if let Ok(version) = identify_version_with_store(&path, Some(store)) {
                installed.push(version);
            }
        }
    }

    let entries = match fs::read_dir(&version_root) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(serialized_error(error.to_string(), "INSTALL_PATH_FAILED")),
    };
    let Some(entries) = entries else {
        return Ok(json!(installed));
    };

    for entry in entries {
        let entry = entry.map_err(|error| serialized_error(error.to_string(), "INSTALL_PATH_FAILED"))?;
        let path = entry.path();
        if !path.join("Beat Saber.exe").is_file() {
            continue;
        }
        let version_text_path = path.join("Beat Saber_Data").join("globalgamemanagers");
        let version_text = match fs::read(&version_text_path) {
            Ok(contents) => String::from_utf8_lossy(&contents).into_owned(),
            Err(_) => continue,
        };
        let Some(mut version) = catalog.iter().rev().find(|candidate| {
            candidate.get("BSVersion").and_then(Value::as_str).is_some_and(|value| version_text.contains(value))
        }).cloned() else {
            continue;
        };
        let Some(object) = version.as_object_mut() else {
            continue;
        };

        let folder_name = entry.file_name().to_string_lossy().to_string();
        let game_version = object.get("BSVersion").and_then(Value::as_str).unwrap_or_default().to_owned();
        if folder_name != game_version {
            object.insert("name".into(), json!(folder_name));
        }
        object.insert("path".into(), json!(path));
        object.insert("steam".into(), json!(false));
        object.insert("oculus".into(), json!(false));

        let metadata = read_or_create_metadata(&path);
        object.insert("metadata".into(), metadata);

        let current_name = object.get("name").cloned();
        if let Some(custom) = custom_versions.iter().find(|custom| {
            custom.get("BSVersion") == object.get("BSVersion") && custom.get("name").cloned() == current_name
        }) {
            if let Some(color) = custom.get("color") {
                object.insert("color".into(), color.clone());
            }
        }
        installed.push(version);
    }

    let colored_versions = installed.iter().filter(|version| {
        version.get("color").and_then(Value::as_str).is_some_and(|color| !color.is_empty())
    }).cloned().collect::<Vec<_>>();
    config.insert(CUSTOM_VERSIONS_KEY.to_owned(), json!(colored_versions));
    write_config(&config_path, &config)
        .map_err(|error| serialized_error(error.to_string(), "VERSION_CONFIG_WRITE_FAILED"))?;

    Ok(json!(installed))
}

pub fn version_path<R: Runtime>(app: &AppHandle<R>, version: &Value) -> Result<PathBuf, SerializedError> {
    if version.get("steam").and_then(Value::as_bool).unwrap_or(false) {
        return super::stores::steam_game_folder()
            .ok_or_else(|| serialized_error("Beat Saber Steam installation not found", "VersionNotFound"));
    }
    if version.get("oculus").and_then(Value::as_bool).unwrap_or(false) {
        return super::stores::oculus_game_folder()
            .ok_or_else(|| serialized_error("Beat Saber Meta installation not found", "VersionNotFound"));
    }
    let folder = version.get("name").and_then(Value::as_str).filter(|name| !name.is_empty())
        .or_else(|| version.get("BSVersion").and_then(Value::as_str))
        .ok_or_else(|| serialized_error("Version folder name is required", "INVALID_VERSION"))?;
    if !is_safe_folder_name(folder) {
        return Err(serialized_error("Version folder name is invalid", "INVALID_VERSION"));
    }
    let root = super::install_directory(app)
        .map_err(|error| serialized_error(error, "INSTALL_PATH_FAILED"))?;
    Ok(root.join(INSTANCES_FOLDER).join(folder))
}

pub fn edit_version<R: Runtime>(app: &AppHandle<R>, args: &Value) -> Result<Value, SerializedError> {
    let version = args.get("version").ok_or_else(|| serialized_error("Version is required", "INVALID_VERSION"))?;
    if is_official_version(version) {
        return Err(serialized_error("Do not edit official Beat Saber versions", "CantEditSteam"));
    }
    let requested_name = args.get("name").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Version name is required", "INVALID_VERSION"))?;
    let color = args.get("color").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Version color is required", "INVALID_VERSION"))?;
    let edited = edited_version(version, requested_name, color)?;
    let old_path = version_path(app, version)?;
    let new_path = version_path(app, &edited)?;
    let root = super::install_directory(app)
        .map_err(|error| serialized_error(error, "INSTALL_PATH_FAILED"))?;

    if old_path != new_path {
        if path_exists(&new_path) {
            return Err(serialized_error("Unable to edit the version, path already exists", "VersionAlreadExist"));
        }
        fs::rename(&old_path, &new_path)
            .map_err(|error| serialized_error(error.to_string(), "CantRename"))?;
    }

    if let Err(error) = upsert_custom_version(&root, version, &edited) {
        if old_path != new_path {
            let _ = fs::rename(&new_path, &old_path);
        }
        return Err(serialized_error(error.to_string(), "VERSION_CONFIG_WRITE_FAILED"));
    }
    let _ = update_last_launched_version(app, version, &edited);
    Ok(edited)
}

pub async fn clone_version<R: Runtime>(app: &AppHandle<R>, args: &Value, cancelled: Arc<AtomicBool>) -> Result<Value, SerializedError> {
    let version = args.get("version").ok_or_else(|| serialized_error("Version is required", "INVALID_VERSION"))?;
    let requested_name = args.get("name").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Version name is required", "INVALID_VERSION"))?;
    let color = args.get("color").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Version color is required", "INVALID_VERSION"))?;
    let mut cloned = edited_version(version, requested_name, color)?;
    if let Some(object) = cloned.as_object_mut() {
        object.insert("steam".into(), json!(false));
        object.insert("oculus".into(), json!(false));
    }

    let origin = version_path(app, version)?;
    if !origin.is_dir() {
        return Err(serialized_error("Version not found", "VersionNotFound"));
    }
    let destination = version_path(app, &cloned)?;
    if path_exists(&destination) {
        return Err(serialized_error("Unable to clone the version, path already exists", "VersionAlreadExist"));
    }

    let source_for_copy = origin.clone();
    let destination_for_copy = destination.clone();
    let cancelled_for_copy = cancelled.clone();
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = destination_for_copy.parent() {
            fs::create_dir_all(parent).map_err(library::LibraryError::Io)?;
        }
        library::copy_library(&source_for_copy, &destination_for_copy, &cancelled_for_copy)
    })
        .await
        .map_err(|error| serialized_error(format!("Version copy worker failed: {error}"), "CantClone"))?
        .map_err(|error| match error {
            library::LibraryError::CopyToSubpath => serialized_error("Cannot clone a version into itself", "COPY_TO_SUBPATH"),
            library::LibraryError::Io(error) if error.kind() == io::ErrorKind::Interrupted => {
                serialized_error("Version copy was cancelled", "REQUEST_CANCELLED")
            }
            library::LibraryError::Io(error) => serialized_error(error.to_string(), "CantClone"),
        })?;

    if cancelled.load(Ordering::Relaxed) {
        let destination_for_cleanup = destination.clone();
        let _ = tokio::task::spawn_blocking(move || library::remove_library(&destination_for_cleanup)).await;
        return Err(serialized_error("Version copy was cancelled", "REQUEST_CANCELLED"));
    }

    let root = super::install_directory(app)
        .map_err(|error| serialized_error(error, "INSTALL_PATH_FAILED"))?;
    upsert_custom_version(&root, version, &cloned)
        .map_err(|error| serialized_error(error.to_string(), "VERSION_CONFIG_WRITE_FAILED"))?;
    Ok(cloned)
}

pub async fn uninstall_version<R: Runtime>(app: &AppHandle<R>, version: &Value) -> Result<Value, SerializedError> {
    if is_official_version(version) {
        return Err(serialized_error("BSManager cannot delete official Beat Saber versions", "CantDeleteOfficialVersion"));
    }
    let path = version_path(app, version)?;
    if !path_exists(&path) {
        return Err(serialized_error("Version not found", "VersionNotFound"));
    }
    let path_for_delete = path.clone();
    tokio::task::spawn_blocking(move || library::remove_library(&path_for_delete))
        .await
        .map_err(|error| serialized_error(format!("Version delete worker failed: {error}"), "VERSION_DELETE_FAILED"))?
        .map_err(|error| serialized_error(error.to_string(), "VERSION_DELETE_FAILED"))?;

    let root = super::install_directory(app)
        .map_err(|error| serialized_error(error, "INSTALL_PATH_FAILED"))?;
    remove_custom_version(&root, version)
        .map_err(|error| serialized_error(error.to_string(), "VERSION_CONFIG_WRITE_FAILED"))?;
    Ok(Value::Null)
}

pub fn relative_path(from: &Path, to: &Path) -> PathBuf {
    let from_components = from.components().collect::<Vec<_>>();
    let to_components = to.components().collect::<Vec<_>>();
    if from_components.first().zip(to_components.first()).is_some_and(|(left, right)| !same_component(left, right)) {
        return to.to_path_buf();
    }
    let common = from_components.iter().zip(to_components.iter()).take_while(|(left, right)| same_component(left, right)).count();
    let mut relative = PathBuf::new();
    for component in from_components.iter().skip(common) {
        if matches!(component, Component::Normal(_)) {
            relative.push("..");
        }
    }
    for component in to_components.iter().skip(common) {
        if let Component::Normal(name) = component {
            relative.push(name);
        }
    }
    relative
}

fn is_official_version(version: &Value) -> bool {
    version.get("steam").and_then(Value::as_bool).unwrap_or(false)
        || version.get("oculus").and_then(Value::as_bool).unwrap_or(false)
}

fn edited_version(version: &Value, requested_name: &str, color: &str) -> Result<Value, SerializedError> {
    let game_version = version.get("BSVersion").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Game version is required", "INVALID_VERSION"))?;
    let name = sanitize_folder_name(requested_name);
    if name.is_empty() {
        return Err(serialized_error("Version name is invalid", "INVALID_VERSION"));
    }
    let mut edited = version.clone();
    let object = edited.as_object_mut().ok_or_else(|| serialized_error("Version data is invalid", "INVALID_VERSION"))?;
    if name == game_version {
        object.remove("name");
    } else {
        object.insert("name".into(), json!(name));
    }
    object.insert("color".into(), json!(color));
    Ok(edited)
}

fn sanitize_folder_name(name: &str) -> String {
    let mut sanitized = name.chars()
        .filter(|character| !character.is_control() && !matches!(character, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'))
        .collect::<String>();
    sanitized = sanitized.trim().trim_end_matches(|character| character == '.' || character == ' ').to_owned();
    let device_name = sanitized.split('.').next().unwrap_or_default().to_ascii_uppercase();
    let reserved = matches!(device_name.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (device_name.len() == 4 && (device_name.starts_with("COM") || device_name.starts_with("LPT"))
            && device_name.as_bytes()[3].is_ascii_digit()
            && device_name.as_bytes()[3] != b'0');
    if reserved {
        sanitized.insert(0, '_');
    }
    sanitized
}

fn is_safe_folder_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name == sanitize_folder_name(name)
        && !name.chars().any(|character| matches!(character, '/' | '\\' | ':'))
}

fn same_component(left: &Component<'_>, right: &Component<'_>) -> bool {
    left.as_os_str().to_string_lossy().eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

fn read_config(path: &Path) -> io::Result<Map<String, Value>> {
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(error),
    }
}

fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn write_config(path: &Path, config: &Map<String, Value>) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Library configuration path is invalid"))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("cfg.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(config).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?)?;
    fs::rename(temporary, path)
}

fn read_or_create_metadata(version_path: &Path) -> Value {
    let metadata_path = version_path.join(VERSION_METADATA_FILE);
    let mut metadata = fs::read(&metadata_path).ok()
        .and_then(|contents| serde_json::from_slice::<Value>(&contents).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let object = metadata.as_object_mut().expect("metadata object checked");
    if object.get("id").and_then(Value::as_str).is_none_or(str::is_empty) {
        object.insert("id".into(), json!(Uuid::new_v4().to_string()));
        object.entry(String::from("store")).or_insert_with(|| json!("steam"));
        if let Err(error) = fs::write(&metadata_path, serde_json::to_vec_pretty(&metadata).unwrap_or_default()) {
            log::warn!("Unable to save version metadata {}: {error}", metadata_path.display());
        }
    }
    metadata
}

fn upsert_custom_version(root: &Path, previous: &Value, updated: &Value) -> io::Result<()> {
    let config_path = root.join(LIBRARY_CONFIG_FILE);
    let mut config = read_config(&config_path)?;
    let mut versions = config.get(CUSTOM_VERSIONS_KEY).and_then(Value::as_array).cloned().unwrap_or_default();
    versions.retain(|version| !same_version_identity(version, previous));
    versions.push(updated.clone());
    config.insert(CUSTOM_VERSIONS_KEY.to_owned(), Value::Array(versions));
    write_config(&config_path, &config)
}

fn remove_custom_version(root: &Path, removed: &Value) -> io::Result<()> {
    let config_path = root.join(LIBRARY_CONFIG_FILE);
    let mut config = read_config(&config_path)?;
    let mut versions = config.get(CUSTOM_VERSIONS_KEY).and_then(Value::as_array).cloned().unwrap_or_default();
    versions.retain(|version| !same_version_identity(version, removed));
    config.insert(CUSTOM_VERSIONS_KEY.to_owned(), Value::Array(versions));
    write_config(&config_path, &config)
}

fn same_version_identity(left: &Value, right: &Value) -> bool {
    left.get("BSVersion") == right.get("BSVersion") && left.get("name") == right.get("name")
}

fn update_last_launched_version<R: Runtime>(app: &AppHandle<R>, previous: &Value, updated: &Value) -> Result<(), SerializedError> {
    let mut settings = super::read_settings(app)
        .map_err(|error| serialized_error(error, "SETTINGS_READ_FAILED"))?;
    let Some(last_version) = settings.get("last-version-launched") else {
        return Ok(());
    };
    let same_launch_target = last_version.get("BSVersion") == previous.get("BSVersion")
        && last_version.get("name") == previous.get("name")
        && last_version.get("steam") == previous.get("steam")
        && last_version.get("oculus") == previous.get("oculus");
    if same_launch_target {
        settings.insert("last-version-launched".to_owned(), updated.clone());
        super::write_settings(app, &settings)
            .map_err(|error| serialized_error(error, "SETTINGS_WRITE_FAILED"))?;
    }
    Ok(())
}
