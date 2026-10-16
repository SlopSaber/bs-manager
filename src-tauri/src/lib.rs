use std::{collections::HashMap, fs, io::Write, path::{Path, PathBuf}, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Map, Value, json};
use tauri::{AppHandle, Emitter, Manager, Runtime, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent, ipc::Channel, window::{ProgressBarState, ProgressBarStatus}};
use tauri_plugin_updater::{Update, UpdaterExt};
use regex::Regex;
use tauri_plugin_dialog::{DialogExt, FilePath};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

mod library;
mod archive_io;
mod launch;
mod linker;
mod liv;
mod meta;
mod models;
mod oculus;
mod shortcuts;
mod stores;
mod steam;
mod versions;
mod windows_integration;
mod beat_saver;
mod maps;
mod playlists;
mod mods;
mod protocols;
mod proxy;
mod map_info_cache;
mod song_cache;
mod legacy_storage;

const VERSION_LIBRARY: &str = include_str!("../../assets/jsons/bs-versions.json");
const SUPPORTERS: &str = include_str!("../../assets/jsons/patreons.json");
const INSTALL_FOLDER: &str = "BSManager";
const LEGACY_APP_DIRECTORY: &str = "bs-manager";
const PREVIEW_IMPORT_MARKER: &str = "preview-legacy-settings-import";
const PREVIEW_RENDERER_IMPORT_MARKER: &str = "preview-renderer-settings-import";
const PREVIEW_RENDERER_IMPORT_VERSION: u64 = 4;
const MAX_UPDATE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Default)]
struct AppState {
    window_arguments: Mutex<HashMap<String, Value>>,
    library_operation: tokio::sync::Mutex<()>,
    meta_login: tokio::sync::Mutex<()>,
    oculus_download: Mutex<Option<oculus::OculusControl>>,
    launch_in_progress: AtomicBool,
    active_requests: Mutex<HashMap<String, ActiveRequest>>,
    response_channels: Mutex<HashMap<String, Channel<Value>>>,
    pending_update: tokio::sync::Mutex<Option<PendingUpdate>>,
    update_operation: tokio::sync::Mutex<()>,
    steam_download: Mutex<Option<steam::SteamControl>>,
    log_redactions: Arc<Mutex<LogRedactions>>,
}

struct PendingUpdate {
    update: Update,
    downloaded: Option<Vec<u8>>,
}

struct ActiveRequest {
    window_label: String,
    cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
struct LogRedactions {
    strings: Vec<String>,
    patterns: Vec<Regex>,
}

impl LogRedactions {
    fn with_default_patterns() -> Self {
        Self {
            strings: Vec::new(),
            patterns: Regex::new(r"(FRL|OC)\S{10,}").into_iter().collect(),
        }
    }

    fn redact(&self, message: &str) -> String {
        let mut redacted = message.to_owned();
        for secret in &self.strings {
            if !secret.is_empty() {
                redacted = redacted.replace(secret, "****");
            }
        }
        for pattern in &self.patterns {
            redacted = pattern.replace_all(&redacted, "****").into_owned();
        }
        redacted
    }

    fn add_string(&mut self, secret: &str) -> Result<(), SerializedError> {
        if secret.len() > 16 * 1024 {
            return Err(serialized_error("Log redaction string is too long", "INVALID_LOG_FILTER"));
        }
        if !secret.is_empty() && !self.strings.iter().any(|value| value == secret) {
            if self.strings.len() >= 512 {
                return Err(serialized_error("Too many log redaction strings", "LOG_FILTER_LIMIT"));
            }
            self.strings.push(secret.to_owned());
        }
        Ok(())
    }

    fn add_pattern(&mut self, source: &str) -> Result<(), SerializedError> {
        if source.len() > 1024 {
            return Err(serialized_error("Log redaction pattern is too long", "INVALID_LOG_FILTER"));
        }
        let pattern = Regex::new(source)
            .map_err(|_| serialized_error("Log redaction pattern is invalid", "INVALID_LOG_FILTER"))?;
        if !self.patterns.iter().any(|existing| existing.as_str() == source) {
            if self.patterns.len() >= 128 {
                return Err(serialized_error("Too many log redaction patterns", "LOG_FILTER_LIMIT"));
            }
            self.patterns.push(pattern);
        }
        Ok(())
    }
}

#[derive(Clone, Serialize)]
struct SerializedError {
    message: String,
    code: String,
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

fn legacy_local_storage_path<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    app.path().config_dir()
        .map(|directory| directory.join(LEGACY_APP_DIRECTORY).join("Local Storage").join("leveldb"))
        .map_err(|error| error.to_string())
}

fn preview_settings_import_needed<R: Runtime>(app: &AppHandle<R>) -> Result<bool, String> {
    if app.config().identifier != "io.bsmanager.bsmanager.preview" {
        return Ok(false);
    }

    let settings = read_settings(app)?;
    if settings.get(PREVIEW_RENDERER_IMPORT_MARKER).and_then(Value::as_u64)
        == Some(PREVIEW_RENDERER_IMPORT_VERSION) {
        return Ok(false);
    }
    let has_renderer_settings = legacy_local_storage_path(app)?.join("CURRENT").is_file();
    let legacy_path = legacy_settings_path(app)?;
    let has_legacy_settings = match fs::metadata(legacy_path) {
        Ok(metadata) => metadata.is_file(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(format!("Could not inspect legacy settings: {error}")),
    };
    Ok(has_legacy_settings || has_renderer_settings)
}

fn import_legacy_settings<R: Runtime>(app: &AppHandle<R>, import: bool) -> Result<(), String> {
    if app.config().identifier != "io.bsmanager.bsmanager.preview" {
        return Err("Legacy settings import is only available in the preview profile".to_owned());
    }

    if !preview_settings_import_needed(app)? {
        return Ok(());
    }

    let mut settings = read_settings(app)?;
    if import {
        let legacy_path = legacy_settings_path(app)?;
        if legacy_path.is_file() {
            let contents = fs::read(&legacy_path).map_err(|error| format!("Could not read legacy settings: {error}"))?;
            let mut legacy = serde_json::from_slice::<Map<String, Value>>(&contents)
                .map_err(|error| format!("Could not read legacy settings: {error}"))?;
            legacy.extend(settings);
            settings = legacy;
        }
        settings.insert(PREVIEW_IMPORT_MARKER.to_owned(), json!("imported"));
    } else {
        if !settings.get(PREVIEW_IMPORT_MARKER).and_then(Value::as_str).is_some_and(|status| status == "imported") {
            settings.insert(PREVIEW_IMPORT_MARKER.to_owned(), json!("skipped"));
        }
        settings.insert(PREVIEW_RENDERER_IMPORT_MARKER.to_owned(), json!(PREVIEW_RENDERER_IMPORT_VERSION));
    }
    write_settings(app, &settings)?;
    proxy::set_enabled(settings.get("use-system-proxy").and_then(Value::as_bool).unwrap_or(false));
    Ok(())
}

fn complete_legacy_renderer_settings_import<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    if app.config().identifier != "io.bsmanager.bsmanager.preview" {
        return Err("Legacy renderer settings import is only available in the preview profile".to_owned());
    }
    let mut settings = read_settings(app)?;
    settings.insert(PREVIEW_RENDERER_IMPORT_MARKER.to_owned(), json!(PREVIEW_RENDERER_IMPORT_VERSION));
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

fn configured_update_source<R: Runtime>(app: &AppHandle<R>) -> Result<Option<(url::Url, String)>, SerializedError> {
    let (endpoint, public_key) = match app.config().identifier.as_str() {
        "io.bsmanager.bsmanager.preview" => (
            option_env!("BSM_TAURI_PREVIEW_UPDATE_ENDPOINT"),
            option_env!("BSM_TAURI_PREVIEW_UPDATE_PUBLIC_KEY"),
        ),
        "io.bsmanager.bsmanager" => (
            option_env!("BSM_TAURI_REPLACEMENT_UPDATE_ENDPOINT"),
            option_env!("BSM_TAURI_REPLACEMENT_UPDATE_PUBLIC_KEY"),
        ),
        _ => return Ok(None),
    };

    match (endpoint.filter(|value| !value.trim().is_empty()), public_key.filter(|value| !value.trim().is_empty())) {
        (None, None) => Ok(None),
        (Some(endpoint), Some(public_key)) => {
            let parsed = url::Url::parse(endpoint)
                .map_err(|error| serialized_error(error.to_string(), "UPDATE_CONFIGURATION_INVALID"))?;
            if parsed.scheme() != "https" {
                return Err(serialized_error("Tauri update endpoint must use HTTPS", "UPDATE_CONFIGURATION_INVALID"));
            }
            Ok(Some((parsed, public_key.to_owned())))
        }
        _ => Err(serialized_error(
            "Tauri update endpoint and public key must be configured together",
            "UPDATE_CONFIGURATION_INCOMPLETE",
        )),
    }
}

async fn check_for_update<R: Runtime>(app: &AppHandle<R>) -> Result<Option<Update>, SerializedError> {
    let Some((endpoint, public_key)) = configured_update_source(app)? else { return Ok(None) };
    let builder = app.updater_builder()
        .pubkey(public_key)
        .configure_client(|builder| {
            if crate::proxy::is_enabled() { builder } else { builder.no_proxy() }
        })
        .endpoints(vec![endpoint])
        .map_err(|error| serialized_error(error.to_string(), "UPDATE_CONFIGURATION_INVALID"))?;
    builder.build()
        .map_err(|error| serialized_error(error.to_string(), "UPDATE_CONFIGURATION_INVALID"))?
        .check()
        .await
        .map_err(|error| serialized_error(error.to_string(), "UPDATE_CHECK_FAILED"))
}

async fn set_pending_update<R: Runtime>(app: &AppHandle<R>, update: Option<Update>) {
    let state = app.state::<AppState>();
    let mut pending = state.pending_update.lock().await;
    *pending = update.map(|update| PendingUpdate { update, downloaded: None });
}

fn emit_update_progress<R: Runtime>(
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    current: u64,
    total: u64,
) -> Result<(), SerializedError> {
    if let Some(channel) = response_channel {
        emit_response_value(window, Some(channel), json!({ "current": current, "total": total }))
            .map_err(|error| serialized_error(error.message, "UPDATE_PROGRESS_FAILED"))?;
    }
    Ok(())
}

async fn wait_for_update_cancellation(cancelled: &AtomicBool) {
    while !cancelled.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn download_update<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let pending = app.state::<AppState>().pending_update.lock().await.take();
    let mut pending = match pending {
        Some(pending) => pending,
        None => {
            if configured_update_source(app)?.is_none() {
                return Err(serialized_error("Remote updates are disabled until signed Tauri update sources are configured", "UPDATES_DISABLED"));
            }
            PendingUpdate {
                update: check_for_update(app).await?
                    .ok_or_else(|| serialized_error("No update is available", "NO_UPDATE_AVAILABLE"))?,
                downloaded: None,
            }
        }
    };
    if pending.downloaded.is_some() {
        *app.state::<AppState>().pending_update.lock().await = Some(pending);
        return Ok(Value::Null);
    }

    let client = crate::proxy::client_builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(30 * 60))
        .build()
        .map_err(|error| serialized_error(error.to_string(), "UPDATE_NETWORK_FAILED"))?;
    let response = tokio::select! {
        response = client.get(pending.update.download_url.clone()).send() => {
            response.map_err(|error| serialized_error(error.to_string(), "UPDATE_DOWNLOAD_FAILED"))?
        }
        _ = wait_for_update_cancellation(&cancelled) => {
            return Err(serialized_error("Update download was cancelled", "REQUEST_CANCELLED"));
        }
    };
    if !response.status().is_success() {
        return Err(serialized_error(format!("Update download returned HTTP {}", response.status()), "UPDATE_DOWNLOAD_FAILED"));
    }
    let total = response.content_length().unwrap_or(0);
    if total > MAX_UPDATE_BYTES {
        return Err(serialized_error("Update package exceeds the 2 GiB size limit", "UPDATE_TOO_LARGE"));
    }
    emit_update_progress(window, response_channel, 0, total)?;
    let mut downloaded = Vec::new();
    let mut current = 0_u64;
    let mut stream = response.bytes_stream();
    loop {
        let next = tokio::select! {
            chunk = stream.next() => chunk,
            _ = wait_for_update_cancellation(&cancelled) => {
                return Err(serialized_error("Update download was cancelled", "REQUEST_CANCELLED"));
            }
        };
        let Some(result) = next else { break };
        let chunk = result.map_err(|error| serialized_error(error.to_string(), "UPDATE_DOWNLOAD_FAILED"))?;
        current = current.saturating_add(chunk.len() as u64);
        if current > MAX_UPDATE_BYTES {
            return Err(serialized_error("Update package exceeds the 2 GiB size limit", "UPDATE_TOO_LARGE"));
        }
        downloaded.extend_from_slice(&chunk);
        if total > 0 {
            emit_update_progress(window, response_channel, current.min(total), total)?;
        }
    }
    if cancelled.load(Ordering::Relaxed) {
        return Err(serialized_error("Update download was cancelled", "REQUEST_CANCELLED"));
    }
    pending.update.verify(&downloaded)
        .map_err(|error| serialized_error(error.to_string(), "UPDATE_SIGNATURE_INVALID"))?;
    emit_update_progress(window, response_channel, 100, 100)?;
    pending.downloaded = Some(downloaded);
    *app.state::<AppState>().pending_update.lock().await = Some(pending);
    Ok(Value::Null)
}

async fn install_update<R: Runtime>(app: &AppHandle<R>) -> Result<Value, SerializedError> {
    let pending = app.state::<AppState>().pending_update.lock().await.take()
        .ok_or_else(|| serialized_error("No downloaded update is ready to install", "NO_DOWNLOADED_UPDATE"))?;
    let bytes = pending.downloaded
        .ok_or_else(|| serialized_error("Update package has not been downloaded", "NO_DOWNLOADED_UPDATE"))?;
    pending.update.install(bytes)
        .map_err(|error| serialized_error(error.to_string(), "UPDATE_INSTALL_FAILED"))?;
    Ok(Value::Null)
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
    let title = if app.config().identifier == "io.bsmanager.bsmanager.preview" {
        "BSManager Preview"
    } else {
        "BSManager"
    };
    WebviewWindowBuilder::new(app, label, url)
        .title(title)
        .inner_size(width, height)
        .min_inner_size(min_width, min_height)
        .resizable(resizable)
        .center()
        .decorations(false)
        .visible(true)
        .build()
        .map_err(|error| format!("Could not open {template}: {error}"))?;
    if let Some(window) = app.get_webview_window(label) {
        let lifecycle_app = app.clone();
        let lifecycle_label = label.to_owned();
        window.on_window_event(move |event| {
            if matches!(event, WindowEvent::Destroyed) {
                cancel_window_requests(&lifecycle_app, &lifecycle_label);
            }
        });
    }
    Ok(())
}

fn cancel_window_requests<R: Runtime>(app: &AppHandle<R>, label: &str) {
    let state = app.state::<AppState>();
    let mut cancelled_ids = Vec::new();
    if let Ok(mut requests) = state.active_requests.lock() {
        requests.retain(|id, request| {
            if request.window_label == label {
                request.cancelled.store(true, Ordering::Relaxed);
                cancelled_ids.push(id.clone());
                false
            } else {
                true
            }
        });
    }
    if let Ok(mut responses) = state.response_channels.lock() {
        for id in cancelled_ids { responses.remove(&id); }
    }
}

fn ipc_response<R: Runtime>(window: &WebviewWindow<R>, channel: &str, response_id: Option<&str>, result: Result<Value, SerializedError>, streamed: bool) {
    let Some(response_id) = response_id else { return };
    match result {
        Ok(value) => {
            if !streamed {
                let _ = send_response_message(window, response_id, json!({ "kind": "value", "payload": value }));
            }
            let _ = send_response_message(window, response_id, json!({ "kind": "complete" }));
        }
        Err(error) => {
            log::warn!("Windows Tauri route '{channel}' failed: {} ({})", error.message, error.code);
            let _ = send_response_message(window, response_id, json!({ "kind": "error", "payload": error }));
        }
    }
}

fn send_response_message<R: Runtime>(window: &WebviewWindow<R>, response_id: &str, message: Value) -> Result<(), SerializedError> {
    let app = window.app_handle();
    let response = app.state::<AppState>().response_channels.lock()
        .map_err(|_| serialized_error("IPC response channels are unavailable", "IPC_EMIT_FAILED"))?
        .get(response_id).cloned();
    if let Some(response) = response {
        response.send(message).map_err(|error| serialized_error(error.to_string(), "IPC_EMIT_FAILED"))?;
    }
    Ok(())
}

pub(crate) fn emit_response_value<R: Runtime>(window: &WebviewWindow<R>, response_id: Option<&str>, value: Value) -> Result<(), SerializedError> {
    if let Some(response_id) = response_id {
        send_response_message(window, response_id, json!({ "kind": "value", "payload": value }))?;
    }
    Ok(())
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
    if matches!(channel, "download-bs-version" | "auto-download-bs-version" | "download-bs-version-qr" | "bs-steam-download") {
        // The Electron service stopped its current DepotDownloader before starting a replacement.
        steam::stop_download(app);
    }
    let version_scan = channel == "bs-version.installed-versions";
    let map_scan = channel == "load-version-maps";
    let mutates_library = matches!(channel,
        "import-version" | "download-bs-version" | "auto-download-bs-version" | "download-bs-version-qr" | "bs-steam-download" |
        "bs-oculus-download" | "bs-version.edit" | "bs-version.clone" | "bs.uninstall" |
        "delete-maps" | "bs-maps.import-maps" | "bs-maps.download-map" | "one-click-install-map" |
        "install-playlist-file" | "import-playlists" | "delete-playlist" | "download-playlist" | "one-click-install-playlist" |
        "bs-mods.import-mods" | "bs-mods.install-mods" | "bs-mods.uninstall-mods" | "bs-mods.uninstall-all-mods" |
        "download-model" | "one-click-install-model" | "delete-models" |
        "link-version-folder-action" | "relink-all-versions-folders"
    ) || (channel == "bs-installer.set-install-path" && args.get("move").and_then(Value::as_bool) == Some(true));
    let mut parallel_steam_download = false;
    let mut version_scan_writeback = version_scan;
    let mut map_scan_writeback = map_scan;
    if app.config().identifier == "io.bsmanager.bsmanager.preview" && (mutates_library || version_scan || map_scan) {
        let legacy_running = windows_integration::legacy_manager_running().await?;
        parallel_steam_download = legacy_running
            && matches!(channel, "download-bs-version" | "auto-download-bs-version" | "download-bs-version-qr" | "bs-steam-download")
            && args.get("isVerification").and_then(Value::as_bool) != Some(true);
        if legacy_running && mutates_library && !parallel_steam_download {
            return Err(serialized_error(
                "Close legacy BSManager before changing shared game or content folders",
                "LEGACY_MANAGER_RUNNING",
            ));
        }
        // Version discovery also reconciles config.cfg and metadata.config. Keep it read-only while Electron owns the library.
        if legacy_running && version_scan {
            version_scan_writeback = false;
        }
        // Map discovery writes metadata.json into custom-song folders and can create the scanned folder.
        if legacy_running && map_scan {
            map_scan_writeback = false;
        }
    }
    let writes_library = mutates_library || version_scan_writeback || map_scan_writeback;
    let _operation_locks = if writes_library {
        let mut roots = vec![install_directory(app)
            .map_err(|error| serialized_error(error, "INSTALL_PATH_FAILED"))?];
        if channel == "bs-installer.set-install-path" {
            let path = args.get("path").and_then(Value::as_str)
                .filter(|path| !path.trim().is_empty())
                .ok_or_else(|| serialized_error("Install path is required", "INVALID_INSTALL_PATH"))?;
            let selected = library::canonicalize_existing_directory(Path::new(path.trim()))
                .map_err(|error| serialized_error(error, "INVALID_FOLDER"))?;
            roots.push(normalize_install_parent(selected).join(INSTALL_FOLDER));
        }
        let lock_cancelled = cancelled.clone();
        let locks = tokio::task::spawn_blocking(move || {
            library::acquire_operation_locks(&roots, lock_cancelled)
        }).await.map_err(|error| serialized_error(error.to_string(), "LIBRARY_LOCK_FAILED"))?
            .map_err(|error| if error.kind() == std::io::ErrorKind::Interrupted {
                serialized_error("Library operation was cancelled while waiting for another instance", "REQUEST_CANCELLED")
            } else {
                serialized_error(error.to_string(), "LIBRARY_LOCK_FAILED")
            })?;
        Some(locks)
    } else {
        None
    };

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
            steam::download(app, window, response_channel, &args, cancelled, qr, automatic, parallel_steam_download).await
        }
        "bs-steam-download" => {
            if window.label() != "main" {
                return Err(serialized_error("Steam downloads are only available in the main window", "WINDOW_NOT_ALLOWED"));
            }
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            steam::download_v2(app, window, response_channel, &args, cancelled, parallel_steam_download).await
        }
        "bs-steam-download-respond" => {
            if window.label() != "main" {
                return Err(serialized_error("Steam session controls are only available in the main window", "WINDOW_NOT_ALLOWED"));
            }
            Ok(steam::respond(app, &args))
        }
        "bs-steam-stop-download" => {
            if window.label() != "main" {
                return Err(serialized_error("Steam session controls are only available in the main window", "WINDOW_NOT_ALLOWED"));
            }
            Ok(steam::stop_download(app))
        }
        "steam-session-exists" => {
            if window.label() != "main" {
                return Err(serialized_error("Steam session controls are only available in the main window", "WINDOW_NOT_ALLOWED"));
            }
            Ok(json!(steam::session_exists(app).await?))
        }
        "steam-session-username" => {
            if window.label() != "main" {
                return Err(serialized_error("Steam session controls are only available in the main window", "WINDOW_NOT_ALLOWED"));
            }
            Ok(steam::session_username(app).await?.map(Value::String).unwrap_or(Value::Null))
        }
        "import-legacy-steam-username" => {
            if window.label() != "main" {
                return Err(serialized_error("Steam session controls are only available in the main window", "WINDOW_NOT_ALLOWED"));
            }
            let username = args.as_str().ok_or_else(|| serialized_error("Steam username is required", "INVALID_SESSION"))?;
            steam::import_legacy_username(app, username).await?;
            Ok(Value::Null)
        }
        "stop-download-bs-version" => Ok(steam::stop_download(app)),
        "send-input-bs-download" => Ok(steam::send_input(app, &args)),
        "delete-steam-session" => {
            if window.label() != "main" {
                return Err(serialized_error("Steam session controls are only available in the main window", "WINDOW_NOT_ALLOWED"));
            }
            steam::delete_session(app).await?;
            Ok(Value::Null)
        }
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
        "ms-get-model-by-id" => Ok(models::get_model_by_id(&args).await),
        "search-models" => models::search_models(&args).await,
        "bsv-search-map" | "bsv-get-map-details-from-hashs" | "bsv-get-map-details-by-id" |
        "bsv-search-playlist" | "bsv-get-playlist-details-by-id" => beat_saver::dispatch(channel, &args).await,
        "get-maps-info-from-cache" => beat_saver::get_map_details_from_cache(app, &args).await,
        "load-version-maps" => {
            if map_scan_writeback {
                let app_state = app.state::<AppState>();
                let _operation = app_state.library_operation.lock().await;
                maps::load_version_maps(app, window, response_channel, &args, true, cancelled).await
            } else {
                maps::load_version_maps(app, window, response_channel, &args, false, cancelled).await
            }
        }
        "delete-maps" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            maps::delete_maps(app, window, response_channel, &args, cancelled).await
        }
        "export-maps" => maps::export_maps(app, window, response_channel, &args, cancelled).await,
        "bs-maps.import-maps" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            maps::import_maps(app, window, response_channel, &args, cancelled).await
        }
        "get-version-playlists-details" => playlists::get_version_playlists(app, window, response_channel, &args, cancelled).await,
        "install-playlist-file" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            playlists::install_playlist(app, &args)
        }
        "import-playlists" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            playlists::import_playlists(app, window, response_channel, &args, cancelled).await
        }
        "delete-playlist" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            playlists::delete_playlist(app, window, response_channel, &args, cancelled).await
        }
        "export-playlists" => playlists::export_playlists(app, window, response_channel, &args, cancelled).await,
        "download-playlist" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            playlists::download_playlist(app, window, response_channel, &args, cancelled).await
        }
        "one-click-install-playlist" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            playlists::one_click_install_playlist(app, window, response_channel, &args, cancelled).await
        }
        "bs-mods.get-available-mods" | "bs-mods.get-installed-mods" | "bs-mods.beatmods-up" => {
            mods::handle(app, window, channel, &args, response_channel, cancelled).await
        }
        "bs-mods.import-mods" | "bs-mods.install-mods" | "bs-mods.uninstall-mods" | "bs-mods.uninstall-all-mods" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            mods::handle(app, window, channel, &args, response_channel, cancelled).await
        }
        "bs-maps.download-map" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            maps::download_map(app, &args, cancelled).await
        }
        "one-click-install-map" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            maps::one_click_install_map(app, &args, cancelled).await
        }
        "last-downloaded-map" => maps::stream_last_downloaded(window, response_channel, cancelled).await,
        "download-model" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            models::download_model(app, window, response_channel, &args, cancelled).await
        }
        "one-click-install-model" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            models::one_click_install_model(app, &args, cancelled).await
        }
        "get-version-models" => models::get_version_models(app, window, response_channel, &args, cancelled).await,
        "export-models" => models::export_models(app, window, response_channel, &args, cancelled).await,
        "delete-models" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            models::delete_models(app, window, response_channel, &args, cancelled).await
        }
        "is-oculus-sideloaded-apps-enabled" => windows_integration::sideloaded_apps_enabled(),
        "enable-oculus-sideloaded-apps" => windows_integration::enable_sideloaded_apps(app).await,
        "vr-runtime.get-active" => Ok(windows_integration::active_vr_runtime(args.as_str())),
        "shortcut-launch-options" => shortcut_launch_options(app, window),
        "current-version" => Ok(json!(env!("CARGO_PKG_VERSION"))),
        "get-available-update" => {
            let state = app.state::<AppState>();
            let _operation = state.update_operation.lock().await;
            let update = check_for_update(app).await?;
            set_pending_update(app, update.clone()).await;
            Ok(update.map(|update| json!({ "version": update.version })).unwrap_or(Value::Null))
        }
        "check-update" => {
            let state = app.state::<AppState>();
            let _operation = state.update_operation.lock().await;
            let update = check_for_update(app).await?;
            set_pending_update(app, update.clone()).await;
            Ok(json!(update.is_some()))
        }
        "download-update" => {
            let state = app.state::<AppState>();
            let _operation = state.update_operation.lock().await;
            download_update(app, window, response_channel, cancelled).await
        }
        "install-update" => {
            let state = app.state::<AppState>();
            let _operation = state.update_operation.lock().await;
            install_update(app).await
        }
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
                let cancelled_for_copy = cancelled.clone();
                let copied = tokio::task::spawn_blocking(move || library::copy_library(&source, &destination, &cancelled_for_copy))
                    .await
                    .map_err(|error| serialized_error(format!("Library move worker failed: {error}"), "LIBRARY_MOVE_FAILED"))?
                    .map_err(|error| match error {
                        library::LibraryError::CopyToSubpath => serialized_error(
                            "The destination cannot be inside or above the current library",
                            "COPY_TO_SUBPATH",
                        ),
                        library::LibraryError::Io(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                            serialized_error("Library move was cancelled", "REQUEST_CANCELLED")
                        }
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
            let source = legacy_local_storage_path(app).map_err(|error| serialized_error(error, "SETTINGS_IMPORT_FAILED"))?;
            let cache = app.path().app_cache_dir().map_err(|error| serialized_error(error.to_string(), "SETTINGS_IMPORT_FAILED"))?;
            let renderer_settings = tokio::task::spawn_blocking(move || legacy_storage::read_preferences(&source, &cache))
                .await
                .map_err(|error| serialized_error(format!("Legacy preference reader failed: {error}"), "SETTINGS_IMPORT_FAILED"))?
                .map_err(|error| serialized_error(error, "SETTINGS_IMPORT_FAILED"))?;
            import_legacy_settings(app, true).map_err(|error| serialized_error(error, "SETTINGS_IMPORT_FAILED"))?;
            log::info!("Imported {} non-secret legacy renderer preferences", renderer_settings.len());
            Ok(json!(renderer_settings))
        }
        "preview.complete-renderer-settings-import" => {
            if window.label() != "main" {
                return Err(serialized_error("Legacy settings import is only available in the main window", "WINDOW_NOT_ALLOWED"));
            }
            complete_legacy_renderer_settings_import(app)
                .map_err(|error| serialized_error(error, "SETTINGS_IMPORT_FAILED"))?;
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
            let use_system_proxy = value.as_bool().unwrap_or(false);
            let mut settings = read_settings(app).map_err(|error| serialized_error(error, "SETTINGS_READ_FAILED"))?;
            settings.insert(key.to_owned(), value);
            write_settings(app, &settings).map_err(|error| serialized_error(error, "SETTINGS_WRITE_FAILED"))?;
            if key == "use-system-proxy" { proxy::set_enabled(use_system_proxy); }
            Ok(Value::Null)
        }
        "static-configuration.delete" => {
            let key = args.as_str().ok_or_else(|| serialized_error("Setting key is required", "INVALID_SETTING"))?;
            let mut settings = read_settings(app).map_err(|error| serialized_error(error, "SETTINGS_READ_FAILED"))?;
            settings.remove(key);
            write_settings(app, &settings).map_err(|error| serialized_error(error, "SETTINGS_WRITE_FAILED"))?;
            if key == "use-system-proxy" { proxy::set_enabled(false); }
            Ok(Value::Null)
        }
        "bs-version.get-version-dict" => versions::get_version_catalog(app, window, &args, response_channel, cancelled).await,
        "is-preview-build" => Ok(json!(app.config().identifier == "io.bsmanager.bsmanager.preview")),
        "bs-version.installed-versions" => {
            let app_state = app.state::<AppState>();
            let _operation = app_state.library_operation.lock().await;
            let app = app.clone();
            tokio::task::spawn_blocking(move || {
                let installed = versions::get_installed_versions(&app, version_scan_writeback)?;
                if let Err(error) = liv::sync(&app, &installed) {
                    log::warn!("Could not synchronize LIV shortcuts: {error}");
                }
                Ok(installed)
            })
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
            versions::clone_version(app, &args, cancelled).await
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
        "register-maps-deep-link" => set_protocols(app, window, &["beatsaver", "web+bsmap"], true),
        "unregister-maps-deep-link" => set_protocols(app, window, &["beatsaver", "web+bsmap"], false),
        "is-map-deep-links-enabled" => are_protocols_registered(app, window, &["beatsaver", "web+bsmap"]),
        "register-playlists-deep-link" => set_playlist_handlers(app, window, true),
        "unregister-playlists-deep-link" => set_playlist_handlers(app, window, false),
        "is-playlists-deep-links-enabled" => are_playlist_handlers_registered(app, window),
        "register-models-deep-link" => set_protocols(app, window, &["modelsaber"], true),
        "unregister-models-deep-link" => set_protocols(app, window, &["modelsaber"], false),
        "is-models-deep-links-enabled" => are_protocols_registered(app, window, &["modelsaber"]),
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
            create_window(app, template, None).map_err(|error| serialized_error(error, "WINDOW_FAILED"))?;
            if channel == "open-window-then-close-all" {
                if let Some((target_label, _, _, _, _, _, _)) = resolve_window(template) {
                    for (label, _, _, _, _, _, _) in [
                        resolve_window("index.html"), resolve_window("launcher.html"),
                        resolve_window("oneclick-download-map.html"), resolve_window("oneclick-download-playlist.html"),
                        resolve_window("oneclick-download-model.html"), resolve_window("shortcut-launch.html"),
                    ].into_iter().flatten() {
                        if label != target_label {
                            if let Some(other) = app.get_webview_window(label) { let _ = other.close(); }
                        }
                    }
                }
            }
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
        "window.progression" => {
            let progression = args.as_f64()
                .filter(|progression| progression.is_finite())
                .ok_or_else(|| serialized_error("Taskbar progress must be a number", "INVALID_PROGRESS"))?;
            let progress = progression.clamp(0.0, 100.0).round() as u64;
            window.set_progress_bar(ProgressBarState {
                status: Some(ProgressBarStatus::Normal),
                progress: Some(progress),
            }).map_err(|error| serialized_error(error.to_string(), "TASKBAR_PROGRESS_FAILED"))?;
            Ok(Value::Null)
        }
        "restart-app" => {
            if window.label() != "main" {
                return Err(serialized_error("Restart is only available in the main window", "WINDOW_NOT_ALLOWED"));
            }
            Ok(Value::Null)
        }
        "notify-system" => {
            let title = args.get("title").and_then(Value::as_str).unwrap_or("BSManager");
            let body = args.get("body").and_then(Value::as_str).unwrap_or("");
            app.notification().builder().title(title).body(body)
                .show().map_err(|error| serialized_error(error.to_string(), "NOTIFICATION_FAILED"))?;
            Ok(Value::Null)
        }
        "log-error" => { log::error!("Renderer error: {args}"); Ok(Value::Null) }
        "add-filter-string" => {
            let secret = args.as_str().ok_or_else(|| serialized_error("Log redaction value must be a string", "INVALID_LOG_FILTER"))?;
            app.state::<AppState>().log_redactions.lock()
                .map_err(|_| serialized_error("Log filters are unavailable", "LOG_FILTER_FAILED"))?
                .add_string(secret)?;
            Ok(Value::Null)
        }
        "add-filter-pattern" => {
            let source = args.as_str().ok_or_else(|| serialized_error("Log redaction pattern must be a string", "INVALID_LOG_FILTER"))?;
            app.state::<AppState>().log_redactions.lock()
                .map_err(|_| serialized_error("Log filters are unavailable", "LOG_FILTER_FAILED"))?
                .add_pattern(source)?;
            Ok(Value::Null)
        }
        "get-window-args" => {
            let label = window.label().to_owned();
            let state = app.state::<AppState>();
            let args = state.window_arguments.lock().map_err(|_| serialized_error("Window state is unavailable", "WINDOW_FAILED"))?
                .get(&label).cloned().unwrap_or_else(|| json!({}));
            Ok(args)
        }
        _ => Err(serialized_error(format!("This Windows Tauri app does not implement '{channel}'"), "NOT_IMPLEMENTED")),
    }
}

async fn run_ipc_route<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    channel: String,
    args: Value,
    response_id: Option<String>,
    cancelled: Arc<AtomicBool>,
) {
    let result = handle_request(
        app,
        window,
        &channel,
        args,
        response_id.as_deref(),
        cancelled,
    ).await;
    let should_restart = channel == "restart-app" && result.is_ok();
    if response_id.is_none() {
        if let Err(error) = &result {
            log::warn!("Windows Tauri one-way route '{channel}' failed: {}", error.message);
        }
    }
    ipc_response(&window, &channel, response_id.as_deref(), result, matches!(channel.as_str(), "import-version" | "download-bs-version" | "auto-download-bs-version" | "download-bs-version-qr" | "bs-steam-download" | "bs-oculus-download" | "bs-launch.launch" | "download-model" | "get-version-models" | "export-models" | "delete-models" | "load-version-maps" | "delete-maps" | "export-maps" | "bs-maps.import-maps" | "get-version-playlists-details" | "import-playlists" | "delete-playlist" | "export-playlists" | "download-playlist" | "one-click-install-playlist" | "bs-mods.import-mods" | "bs-mods.install-mods" | "bs-mods.uninstall-mods" | "bs-mods.uninstall-all-mods" | "download-update" | "last-downloaded-map" | "bs-version.get-version-dict"));
    if should_restart { app.request_restart(); }
}

#[tauri::command]
fn request<R: Runtime>(
    app: AppHandle<R>,
    window: WebviewWindow<R>,
    request_id: String,
    channel: String,
    args: Option<Value>,
    on_event: Channel<Value>,
) -> Result<(), String> {
    if !is_application_window(window.label()) {
        return Err(format!("Rejected IPC route from window '{}'", window.label()));
    }
    if request_id.trim().is_empty() || request_id.len() > 128 {
        return Err("Invalid request identifier".to_owned());
    }

    let cancelled = Arc::new(AtomicBool::new(false));
    let state = app.state::<AppState>();
    {
        let mut requests = state.active_requests.lock().map_err(|_| "Active requests are unavailable".to_owned())?;
        if requests.contains_key(&request_id) {
            return Err("Request identifier is already active".to_owned());
        }
        let mut responses = state.response_channels.lock().map_err(|_| "IPC response channels are unavailable".to_owned())?;
        if responses.contains_key(&request_id) {
            return Err("Request identifier is already active".to_owned());
        }
        requests.insert(request_id.clone(), ActiveRequest {
            window_label: window.label().to_owned(),
            cancelled: cancelled.clone(),
        });
        responses.insert(request_id.clone(), on_event);
    }

    let task_app = app.clone();
    let task_window = window.clone();
    let cleanup_id = request_id.clone();
    tauri::async_runtime::spawn(async move {
        run_ipc_route(&task_app, &task_window, channel, args.unwrap_or(Value::Null), Some(request_id), cancelled).await;
        let state = task_app.state::<AppState>();
        if let Ok(mut requests) = state.active_requests.lock() { requests.remove(&cleanup_id); }
        if let Ok(mut responses) = state.response_channels.lock() { responses.remove(&cleanup_id); }
    });
    Ok(())
}

#[tauri::command]
async fn send_lazy<R: Runtime>(app: AppHandle<R>, window: WebviewWindow<R>, channel: String, args: Option<Value>) {
    if !is_application_window(window.label()) {
        log::warn!("Rejected one-way IPC route '{channel}' from window '{}'", window.label());
        return;
    }
    run_ipc_route(&app, &window, channel, args.unwrap_or(Value::Null), None, Arc::new(AtomicBool::new(false))).await;
}

#[tauri::command]
fn cancel_request<R: Runtime>(app: AppHandle<R>, window: WebviewWindow<R>, request_id: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut requests = state.active_requests.lock().map_err(|_| "Active requests are unavailable".to_owned())?;
    if requests.get(&request_id).is_some_and(|request| request.window_label == window.label()) {
        if let Some(request) = requests.remove(&request_id) { request.cancelled.store(true, Ordering::Relaxed); }
        drop(requests);
        state.response_channels.lock().map_err(|_| "IPC response channels are unavailable".to_owned())?.remove(&request_id);
    } else if requests.contains_key(&request_id) {
        return Err("Request cancellation came from a different window".to_owned());
    }
    Ok(())
}

fn is_application_window(label: &str) -> bool {
    matches!(label, "main" | "launcher" | "oneclick-download-map" | "oneclick-download-playlist" | "oneclick-download-model" | "shortcut-launch")
}

fn first_launch_argument(arguments: &[String]) -> Option<&str> {
    arguments.iter()
        .find(|argument| url::Url::parse(argument).is_ok_and(|url| {
            ["bsmanager", "bsmanager-preview", "beatsaver", "web+bsmap", "bsplaylist", "modelsaber"]
                .contains(&url.scheme())
        }))
        .or_else(|| arguments.iter().find(|argument| {
            Path::new(argument).extension().is_some_and(|extension| extension.eq_ignore_ascii_case("bplist"))
        }))
        .map(String::as_str)
}

fn handle_launch_argument<R: Runtime>(app: &AppHandle<R>, argument: &str) -> bool {
    if let Ok(url) = url::Url::parse(argument) {
        if ["bsmanager", "bsmanager-preview", "beatsaver", "web+bsmap", "bsplaylist", "modelsaber"].contains(&url.scheme()) {
            let mut args = Map::new();
            let template = match url.scheme() {
                "bsmanager" | "bsmanager-preview" if url.host_str() == Some("launch") => {
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
                return match create_window(app, template, Some(Value::Object(args))) {
                    Ok(()) => true,
                    Err(error) => { log::warn!("Could not open deep-link window: {error}"); false }
                };
            } else {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                    return true;
                }
                return create_window(app, "index.html", None).is_ok();
            }
        }
    } else if Path::new(argument).extension().is_some_and(|extension| extension.eq_ignore_ascii_case("bplist")) {
        let args = json!({ "playlistUrl": argument });
        return match create_window(app, "oneclick-download-playlist.html", Some(args)) {
            Ok(()) => true,
            Err(error) => { log::warn!("Could not open playlist file: {error}"); false }
        };
    }
    false
}

fn should_open_update_launcher<R: Runtime>(app: &AppHandle<R>) -> bool {
    let mut settings = match read_settings(app) {
        Ok(settings) => settings,
        Err(error) => { log::warn!("Could not read auto-update setting: {error}"); return false }
    };
    let mode = settings.get("auto-update").and_then(Value::as_str).unwrap_or("always").to_owned();
    if mode == "once" {
        settings.insert("auto-update".to_owned(), json!("never"));
        if let Err(error) = write_settings(app, &settings) {
            log::warn!("Could not clear one-time auto-update setting: {error}");
        }
    }
    if mode == "never" { return false; }
    match configured_update_source(app) {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(error) => { log::warn!("Tauri auto-update is unavailable: {}", error.message); false }
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

fn set_protocols<R: Runtime>(app: &AppHandle<R>, window: &WebviewWindow<R>, protocols: &[&str], register: bool) -> Result<Value, SerializedError> {
    if window.label() != "main" {
        return Err(serialized_error("Protocol settings are only available in the main window", "WINDOW_NOT_ALLOWED"));
    }
    for protocol in protocols {
        protocols::set(app, protocol, register)
            .map_err(|error| serialized_error(error, "PROTOCOL_REGISTRATION_FAILED"))?;
    }
    Ok(json!(true))
}

fn are_protocols_registered<R: Runtime>(app: &AppHandle<R>, window: &WebviewWindow<R>, protocols: &[&str]) -> Result<Value, SerializedError> {
    if window.label() != "main" {
        return Err(serialized_error("Protocol settings are only available in the main window", "WINDOW_NOT_ALLOWED"));
    }
    let mut registered = true;
    for protocol in protocols {
        registered &= protocols::is_registered(app, protocol)
            .map_err(|error| serialized_error(error, "PROTOCOL_QUERY_FAILED"))?;
    }
    Ok(json!(registered))
}

fn set_playlist_handlers<R: Runtime>(app: &AppHandle<R>, window: &WebviewWindow<R>, enabled: bool) -> Result<Value, SerializedError> {
    if window.label() != "main" {
        return Err(serialized_error("Playlist associations are only available in the main window", "WINDOW_NOT_ALLOWED"));
    }
    protocols::set(app, "bsplaylist", enabled)
        .map_err(|error| serialized_error(error, "PROTOCOL_REGISTRATION_FAILED"))?;
    if let Err(error) = protocols::set_bplist_association(app, enabled) {
        let _ = protocols::set(app, "bsplaylist", !enabled);
        return Err(serialized_error(error, "FILE_ASSOCIATION_FAILED"));
    }
    Ok(json!(true))
}

fn are_playlist_handlers_registered<R: Runtime>(app: &AppHandle<R>, window: &WebviewWindow<R>) -> Result<Value, SerializedError> {
    if window.label() != "main" {
        return Err(serialized_error("Playlist association settings are only available in the main window", "WINDOW_NOT_ALLOWED"));
    }
    let protocol = protocols::is_registered(app, "bsplaylist")
        .map_err(|error| serialized_error(error, "PROTOCOL_QUERY_FAILED"))?;
    let file_association = protocols::is_bplist_associated(app)
        .map_err(|error| serialized_error(error, "FILE_ASSOCIATION_QUERY_FAILED"))?;
    Ok(json!(protocol && file_association))
}

fn install_startup_panic_hook(identifier: &str, redactions: Arc<Mutex<LogRedactions>>) {
    let Some(local_app_data) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) else { return };
    let log_path = local_app_data.join(identifier).join("logs").join("startup-panic.log");
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let message = panic_info.to_string();
        let message = match redactions.try_lock() {
            Ok(filters) => filters.redact(&message),
            Err(_) => "Panic details suppressed because log redaction state is unavailable".to_owned(),
        };
        if let Some(directory) = log_path.parent() {
            let _ = fs::create_dir_all(directory);
        }
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&log_path) {
            let _ = writeln!(file, "[{}] {message}", chrono::Utc::now().to_rfc3339());
        }
        previous_hook(panic_info);
    }));
}

pub fn run() {
    let log_redactions = Arc::new(Mutex::new(LogRedactions::with_default_patterns()));
    let context = tauri::generate_context!();
    install_startup_panic_hook(&context.config().identifier, Arc::clone(&log_redactions));
    let logger_redactions = Arc::clone(&log_redactions);
    let logger = tauri_plugin_log::Builder::new()
        .level(log::LevelFilter::Info)
        .format(move |out, message, record| {
            let message = logger_redactions.lock()
                .map(|filters| filters.redact(&message.to_string()))
                .unwrap_or_else(|_| message.to_string());
            out.finish(format_args!(
                "[{}][{}][{}] {}",
                chrono::Utc::now().format("%Y-%m-%d][%H:%M:%S"),
                record.target(),
                record.level(),
                message
            ));
        })
        .build();
    tauri::Builder::default()
        .manage(AppState { log_redactions, ..AppState::default() })
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if let Some(argument) = first_launch_argument(&args) {
                handle_launch_argument(app, argument);
            }
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(logger)
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_window_state::Builder::default().skip_initial_state("main").build())
        .invoke_handler(tauri::generate_handler![request, send_lazy, cancel_request])
        .setup(|app| {
            log::info!("Starting application setup for {}", app.config().identifier);
            if let Err(error) = proxy::configure(app.handle()) {
                log::warn!("Could not read the system proxy setting: {error}");
            }
            let launch_protocol = shortcuts::launch_protocol(app.handle());
            match protocols::is_registered(app.handle(), launch_protocol) {
                Ok(true) => {}
                Ok(false) => {
                    if let Err(error) = protocols::set(app.handle(), launch_protocol, true) {
                        log::warn!("Could not register BSManager launch links: {error}");
                    }
                }
                Err(error) => log::warn!("Could not inspect BSManager launch link registration: {error}"),
            }
            map_info_cache::initialize(app.handle());
            let cache_app = app.handle().clone();
            tauri::async_runtime::spawn(async move { song_cache::initialize(&cache_app).await; });
            let arguments = std::env::args_os().skip(1).filter_map(|argument| argument.into_string().ok()).collect::<Vec<_>>();
            if let Some(argument) = first_launch_argument(&arguments)
                && handle_launch_argument(app.handle(), argument) {
                return Ok(());
            }
            let startup_window = if should_open_update_launcher(app.handle()) { "launcher.html" } else { "index.html" };
            log::info!("Opening startup window '{startup_window}'");
            if let Err(error) = create_window(app.handle(), startup_window, None) {
                log::error!("Could not open startup window '{startup_window}': {error}");
                create_window(app.handle(), "index.html", None)
                    .map_err(|_| std::io::Error::other("Could not open fallback startup window"))?;
            }
            log::info!("Startup window is ready");
            Ok(())
        })
        .run(context)
        .expect("BSManager Tauri failed to start");
}
