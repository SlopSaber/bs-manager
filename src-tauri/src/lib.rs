use std::{collections::HashMap, fs, path::{Path, PathBuf}, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tauri::{AppHandle, Emitter, Manager, Runtime, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_dialog::{DialogExt, FilePath};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

mod library;
mod launch;
mod linker;
mod meta;
mod oculus;
mod shortcuts;
mod stores;
mod steam;
mod versions;
mod windows_integration;

const VERSION_LIBRARY: &str = include_str!("../../assets/jsons/bs-versions.json");
const SUPPORTERS: &str = include_str!("../../assets/jsons/patreons.json");
const INSTALL_FOLDER: &str = "BSManager";
const LEGACY_APP_DIRECTORY: &str = "bs-manager";
const PREVIEW_IMPORT_MARKER: &str = "preview-legacy-settings-import";

#[derive(Default)]
struct AppState {
    window_arguments: Mutex<HashMap<String, Value>>,
    library_operation: tokio::sync::Mutex<()>,
    meta_login: tokio::sync::Mutex<()>,
    oculus_download: Mutex<Option<oculus::OculusControl>>,
    launch_in_progress: AtomicBool,
    active_requests: Mutex<HashMap<String, Arc<AtomicBool>>>,
    steam_download: Mutex<Option<steam::SteamControl>>,
}

#[derive(Clone, Serialize)]
struct SerializedError {
    message: String,
    code: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IpcRequest {
    #[serde(default)]
    args: Value,
    #[serde(default)]
    responce_channel: Option<String>,
}

fn serialized_error(message: impl Into<String>, code: &str) -> SerializedError {
    SerializedError { message: message.into(), code: code.to_owned() }
}

fn home_directory<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    app.path().home_dir().map_err(|error| error.to_string())
}

fn settings_path<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    let resolver = app.path();
    if app.config().identifier == "io.bsmanager.bsmanager" {
        return legacy_settings_path(app);
    }

    resolver.app_config_dir()
        .map(|directory| directory.join("config.json"))
        .map_err(|error| error.to_string())
}

fn legacy_settings_path<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    app.path().config_dir()
        .map(|directory| directory.join(LEGACY_APP_DIRECTORY).join("config.json"))
        .map_err(|error| error.to_string())
}

fn preview_settings_import_needed<R: Runtime>(app: &AppHandle<R>) -> Result<bool, String> {
    if app.config().identifier != "io.bsmanager.bsmanager.preview" {
        return Ok(false);
    }

    if read_settings(app)?.contains_key(PREVIEW_IMPORT_MARKER) {
        return Ok(false);
    }

    let legacy_path = legacy_settings_path(app)?;
    match fs::metadata(legacy_path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Could not inspect legacy settings: {error}")),
    }
}

fn import_legacy_settings<R: Runtime>(app: &AppHandle<R>, import: bool) -> Result<(), String> {
    if app.config().identifier != "io.bsmanager.bsmanager.preview" {
        return Err("Legacy settings import is only available in the preview profile".to_owned());
    }

    if !preview_settings_import_needed(app)? {
        return Ok(());
    }

    let mut settings = if import {
        let legacy_path = legacy_settings_path(app)?;
        let contents = fs::read(&legacy_path).map_err(|error| format!("Could not read legacy settings: {error}"))?;
        serde_json::from_slice::<Map<String, Value>>(&contents)
            .map_err(|error| format!("Could not read legacy settings: {error}"))?
    } else {
        read_settings(app)?
    };

    if import {
        let preview_settings = read_settings(app)?;
        settings.extend(preview_settings);
    }
    settings.insert(PREVIEW_IMPORT_MARKER.to_owned(), json!(if import { "imported" } else { "skipped" }));
    write_settings(app, &settings)
}

fn read_settings<R: Runtime>(app: &AppHandle<R>) -> Result<Map<String, Value>, String> {
    let path = settings_path(app)?;
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice::<Map<String, Value>>(&contents)
            .map_err(|error| format!("Could not read application settings: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(format!("Could not read application settings: {error}")),
    }
}

fn write_settings<R: Runtime>(app: &AppHandle<R>, settings: &Map<String, Value>) -> Result<(), String> {
    let path = settings_path(app)?;
    let parent = path.parent().ok_or_else(|| "Application settings path is invalid".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| format!("Could not create settings folder: {error}"))?;
    let bytes = serde_json::to_vec_pretty(settings).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes).map_err(|error| format!("Could not write settings: {error}"))?;
    fs::rename(&temporary, &path).map_err(|error| format!("Could not replace settings: {error}"))
}

fn default_install_directory<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    let documents_path = app.path().document_dir().map_err(|error| error.to_string())?.join(INSTALL_FOLDER);
    if documents_path.is_dir() { return Ok(documents_path); }
    Ok(home_directory(app)?.join(INSTALL_FOLDER))
}

fn path_ends_in_install_folder(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(INSTALL_FOLDER))
}

fn normalize_install_parent(path: PathBuf) -> PathBuf {
    if path_ends_in_install_folder(&path) {
        if let Some(parent) = path.parent() {
            return parent.to_path_buf();
        }
    }
    path
}

fn install_directory<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    if let Some(path) = read_settings(app)?.get("installation-folder")
        .and_then(Value::as_str).filter(|path| !path.is_empty())
    {
        let path = PathBuf::from(path);
        // Accept early preview values that stored the full BSManager path.
        return if path_ends_in_install_folder(&path) {
            Ok(path)
        } else {
            Ok(path.join(INSTALL_FOLDER))
        };
    }
    default_install_directory(app)
}

fn resolve_window(template: &str) -> Option<(&'static str, &'static str, f64, f64, f64, f64, bool)> {
    match template.split('?').next()? {
        "index.html" => Some(("main", "index.html", 1080.0, 720.0, 900.0, 500.0, true)),
        "launcher.html" => Some(("launcher", "launcher.html", 380.0, 500.0, 380.0, 500.0, false)),
        "oneclick-download-map.html" => Some(("oneclick-download-map", "oneclick-download-map.html", 350.0, 400.0, 350.0, 400.0, false)),
        "oneclick-download-playlist.html" => Some(("oneclick-download-playlist", "oneclick-download-playlist.html", 350.0, 400.0, 350.0, 400.0, false)),
        "oneclick-download-model.html" => Some(("oneclick-download-model", "oneclick-download-model.html", 350.0, 400.0, 350.0, 400.0, false)),
        "shortcut-launch.html" => Some(("shortcut-launch", "shortcut-launch.html", 600.0, 500.0, 600.0, 300.0, true)),
        _ => None,
    }
}

fn create_window<R: Runtime>(app: &AppHandle<R>, template: &str, args: Option<Value>) -> Result<(), String> {
    let (label, page, width, height, min_width, min_height, resizable) = resolve_window(template)
        .ok_or_else(|| format!("Unsupported application window: {template}"))?;
    if let Some(args) = args.as_ref() {
        let state = app.state::<AppState>();
        state.window_arguments.lock().map_err(|_| "Window state is unavailable".to_owned())?
            .insert(label.to_owned(), args.clone());
    }
    if let Some(window) = app.get_webview_window(label) {
        window.show().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
        if let Some(args) = args {
            let _ = window.emit("window-arguments", args);
        }
        return Ok(());
    }

    let query = match args {
        Some(Value::Object(values)) if !values.is_empty() => {
            let values = values.into_iter().map(|(key, value)| {
                let value = value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string());
                format!("{}={}", urlencoding::encode(&key), urlencoding::encode(&value))
            }).collect::<Vec<_>>().join("&");
            format!("?{values}")
        }
        _ => String::new(),
    };
    let url = WebviewUrl::App(format!("{page}{query}").into());
    WebviewWindowBuilder::new(app, label, url)
        .title(if label == "main" { "BSManager" } else { "BSManager" })
        .inner_size(width, height)
        .min_inner_size(min_width, min_height)
        .resizable(resizable)
        .decorations(false)
        .visible(true)
        .build()
        .map_err(|error| format!("Could not open {template}: {error}"))?;
    Ok(())
}

fn unwrap_request(value: &Value) -> IpcRequest {
    serde_json::from_value(value.clone()).unwrap_or(IpcRequest { args: value.clone(), responce_channel: None })
}

fn ipc_response<R: Runtime>(window: &WebviewWindow<R>, channel: &str, request: &IpcRequest, result: Result<Value, SerializedError>, streamed: bool) {
    let Some(response_channel) = &request.responce_channel else { return };
    match result {
        Ok(value) => {
            if !streamed {
                let _ = window.emit(response_channel, value);
            }
            let complete_channel = format!("{response_channel}_complete");
            let _ = window.emit(&complete_channel, ());
        }
        Err(error) => {
            log::warn!("Windows Tauri route '{channel}' failed: {} ({})", error.message, error.code);
            let error_channel = format!("{response_channel}_error");
            let _ = window.emit(&error_channel, error);
        }
    }
}

async fn choose_paths<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    channel: &str,
    args: &Value,
) -> Result<Value, SerializedError> {
    let (sender, receiver) = tokio::sync::oneshot::channel::<Option<Vec<FilePath>>>();
    let title = args.get("title").and_then(Value::as_str).unwrap_or("Select files").to_owned();
    let mut builder = app.dialog().file().set_title(title).set_parent(window);
    let default_path = args.get("defaultPath").and_then(Value::as_str).or_else(|| args.as_str());
    let parent = args.get("parent").and_then(Value::as_str);
    if let Some(default_path) = default_path {
        let path = if parent == Some("home") && !Path::new(default_path).is_absolute() {
            home_directory(app).map_err(|error| serialized_error(error, "DIALOG_FAILED"))?.join(default_path)
        } else {
            PathBuf::from(default_path)
        };
        builder = builder.set_directory(path);
    } else if parent == Some("home") {
        builder = builder.set_directory(home_directory(app).map_err(|error| serialized_error(error, "DIALOG_FAILED"))?);
    }
    if let Some(filters) = args.get("filters").and_then(Value::as_array) {
        for filter in filters {
            let name = filter.get("name").and_then(Value::as_str).unwrap_or("File");
            let extensions = filter.get("extensions").and_then(Value::as_array).map(|values| {
                values.iter().filter_map(Value::as_str).collect::<Vec<_>>()
            }).unwrap_or_default();
            builder = builder.add_filter(name, &extensions);
        }
    }
    if channel == "choose-image" && args.get("filters").is_none() {
        builder = builder.add_filter("Images", &["jpg", "jpeg", "png"]);
    }

    let properties = args.get("properties").and_then(Value::as_array);
    let is_directory = channel == "choose-folder" || properties.is_some_and(|values| {
        values.iter().any(|value| value.as_str() == Some("openDirectory"))
    });
    let multiple = channel == "choose-image" || properties.is_some_and(|values| {
        values.iter().any(|value| value.as_str() == Some("multiSelections"))
    });
    if is_directory {
        builder.pick_folder(move |path| { let _ = sender.send(path.map(|path| vec![path])); });
    } else if multiple {
        builder.pick_files(move |paths| { let _ = sender.send(paths); });
    } else {
        builder.pick_file(move |path| { let _ = sender.send(path.map(|path| vec![path])); });
    }

    let Some(paths) = receiver.await.map_err(|error| serialized_error(error.to_string(), "DIALOG_FAILED"))? else {
        return Ok(json!({ "canceled": true, "filePaths": [] }));
    };
    let paths = paths.into_iter().map(|path| {
        path.into_path().map_err(|error| serialized_error(error.to_string(), "DIALOG_FAILED"))
    }).collect::<Result<Vec<_>, _>>()?;
    Ok(json!({ "canceled": false, "filePaths": paths }))
}

async fn save_file<R: Runtime>(app: &AppHandle<R>, window: &WebviewWindow<R>, args: &Value) -> Result<Value, SerializedError> {
    let (sender, receiver) = tokio::sync::oneshot::channel::<Option<FilePath>>();
    let mut builder = app.dialog().file().set_parent(window);
    if let Some(title) = args.get("title").and_then(Value::as_str) {
        builder = builder.set_title(title);
    }
    if let Some(directory) = args.get("defaultPath").and_then(Value::as_str) {
        let path = PathBuf::from(directory);
        if path.is_dir() { builder = builder.set_directory(path); }
    }
    if let Some(name) = args.get("filename").and_then(Value::as_str) {
        builder = builder.set_file_name(name);
    }
    if let Some(filters) = args.get("filters").and_then(Value::as_array) {
        for filter in filters {
            let name = filter.get("name").and_then(Value::as_str).unwrap_or("File");
            let extensions = filter.get("extensions").and_then(Value::as_array).map(|values| {
                values.iter().filter_map(Value::as_str).collect::<Vec<_>>()
            }).unwrap_or_default();
            builder = builder.add_filter(name, &extensions);
        }
    }
    builder.save_file(move |path| { let _ = sender.send(path); });
    let path = receiver.await.map_err(|error| serialized_error(error.to_string(), "DIALOG_FAILED"))?
        .ok_or_else(|| serialized_error("No file path selected", "DIALOG_CANCELLED"))?;
    let path = path.into_path().map_err(|error| serialized_error(error.to_string(), "DIALOG_FAILED"))?;
    Ok(json!(path))
}

async fn handle_request<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    channel: &str,
    args: Value,
    response_channel: Option<&str>,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    match channel {
        "import-version" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            versions::import_version(app, window, &args, response_channel, cancelled).await
        }
        "download-bs-version" | "auto-download-bs-version" | "download-bs-version-qr" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            let qr = channel == "download-bs-version-qr";
            let automatic = channel == "auto-download-bs-version";
            steam::download(app, window, response_channel, &args, cancelled, qr, automatic).await
        }
        "stop-download-bs-version" => Ok(steam::stop_download(app)),
        "send-input-bs-download" => Ok(steam::send_input(app, &args)),
        "login-with-meta" => {
            if window.label() != "main" {
                return Err(serialized_error("Meta login is only available in the main window", "WINDOW_NOT_ALLOWED"));
            }
            let app_state = app.state::<AppState>();
            let _login = app_state.meta_login.lock().await;
            let keep_token = args.as_bool().unwrap_or(false);
            meta::login(app, &cancelled, keep_token).await
        }
        "delete-meta-session" => {
            if window.label() != "main" {
                return Err(serialized_error("Meta session controls are only available in the main window", "WINDOW_NOT_ALLOWED"));
            }
            let app_state = app.state::<AppState>();
            let _login = app_state.meta_login.lock().await;
            meta::delete_session(app).await
        }
        "meta-session-exists" => Ok(meta::session_exists(app).await),
        "bs-oculus-download" => {
            {
                let active = app.state::<AppState>();
                if active.oculus_download.lock().is_ok_and(|download| download.is_some()) {
                    return Err(serialized_error("Already downloading", "ALREADY_DOWNLOADING"));
                }
            }
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            oculus::download(app, window, response_channel, &args, cancelled).await
        }
        "bs-oculus-stop-download" => Ok(oculus::stop_download(app)),
        "bs-launch.launch" => {
            if !matches!(window.label(), "main" | "shortcut-launch") {
                return Err(serialized_error("Launch is unavailable in this window", "WINDOW_NOT_ALLOWED"));
            }
            launch::launch(app, window, response_channel, &args).await
        }
        "bs-launch.need-start-as-admin" => Ok(windows_integration::need_start_as_admin().await),
        "create-launch-shortcut" => Ok(json!(shortcuts::create_launch_shortcut(app, &args).await)),
        "is-oculus-sideloaded-apps-enabled" => windows_integration::sideloaded_apps_enabled(),
        "enable-oculus-sideloaded-apps" => windows_integration::enable_sideloaded_apps(app).await,
        "vr-runtime.get-active" => Ok(windows_integration::active_vr_runtime(args.as_str())),
        "shortcut-launch-options" => shortcut_launch_options(app, window),
        "current-version" => Ok(json!(env!("CARGO_PKG_VERSION"))),
        "get-available-update" => Ok(Value::Null),
        "check-update" => Ok(json!(false)),
        "get-supporters" => serde_json::from_str(SUPPORTERS)
            .map_err(|error| serialized_error(error.to_string(), "SUPPORTERS_READ_FAILED")),
        "bs-installer.folder-exists" => Ok(json!(install_directory(app).map_err(|error| serialized_error(error, "INSTALL_PATH_FAILED"))?.is_dir())),
        "bs-installer.default-install-path" => Ok(json!(default_install_directory(app).map_err(|error| serialized_error(error, "INSTALL_PATH_FAILED"))?)),
        "bs-installer.install-path" => Ok(json!(install_directory(app).map_err(|error| serialized_error(error, "INSTALL_PATH_FAILED"))?)),
        "bs-installer.set-install-path" => {
            let path = args.get("path").and_then(Value::as_str).filter(|value| !value.trim().is_empty())
                .ok_or_else(|| serialized_error("Install path is required", "INVALID_INSTALL_PATH"))?;
            let move_existing = args.get("move").and_then(Value::as_bool).unwrap_or(false);
            let parent = if move_existing {
                let chosen = library::canonicalize_existing_directory(Path::new(path.trim()))
                    .map_err(|error| serialized_error(error, "INVALID_FOLDER"))?;
                normalize_install_parent(chosen)
            } else {
                normalize_install_parent(PathBuf::from(path.trim()))
            };
            let install_path = parent.join(INSTALL_FOLDER);

            if move_existing {
                let app_state = app.state::<AppState>();
                let _operation = app_state.library_operation.lock().await;
                let old_directory = install_directory(app)
                    .map_err(|error| serialized_error(error, "INSTALL_PATH_FAILED"))?;
                let source = old_directory.clone();
                let destination = install_path.clone();
                let copied = tokio::task::spawn_blocking(move || library::copy_library(&source, &destination))
                    .await
                    .map_err(|error| serialized_error(format!("Library move worker failed: {error}"), "LIBRARY_MOVE_FAILED"))?
                    .map_err(|error| match error {
                        library::LibraryError::CopyToSubpath => serialized_error(
                            "The destination cannot be inside or above the current library",
                            "COPY_TO_SUBPATH",
                        ),
                        library::LibraryError::Io(error) => serialized_error(error.to_string(), "LIBRARY_MOVE_FAILED"),
                    })?;

                let mut settings = read_settings(app).map_err(|error| serialized_error(error, "SETTINGS_READ_FAILED"))?;
                settings.insert("installation-folder".into(), json!(parent));
                write_settings(app, &settings).map_err(|error| serialized_error(error, "SETTINGS_WRITE_FAILED"))?;

                if copied {
                    tokio::task::spawn_blocking(move || library::remove_library(&old_directory))
                        .await
                        .map_err(|error| serialized_error(format!("Library cleanup worker failed: {error}"), "LIBRARY_MOVE_CLEANUP_FAILED"))?
                        .map_err(|error| serialized_error(error.to_string(), "LIBRARY_MOVE_CLEANUP_FAILED"))?;
                }
                return Ok(json!(install_path));
            }

            let mut settings = read_settings(app).map_err(|error| serialized_error(error, "SETTINGS_READ_FAILED"))?;
            settings.insert("installation-folder".into(), json!(parent));
            write_settings(app, &settings).map_err(|error| serialized_error(error, "SETTINGS_WRITE_FAILED"))?;
            Ok(json!(install_path))
        }
        "preview.legacy-settings-import-needed" => {
            if window.label() != "main" {
                return Err(serialized_error("Legacy settings import is only available in the main window", "WINDOW_NOT_ALLOWED"));
            }
            Ok(json!(preview_settings_import_needed(app).map_err(|error| serialized_error(error, "SETTINGS_IMPORT_CHECK_FAILED"))?))
        }
        "preview.import-legacy-settings" => {
            if window.label() != "main" {
                return Err(serialized_error("Legacy settings import is only available in the main window", "WINDOW_NOT_ALLOWED"));
            }
            import_legacy_settings(app, true).map_err(|error| serialized_error(error, "SETTINGS_IMPORT_FAILED"))?;
            Ok(Value::Null)
        }
        "preview.skip-legacy-settings-import" => {
            if window.label() != "main" {
                return Err(serialized_error("Legacy settings import is only available in the main window", "WINDOW_NOT_ALLOWED"));
            }
            import_legacy_settings(app, false).map_err(|error| serialized_error(error, "SETTINGS_IMPORT_FAILED"))?;
            Ok(Value::Null)
        }
        "static-configuration.get" => {
            let key = args.as_str().ok_or_else(|| serialized_error("Setting key is required", "INVALID_SETTING"))?;
            Ok(read_settings(app).map_err(|error| serialized_error(error, "SETTINGS_READ_FAILED"))?.get(key).cloned().unwrap_or(Value::Null))
        }
        "static-configuration.set" => {
            let key = args.get("key").and_then(Value::as_str).ok_or_else(|| serialized_error("Setting key is required", "INVALID_SETTING"))?;
            let value = args.get("value").cloned().unwrap_or(Value::Null);
            let mut settings = read_settings(app).map_err(|error| serialized_error(error, "SETTINGS_READ_FAILED"))?;
            settings.insert(key.to_owned(), value);
            write_settings(app, &settings).map_err(|error| serialized_error(error, "SETTINGS_WRITE_FAILED"))?;
            Ok(Value::Null)
        }
        "static-configuration.delete" => {
            let key = args.as_str().ok_or_else(|| serialized_error("Setting key is required", "INVALID_SETTING"))?;
            let mut settings = read_settings(app).map_err(|error| serialized_error(error, "SETTINGS_READ_FAILED"))?;
            settings.remove(key);
            write_settings(app, &settings).map_err(|error| serialized_error(error, "SETTINGS_WRITE_FAILED"))?;
            Ok(Value::Null)
        }
        "bs-version.get-version-dict" => serde_json::from_str(VERSION_LIBRARY).map_err(|error| serialized_error(error.to_string(), "VERSION_LIBRARY_READ_FAILED")),
        "bs-version.installed-versions" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            let app = app.clone();
            tokio::task::spawn_blocking(move || versions::get_installed_versions(&app))
                .await
                .map_err(|error| serialized_error(format!("Version scan worker failed: {error}"), "VERSION_SCAN_FAILED"))?
        }
        "bs-version.open-folder" => {
            let path = versions::version_path(app, &args)?;
            if path.is_dir() {
                app.opener().open_path(path.to_string_lossy().to_string(), None::<&str>)
                    .map_err(|error| serialized_error(error.to_string(), "OPEN_PATH_FAILED"))?;
            }
            Ok(Value::Null)
        }
        "bs-version.edit" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            versions::edit_version(app, &args)
        }
        "bs-version.clone" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            versions::clone_version(app, &args).await
        }
        "bs.uninstall" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            versions::uninstall_version(app, &args).await
        }
        "get-version-full-path" => {
            Ok(json!(versions::version_path(app, &args)?.to_string_lossy().to_string()))
        }
        "full-version-path-to-relative" => {
            let version = args.get("version").ok_or_else(|| serialized_error("Version is required", "INVALID_VERSION"))?;
            let version_path = versions::version_path(app, version)?;
            let full_path = args.get("fullPath").and_then(Value::as_str)
                .ok_or_else(|| serialized_error("Full path is required", "INVALID_PATH"))?;
            Ok(json!(versions::relative_path(&version_path, Path::new(full_path)).to_string_lossy().to_string()))
        }
        "get-linked-folders" => linker::get_linked_folders(app, &args),
        "link-version-folder-action" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            let app_handle = app.clone();
            let request_args = args.clone();
            tokio::task::spawn_blocking(move || linker::do_action(&app_handle, &request_args))
                .await
                .map_err(|error| serialized_error(format!("Folder link worker failed: {error}"), "LINK_FAILED"))?
        }
        "is-version-folder-linked" => linker::is_version_folder_linked(app, &args),
        "relink-all-versions-folders" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            let app_handle = app.clone();
            tokio::task::spawn_blocking(move || linker::relink_all(&app_handle))
                .await
                .map_err(|error| serialized_error(format!("Folder relink worker failed: {error}"), "LINK_FAILED"))?
        }
        "get-shared-folder" => Ok(json!(linker::shared_folder(app)?.to_string_lossy().to_string())),
        "register-maps-deep-link" => set_protocols(app, &["beatsaver", "web+bsmap"], true),
        "unregister-maps-deep-link" => set_protocols(app, &["beatsaver", "web+bsmap"], false),
        "is-map-deep-links-enabled" => are_protocols_registered(app, &["beatsaver", "web+bsmap"]),
        "register-playlists-deep-link" => set_protocols(app, &["bsplaylist"], true),
        "unregister-playlists-deep-link" => set_protocols(app, &["bsplaylist"], false),
        "is-playlists-deep-links-enabled" => are_protocols_registered(app, &["bsplaylist"]),
        "register-models-deep-link" => set_protocols(app, &["modelsaber"], true),
        "unregister-models-deep-link" => set_protocols(app, &["modelsaber"], false),
        "is-models-deep-links-enabled" => are_protocols_registered(app, &["modelsaber"]),
        "close-window" => { window.close().map_err(|error| serialized_error(error.to_string(), "WINDOW_FAILED"))?; Ok(Value::Null) }
        "minimise-window" => { window.minimize().map_err(|error| serialized_error(error.to_string(), "WINDOW_FAILED"))?; Ok(Value::Null) }
        "maximise-window" => { window.maximize().map_err(|error| serialized_error(error.to_string(), "WINDOW_FAILED"))?; Ok(Value::Null) }
        "unmaximise-window" => { window.unmaximize().map_err(|error| serialized_error(error.to_string(), "WINDOW_FAILED"))?; Ok(Value::Null) }
        "new-window" => {
            let url = args.as_str().ok_or_else(|| serialized_error("A link is required", "INVALID_URL"))?;
            let parsed = url::Url::parse(url).map_err(|error| serialized_error(error.to_string(), "INVALID_URL"))?;
            if parsed.scheme() != "https" && parsed.scheme() != "http" {
                return Err(serialized_error("Only HTTP and HTTPS links can be opened", "INVALID_URL"));
            }
            app.opener().open_url(url, None::<&str>).map_err(|error| serialized_error(error.to_string(), "OPEN_URL_FAILED"))?;
            Ok(Value::Null)
        }
        "choose-folder" | "choose-file" | "open-dialog" => choose_paths(app, window, channel, &args).await,
        "save-file" => save_file(app, window, &args).await,
        "choose-image" => {
            let paths = choose_paths(app, window, channel, &args).await?
                .get("filePaths").and_then(Value::as_array).cloned().unwrap_or_default();
            if args.get("base64").and_then(Value::as_bool).unwrap_or(false) {
                let images = paths.iter().filter_map(Value::as_str).filter_map(|path| fs::read(path).ok())
                    .map(|bytes| BASE64.encode(bytes)).collect::<Vec<_>>();
                Ok(json!(images))
            } else { Ok(json!(paths)) }
        }
        "open-window-or-focus" | "open-window-then-close-all" => {
            let template = args.as_str().ok_or_else(|| serialized_error("Window name is required", "INVALID_WINDOW"))?;
            if channel == "open-window-then-close-all" {
                for (label, _, _, _, _, _, _) in [
                    resolve_window("index.html"), resolve_window("launcher.html"),
                    resolve_window("oneclick-download-map.html"), resolve_window("oneclick-download-playlist.html"),
                    resolve_window("oneclick-download-model.html"), resolve_window("shortcut-launch.html"),
                ].into_iter().flatten() {
                    if label != resolve_window(template).map(|value| value.0).unwrap_or("") {
                        if let Some(other) = app.get_webview_window(label) { let _ = other.close(); }
                    }
                }
            }
            create_window(app, template, None).map_err(|error| serialized_error(error, "WINDOW_FAILED"))?;
            Ok(Value::Null)
        }
        "open-logs" => {
            let path = app.path().app_log_dir().map_err(|error| serialized_error(error.to_string(), "LOG_PATH_FAILED"))?;
            app.opener().open_path(path.to_string_lossy().to_string(), None::<&str>)
                .map_err(|error| serialized_error(error.to_string(), "OPEN_PATH_FAILED"))?;
            Ok(Value::Null)
        }
        "view-path-in-explorer" => {
            let path = args.as_str().ok_or_else(|| serialized_error("A path is required", "INVALID_PATH"))?;
            app.opener().reveal_item_in_dir(path).map_err(|error| serialized_error(error.to_string(), "OPEN_PATH_FAILED"))?;
            Ok(Value::Null)
        }
        "window.progression" => Ok(Value::Null),
        "notify-system" => {
            let title = args.get("title").and_then(Value::as_str).unwrap_or("BSManager");
            let body = args.get("body").and_then(Value::as_str).unwrap_or("");
            app.notification().builder().title(title).body(body)
                .show().map_err(|error| serialized_error(error.to_string(), "NOTIFICATION_FAILED"))?;
            Ok(Value::Null)
        }
        "log-error" => { log::error!("Renderer error: {args}"); Ok(Value::Null) }
        "add-filter-string" | "add-filter-pattern" => Ok(Value::Null),
        "get-window-args" => {
            let label = window.label().to_owned();
            let state = app.state::<AppState>();
            let args = state.window_arguments.lock().map_err(|_| serialized_error("Window state is unavailable", "WINDOW_FAILED"))?
                .get(&label).cloned().unwrap_or_else(|| json!({}));
            Ok(args)
        }
        _ => Err(serialized_error(format!("This Windows Tauri preview does not implement '{channel}' yet"), "NOT_IMPLEMENTED")),
    }
}

#[tauri::command]
async fn ipc_send<R: Runtime>(app: AppHandle<R>, window: WebviewWindow<R>, channel: String, args: Option<Value>) {
    if !is_application_window(window.label()) {
        log::warn!("Rejected IPC route '{channel}' from window '{}'", window.label());
        return;
    }
    if let Some(response_id) = channel.strip_suffix("_teardown") {
        let state = app.state::<AppState>();
        if let Ok(mut requests) = state.active_requests.lock() {
            if let Some(cancelled) = requests.remove(response_id) {
                cancelled.store(true, Ordering::Relaxed);
            }
        }
        return;
    }
    let request = unwrap_request(&args.unwrap_or(Value::Null));
    let has_response = request.responce_channel.is_some();
    let request_args = request.args.clone();
    let cancelled = Arc::new(AtomicBool::new(false));
    if let Some(response_id) = &request.responce_channel {
        let state = app.state::<AppState>();
        if let Ok(mut requests) = state.active_requests.lock() {
            requests.insert(response_id.clone(), cancelled.clone());
        }
    }
    let result = handle_request(
        &app,
        &window,
        &channel,
        request_args,
        request.responce_channel.as_deref(),
        cancelled,
    ).await;
    if let Some(response_id) = &request.responce_channel {
        let state = app.state::<AppState>();
        if let Ok(mut requests) = state.active_requests.lock() {
            requests.remove(response_id);
        }
    }
    if !has_response {
        if let Err(error) = result {
            log::warn!("Windows Tauri one-way route '{channel}' failed: {}", error.message);
        }
    } else {
        ipc_response(&window, &channel, &request, result, matches!(channel.as_str(), "import-version" | "download-bs-version" | "auto-download-bs-version" | "download-bs-version-qr" | "bs-oculus-download" | "bs-launch.launch"));
    }
}

#[tauri::command]
fn register_protocol<R: Runtime>(app: AppHandle<R>, window: WebviewWindow<R>, protocol: String) -> Result<bool, String> {
    if !is_application_window(window.label()) { return Err("Protocol registration is unavailable in this window".to_owned()); }
    let supported = ["bsmanager", "beatsaver", "web+bsmap", "bsplaylist", "modelsaber"];
    if !supported.contains(&protocol.as_str()) { return Err("Unsupported BSManager protocol".to_owned()); }
    app.deep_link().register(&protocol).map_err(|error| error.to_string())?;
    Ok(true)
}

fn is_application_window(label: &str) -> bool {
    matches!(label, "main" | "launcher" | "oneclick-download-map" | "oneclick-download-playlist" | "oneclick-download-model" | "shortcut-launch")
}

fn handle_launch_argument<R: Runtime>(app: &AppHandle<R>, argument: &str) {
    if let Ok(url) = url::Url::parse(argument) {
        if ["bsmanager", "beatsaver", "web+bsmap", "bsplaylist", "modelsaber"].contains(&url.scheme()) {
            let mut args = Map::new();
            let template = match url.scheme() {
                "bsmanager" if url.host_str() == Some("launch") => {
                    for (key, value) in url.query_pairs() {
                        args.insert(key.into_owned(), Value::String(value.into_owned()));
                    }
                    Some("shortcut-launch.html")
                }
                "beatsaver" if !url.host_str().unwrap_or_default().is_empty() => {
                    args.insert("mapId".into(), json!(url.host_str().unwrap_or_default()));
                    args.insert("isHash".into(), json!("false"));
                    Some("oneclick-download-map.html")
                }
                "web+bsmap" if !url.host_str().unwrap_or_default().is_empty() => {
                    args.insert("mapId".into(), json!(url.host_str().unwrap_or_default()));
                    args.insert("isHash".into(), json!("true"));
                    Some("oneclick-download-map.html")
                }
                "bsplaylist" if url.host_str() == Some("playlist") => {
                    let playlist = format!("{}{}", url.path().trim_start_matches('/'), url.query().map(|query| format!("?{query}")).unwrap_or_default());
                    args.insert("playlistUrl".into(), json!(playlist));
                    Some("oneclick-download-playlist.html")
                }
                "modelsaber" => {
                    let model_id = url.path().trim_start_matches('/').split('/').next().unwrap_or_default();
                    if model_id.is_empty() { None } else {
                        args.insert("modelId".into(), json!(model_id));
                        Some("oneclick-download-model.html")
                    }
                }
                _ => None,
            };
            if let Some(template) = template {
                if let Err(error) = create_window(app, template, Some(Value::Object(args))) {
                    log::warn!("Could not open deep-link window: {error}");
                }
            } else {
                let _ = app.emit("app-url", argument.to_owned());
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        }
    } else if Path::new(argument).extension().is_some_and(|extension| extension.eq_ignore_ascii_case("bplist")) {
        let args = json!({ "playlistUrl": argument });
        if let Err(error) = create_window(app, "oneclick-download-playlist.html", Some(args)) {
            log::warn!("Could not open playlist file: {error}");
        }
    }
}

fn shortcut_launch_options<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
) -> Result<Value, SerializedError> {
    if window.label() != "shortcut-launch" {
        return Err(serialized_error("Shortcut launch options are only available in the shortcut window", "WINDOW_NOT_ALLOWED"));
    }
    let state = app.state::<AppState>();
    let args = state.window_arguments.lock()
        .map_err(|_| serialized_error("Window state is unavailable", "WINDOW_FAILED"))?
        .get("shortcut-launch").cloned().unwrap_or_else(|| json!({}));
    let version_id = args.get("version").and_then(Value::as_str).filter(|value| !value.is_empty())
        .ok_or_else(|| serialized_error("Shortcut does not include a Beat Saber version", "INVALID_VERSION"))?;
    let catalog = serde_json::from_str::<Vec<Value>>(VERSION_LIBRARY)
        .map_err(|error| serialized_error(error.to_string(), "VERSION_LIBRARY_READ_FAILED"))?;
    let mut version = catalog.iter().find(|item| item.get("BSVersion").and_then(Value::as_str) == Some(version_id))
        .cloned().unwrap_or_else(|| json!({ "BSVersion": version_id }));
    let steam = args.get("versionSteam").and_then(Value::as_str) == Some("true");
    let oculus = args.get("versionOculus").and_then(Value::as_str) == Some("true");
    {
        let object = version.as_object_mut()
            .ok_or_else(|| serialized_error("Version catalog entry is invalid", "INVALID_VERSION"))?;
        if let Some(name) = args.get("versionName").and_then(Value::as_str).filter(|name| !name.is_empty()) {
            object.insert("name".to_owned(), json!(name));
        }
        object.insert("steam".to_owned(), json!(steam));
        object.insert("oculus".to_owned(), json!(oculus));
        if let Some(ino) = args.get("versionIno").and_then(Value::as_str).and_then(|value| value.parse::<u64>().ok()) {
            object.insert("ino".to_owned(), json!(ino));
        }
    }
    let version_path = versions::version_path(app, &version)?;
    let mut metadata = if steam {
        json!({ "id": "", "store": "steam" })
    } else if oculus {
        json!({ "id": "", "store": "oculus" })
    } else {
        json!({ "id": "", "store": "steam" })
    };
    if !steam && !oculus {
        if let Ok(bytes) = fs::read(version_path.join("metadata.config")) {
            if let Ok(stored) = serde_json::from_slice::<Value>(&bytes) {
                if stored.get("store").and_then(Value::as_str).is_some_and(|store| matches!(store, "steam" | "oculus")) {
                    metadata = stored;
                }
            }
        }
    }
    let object = version.as_object_mut()
        .ok_or_else(|| serialized_error("Version catalog entry is invalid", "INVALID_VERSION"))?;
    object.insert("metadata".to_owned(), metadata);
    object.insert("path".to_owned(), json!(version_path));
    let mut launch_mods = Vec::new();
    for (query_key, mod_name) in [
        ("oculusMode", "oculus"), ("desktopMode", "fpfc"), ("debug", "debug"),
        ("skipSteam", "skip_steam"), ("mapEditor", "editor"), ("protonLogs", "proton_logs"),
        ("parallelViews", "parallel_views"),
    ] {
        if args.get(query_key).and_then(Value::as_str) == Some("true") {
            launch_mods.push(json!(mod_name));
        }
    }
    let mut result = json!({ "version": version, "launchMods": launch_mods });
    if let Some(command) = args.get("command").and_then(Value::as_str) {
        result["command"] = json!(command);
    }
    Ok(result)
}

fn set_protocols<R: Runtime>(app: &AppHandle<R>, protocols: &[&str], register: bool) -> Result<Value, SerializedError> {
    for protocol in protocols {
        let result = if register { app.deep_link().register(protocol) } else { app.deep_link().unregister(protocol) };
        result.map_err(|error| serialized_error(error.to_string(), "PROTOCOL_REGISTRATION_FAILED"))?;
    }
    Ok(json!(true))
}

fn are_protocols_registered<R: Runtime>(app: &AppHandle<R>, protocols: &[&str]) -> Result<Value, SerializedError> {
    let mut registered = true;
    for protocol in protocols {
        registered &= app.deep_link().is_registered(protocol)
            .map_err(|error| serialized_error(error.to_string(), "PROTOCOL_QUERY_FAILED"))?;
    }
    Ok(json!(registered))
}

pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            for argument in args { handle_launch_argument(app, &argument); }
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_log::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .invoke_handler(tauri::generate_handler![ipc_send, register_protocol])
        .setup(|app| {
            for argument in std::env::args_os().skip(1).filter_map(|argument| argument.into_string().ok()) {
                handle_launch_argument(app.handle(), &argument);
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("BSManager Tauri failed to start");
}
