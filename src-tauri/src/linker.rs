use std::{fs, io::{self, Read}, path::{Component, Path, PathBuf}};

use serde::Deserialize;
use serde_json::{Value, json};
use tauri::{AppHandle, Runtime};
use uuid::Uuid;

use crate::{SerializedError, serialized_error, versions};

const SHARED_CONTENT_FOLDER: &str = "SharedContent";
const SHARED_MAPS_FOLDER: &str = "SharedMaps";
const CUSTOM_LEVELS_FOLDER: &str = "CustomLevels";

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinkOptions {
    keep_contents: Option<bool>,
    intermediate_folder: Option<String>,
    backup: Option<bool>,
}

pub fn shared_folder<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, SerializedError> {
    Ok(crate::install_directory(app)
        .map_err(|error| serialized_error(error, "INSTALL_PATH_FAILED"))?
        .join(SHARED_CONTENT_FOLDER))
}

pub fn get_linked_folders<R: Runtime>(app: &AppHandle<R>, args: &Value) -> Result<Value, SerializedError> {
    let version = args.get("version").ok_or_else(|| serialized_error("Version is required", "INVALID_VERSION"))?;
    let version_path = versions::version_path(app, version)?;
    let ignore_broken = args.pointer("/options/ignoreSymlinkTargetError").and_then(Value::as_bool).unwrap_or(false);
    let mut linked = Vec::new();
    for parent in [&version_path, &version_path.join("Beat Saber_Data")] {
        for path in directory_entries(parent, ignore_broken)? {
            if is_link(&path) {
                linked.push(path);
            }
        }
    }
    if args.pointer("/options/relative").and_then(Value::as_bool).unwrap_or(false) {
        Ok(json!(linked.iter().map(|path| versions::relative_path(&version_path, path).to_string_lossy().to_string()).collect::<Vec<_>>()))
    } else {
        Ok(json!(linked))
    }
}

pub fn is_version_folder_linked<R: Runtime>(app: &AppHandle<R>, args: &Value) -> Result<Value, SerializedError> {
    let version = args.get("version").ok_or_else(|| serialized_error("Version is required", "INVALID_VERSION"))?;
    let relative = args.get("relativeFolder").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Relative folder is required", "INVALID_PATH"))?;
    let folder = version_folder_path(app, version, relative)?;
    Ok(json!(is_link(&folder)))
}

pub fn do_action<R: Runtime>(app: &AppHandle<R>, args: &Value) -> Result<Value, SerializedError> {
    let action = args.get("type").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Link action type is required", "INVALID_LINK_ACTION"))?;
    let version = args.get("version").ok_or_else(|| serialized_error("Version is required", "INVALID_VERSION"))?;
    let relative = args.get("relativeFolder").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Relative folder is required", "INVALID_PATH"))?;
    let folder = version_folder_path(app, version, relative)?;
    let options = read_options(args);
    let intermediate = if relative.contains(CUSTOM_LEVELS_FOLDER) {
        Some(SHARED_MAPS_FOLDER.to_owned())
    } else {
        options.intermediate_folder.clone()
    };
    let backup = options.backup.unwrap_or(false) || relative.contains("UserData");

    match action {
        "link" => link_folder(app, &folder, intermediate.as_deref(), backup, options.keep_contents.unwrap_or(true))?,
        "unlink" => {
            let versions = crate::versions::get_installed_versions(app, false)?;
            let move_contents = !other_version_has_link(app, &versions, relative, &folder)?;
            unlink_folder(app, &folder, intermediate.as_deref(), backup, move_contents, options.keep_contents.unwrap_or(true))?;
        }
        _ => return Err(serialized_error("Link action type is invalid", "INVALID_LINK_ACTION")),
    }
    Ok(Value::Null)
}

pub fn relink_all<R: Runtime>(app: &AppHandle<R>) -> Result<Value, SerializedError> {
    let versions = crate::versions::get_installed_versions(app, false)?;
    for version in versions.as_array().into_iter().flatten() {
        let args = json!({ "version": version, "options": { "relative": true, "ignoreSymlinkTargetError": true } });
        let linked = get_linked_folders(app, &args)?;
        for relative in linked.as_array().into_iter().flatten().filter_map(Value::as_str) {
            let action = json!({ "type": "link", "version": version, "relativeFolder": relative });
            do_action(app, &action)?;
        }
    }
    Ok(Value::Null)
}

fn read_options(args: &Value) -> LinkOptions {
    args.get("options").cloned().and_then(|value| serde_json::from_value(value).ok()).unwrap_or_default()
}

fn version_folder_path<R: Runtime>(app: &AppHandle<R>, version: &Value, relative: &str) -> Result<PathBuf, SerializedError> {
    let relative = Path::new(relative);
    if relative.as_os_str().is_empty() || relative.components().any(|component| !matches!(component, Component::Normal(_))) {
        return Err(serialized_error("Relative folder path is invalid", "INVALID_PATH"));
    }
    Ok(versions::version_path(app, version)?.join(relative))
}

fn shared_folder_path<R: Runtime>(app: &AppHandle<R>, folder: &Path, intermediate: Option<&str>) -> Result<PathBuf, SerializedError> {
    let name = folder.file_name().ok_or_else(|| serialized_error("Folder name is invalid", "INVALID_PATH"))?;
    let mut shared = shared_folder(app)?;
    if let Some(intermediate) = intermediate {
        let intermediate = Path::new(intermediate);
        if intermediate.as_os_str().is_empty() || intermediate.components().any(|component| !matches!(component, Component::Normal(_))) {
            return Err(serialized_error("Shared folder path is invalid", "INVALID_PATH"));
        }
        shared.push(intermediate);
    }
    Ok(shared.join(name))
}

fn other_version_has_link<R: Runtime>(app: &AppHandle<R>, versions: &Value, relative: &str, ignored: &Path) -> Result<bool, SerializedError> {
    for version in versions.as_array().into_iter().flatten() {
        let path = version_folder_path(app, version, relative)?;
        if !same_path(&path, ignored) && is_link(&path) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn link_folder<R: Runtime>(app: &AppHandle<R>, folder: &Path, intermediate: Option<&str>, backup: bool, keep_contents: bool) -> Result<(), SerializedError> {
    let shared = shared_folder_path(app, folder, intermediate)?;
    fs::create_dir_all(&shared).map_err(|error| link_error(error, "LINK_FAILED"))?;

    if is_link(folder) {
        if link_targets(folder, &shared) {
            return Ok(());
        }
        let staged_link = temporary_path(folder, "link");
        create_link(app, &shared, &staged_link)?;
        remove_link(folder).map_err(|error| link_error(error, "LINK_FAILED"))?;
        if let Err(error) = fs::rename(&staged_link, folder) {
            let _ = remove_link(&staged_link);
            return Err(link_error(error, "LINK_FAILED"));
        }
        return Ok(());
    }

    if backup && path_exists(folder) {
        let backup_path = backup_path(folder);
        copy_directory(folder, &backup_path).map_err(|error| link_error(error, "LINK_BACKUP_FAILED"))?;
    }
    let contents_already_shared = keep_contents && path_exists(folder)
        && match directory_contents_already_shared(folder, &shared) {
            Ok(already_shared) => already_shared,
            Err(error) => {
                log::debug!("Unable to compare shared folder contents; using merge copy: {error}");
                false
            }
        };
    if keep_contents && path_exists(folder) && !contents_already_shared {
        copy_directory(folder, &shared).map_err(|error| link_error(error, "LINK_COPY_FAILED"))?;
    }

    let staged_link = temporary_path(folder, "link");
    create_link(app, &shared, &staged_link)?;
    if path_exists(folder) {
        if let Err(error) = crate::library::remove_library(folder) {
            let _ = remove_link(&staged_link);
            return Err(link_error(error, "LINK_FAILED"));
        }
    }
    if let Err(error) = fs::rename(&staged_link, folder) {
        let _ = remove_link(&staged_link);
        if keep_contents {
            let _ = copy_directory(&shared, folder);
        }
        return Err(link_error(error, "LINK_FAILED"));
    }
    Ok(())
}

fn unlink_folder<R: Runtime>(app: &AppHandle<R>, folder: &Path, intermediate: Option<&str>, backup_enabled: bool, move_contents: bool, keep_contents: bool) -> Result<(), SerializedError> {
    if !is_link(folder) {
        return Ok(());
    }
    let shared = shared_folder_path(app, folder, intermediate)?;
    let backup = backup_path(folder);
    let restore_backup = backup_enabled && path_exists(&backup);
    let should_copy_shared = !restore_backup && (move_contents || keep_contents);
    let staged_folder = temporary_path(folder, "unlinked");
    fs::create_dir_all(&staged_folder).map_err(|error| link_error(error, "UNLINK_FAILED"))?;

    let copy_result = if restore_backup {
        copy_directory(&backup, &staged_folder).map_err(|error| link_error(error, "UNLINK_RESTORE_FAILED"))
    } else if should_copy_shared && path_exists(&shared) {
        copy_directory(&shared, &staged_folder).map_err(|error| link_error(error, "UNLINK_COPY_FAILED"))
    } else {
        Ok(())
    };
    if let Err(error) = copy_result {
        let _ = remove_directory(&staged_folder);
        return Err(error);
    }

    if let Err(error) = remove_link(folder) {
        let _ = remove_directory(&staged_folder);
        return Err(link_error(error, "UNLINK_FAILED"));
    }
    if let Err(error) = fs::rename(&staged_folder, folder) {
        let _ = remove_directory(&staged_folder);
        let _ = create_link(app, &shared, folder);
        return Err(link_error(error, "UNLINK_FAILED"));
    }

    if restore_backup {
        remove_directory(&backup).map_err(|error| link_error(error, "UNLINK_RESTORE_FAILED"))?;
    } else if move_contents && path_exists(&shared) {
        remove_directory(&shared).map_err(|error| link_error(error, "UNLINK_CLEANUP_FAILED"))?;
    }
    Ok(())
}

fn create_link<R: Runtime>(app: &AppHandle<R>, target: &Path, link: &Path) -> Result<(), SerializedError> {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).map_err(|error| link_error(error, "LINK_FAILED"))?;
    }
    let use_symlink = crate::read_settings(app).map_err(|error| serialized_error(error, "SETTINGS_READ_FAILED"))?
        .get("use-symlinks").and_then(Value::as_bool).unwrap_or(false);
    #[cfg(windows)]
    let result = if use_symlink {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        junction::create(target, link)
    };
    #[cfg(not(windows))]
    let result = {
        let _ = use_symlink;
        Err(io::Error::new(io::ErrorKind::Unsupported, "Folder linking is available only on Windows"))
    };
    result.map_err(|error| link_error(error, "LINKING_NOT_SUPPORTED"))
}

fn copy_directory(source: &Path, destination: &Path) -> io::Result<()> {
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    crate::library::copy_library(source, destination, &cancelled).map(|_| ()).map_err(|error| match error {
        crate::library::LibraryError::CopyToSubpath => io::Error::new(io::ErrorKind::InvalidInput, "Cannot copy a folder into itself"),
        crate::library::LibraryError::Io(error) => error,
    })
}

fn directory_contents_already_shared(source: &Path, destination: &Path) -> io::Result<bool> {
    if is_link(source) || !fs::symlink_metadata(source)?.is_dir() {
        return Ok(false);
    }

    let is_custom_levels = source.file_name().is_some_and(|name| {
        name.to_string_lossy().eq_ignore_ascii_case(CUSTOM_LEVELS_FOLDER)
    });
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        let source_entry = entry.path();
        if !path_exists(&target) {
            return Ok(false);
        }
        let already_shared = if is_custom_levels && is_builtin_map_directory(&source_entry) {
            builtin_map_contents_match(&source_entry, &target)?
        } else {
            source_path_is_already_shared(&source_entry, &target)?
        };
        if !already_shared {
            return Ok(false);
        }
    }
    Ok(true)
}

fn is_builtin_map_directory(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        name.to_string_lossy().to_ascii_lowercase().ends_with(" (built in)")
    })
}

fn builtin_map_contents_match(source: &Path, destination: &Path) -> io::Result<bool> {
    let source_metadata = fs::symlink_metadata(source)?;
    let destination_metadata = fs::symlink_metadata(destination)?;
    if is_link(source) || is_link(destination) {
        return Ok(false);
    }

    if source_metadata.is_dir() || destination_metadata.is_dir() {
        if !source_metadata.is_dir() || !destination_metadata.is_dir() {
            return Ok(false);
        }
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let target = destination.join(entry.file_name());
            if !path_exists(&target) || !builtin_map_contents_match(&entry.path(), &target)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }

    if !source_metadata.is_file() || !destination_metadata.is_file()
        || source_metadata.len() != destination_metadata.len()
        || source_metadata.permissions().readonly() != destination_metadata.permissions().readonly()
    {
        return Ok(false);
    }

    let is_audio = source.extension().is_some_and(|extension| {
        let extension = extension.to_string_lossy();
        ["ogg", "wav", "mp3", "flac", "m4a"]
            .iter()
            .any(|audio| extension.eq_ignore_ascii_case(audio))
    });
    if is_audio {
        return Ok(true);
    }
    source_path_is_already_shared(source, destination)
}

fn source_path_is_already_shared(left: &Path, right: &Path) -> io::Result<bool> {
    let left_metadata = fs::symlink_metadata(left)?;
    let right_metadata = fs::symlink_metadata(right)?;
    if is_link(left) || is_link(right) {
        return Ok(false);
    }

    if left_metadata.is_dir() || right_metadata.is_dir() {
        if !left_metadata.is_dir() || !right_metadata.is_dir() {
            return Ok(false);
        }

        let left_entries = fs::read_dir(left)?.collect::<Result<Vec<_>, _>>()?;
        let right_entries = fs::read_dir(right)?.collect::<Result<Vec<_>, _>>()?;
        if left_entries.len() > right_entries.len() {
            return Ok(false);
        }

        for left_entry in left_entries {
            let left_name = left_entry.file_name();
            let Some(right_entry) = right_entries.iter().find(|entry| {
                entry.file_name().to_string_lossy().eq_ignore_ascii_case(&left_name.to_string_lossy())
            }) else {
                return Ok(false);
            };
            if !source_path_is_already_shared(&left_entry.path(), &right_entry.path())? {
                return Ok(false);
            }
        }
        return Ok(true);
    }

    if !left_metadata.is_file() || !right_metadata.is_file()
        || left_metadata.len() != right_metadata.len()
        || left_metadata.permissions().readonly() != right_metadata.permissions().readonly()
    {
        return Ok(false);
    }

    let mut left_file = fs::File::open(left)?;
    let mut right_file = fs::File::open(right)?;
    let mut left_buffer = [0_u8; 64 * 1024];
    let mut right_buffer = [0_u8; 64 * 1024];
    loop {
        let left_read = left_file.read(&mut left_buffer)?;
        let right_read = right_file.read(&mut right_buffer)?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

fn directory_entries(path: &Path, ignore_broken: bool) -> Result<Vec<PathBuf>, SerializedError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(link_error(error, "LINKED_FOLDER_SCAN_FAILED")),
    };
    let mut directories = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| link_error(error, "LINKED_FOLDER_SCAN_FAILED"))?;
        let path = entry.path();
        if entry.file_type().map_err(|error| link_error(error, "LINKED_FOLDER_SCAN_FAILED"))?.is_dir() {
            directories.push(path);
        } else if is_link(&path) {
            if link_target_is_directory(&path) {
                directories.push(path);
            } else if ignore_broken && path.extension().is_none() {
                directories.push(path);
            }
        }
    }
    Ok(directories)
}

fn link_target_is_directory(link: &Path) -> bool {
    link_target(link).is_some_and(|target| fs::metadata(target).is_ok_and(|metadata| metadata.is_dir()))
}

fn link_targets(link: &Path, target: &Path) -> bool {
    link_target(link).is_some_and(|actual| same_path(&actual, target))
}

fn link_target(link: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    if junction::exists(link).ok()? {
        return junction::get_target(link).ok();
    }
    let target = fs::read_link(link).ok()?;
    Some(if target.is_absolute() { target } else { link.parent()?.join(target) })
}

fn is_link(path: &Path) -> bool {
    #[cfg(windows)]
    if junction::exists(path).unwrap_or(false) {
        return true;
    }
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

fn remove_link(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    if crate::library::junction_exists(path)? {
        return junction::delete(path);
    }
    if !fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Ok(());
    }
    fs::remove_dir(path).or_else(|error| fs::remove_file(path).or(Err(error)))
}

fn remove_directory(path: &Path) -> io::Result<()> {
    crate::library::remove_library(path)
}

fn temporary_path(path: &Path, kind: &str) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.bsmanager-{kind}-{}", Uuid::new_v4()))
}

fn backup_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{name}_backup"))
}

fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn same_path(left: &Path, right: &Path) -> bool {
    let left = fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    let left = left.components().collect::<Vec<_>>();
    let right = right.components().collect::<Vec<_>>();
    left.len() == right.len() && left.iter().zip(right.iter()).all(|(left, right)| {
        left.as_os_str().to_string_lossy().eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
    })
}

fn link_error(error: impl std::fmt::Display, code: &str) -> SerializedError {
    serialized_error(error.to_string(), code)
}
