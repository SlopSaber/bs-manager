use std::{fs, io, path::{Component, Path, PathBuf}};

use serde_json::{Map, Value, json};
use tauri::{AppHandle, Runtime};
use uuid::Uuid;

use crate::{library, SerializedError, serialized_error};

const INSTANCES_FOLDER: &str = "BSInstances";
const LIBRARY_CONFIG_FILE: &str = "config.cfg";
const VERSION_METADATA_FILE: &str = "metadata.config";
const CUSTOM_VERSIONS_KEY: &str = "custom-versions";

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

    let entries = match fs::read_dir(&version_root) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(serialized_error(error.to_string(), "INSTALL_PATH_FAILED")),
    };
    let Some(entries) = entries else {
        return Ok(json!([]));
    };

    let mut installed = Vec::new();
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

pub async fn clone_version<R: Runtime>(app: &AppHandle<R>, args: &Value) -> Result<Value, SerializedError> {
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
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = destination_for_copy.parent() {
            fs::create_dir_all(parent).map_err(library::LibraryError::Io)?;
        }
        library::copy_library(&source_for_copy, &destination_for_copy)
    })
        .await
        .map_err(|error| serialized_error(format!("Version copy worker failed: {error}"), "CantClone"))?
        .map_err(|error| match error {
            library::LibraryError::CopyToSubpath => serialized_error("Cannot clone a version into itself", "COPY_TO_SUBPATH"),
            library::LibraryError::Io(error) => serialized_error(error.to_string(), "CantClone"),
        })?;

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
