use std::{
    collections::HashMap,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, atomic::{AtomicBool, Ordering}},
    time::Duration,
};

use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager, Runtime, WebviewWindow};
use tokio::io::AsyncWriteExt;
use url::Url;
use zip::{CompressionMethod, write::{SimpleFileOptions, ZipWriter}};

use crate::{SerializedError, linker, serialized_error, versions};

const MODEL_SABER_API: &str = "https://modelsaber.com/api/v2/get.php";
static MODEL_HASH_CACHE: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();

pub async fn export_models<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    args: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let output = args.get("outPath").and_then(Value::as_str).filter(|path| !path.trim().is_empty())
        .ok_or_else(|| serialized_error("Output path is required", "INVALID_PATH"))?;
    let output = PathBuf::from(output);
    let version = args.get("version").filter(|version| !version.is_null());
    let mut files = Vec::<(PathBuf, String)>::new();
    let selected = args.get("models").and_then(Value::as_array).filter(|models| !models.is_empty());
    if let Some(selected) = selected {
        for model in selected {
            let kind = model_type(model)?;
            let model_version = model.get("version").filter(|value| !value.is_null()).or(version);
            let root = model_folder(app, kind, model_version)?;
            let path = model.get("path").and_then(Value::as_str)
                .ok_or_else(|| serialized_error("Model path is required", "INVALID_PATH"))?;
            let path = resolve_model_file(&root, Path::new(path), kind)?;
            let name = Path::new(model_folder_name(kind)?).join(path.file_name().unwrap_or_default());
            files.push((path, name.to_string_lossy().replace('\\', "/")));
        }
    } else {
        for kind in ["avatar", "bloq", "platform", "saber"] {
            let root = model_folder(app, kind, version)?;
            tokio::fs::create_dir_all(&root).await
                .map_err(|error| serialized_error(error.to_string(), "MODEL_PATH_FAILED"))?;
            let scan_root = root.clone();
            let kind = kind.to_owned();
            let mut found = tokio::task::spawn_blocking(move || collect_model_files(&scan_root))
                .await.map_err(|error| serialized_error(error.to_string(), "MODEL_EXPORT_FAILED"))?
                .map_err(|error| serialized_error(error.to_string(), "MODEL_EXPORT_FAILED"))?;
            for path in found.drain(..) {
                let relative = path.strip_prefix(&root).unwrap_or(&path);
                let name = Path::new(model_folder_name(&kind)?).join(relative);
                files.push((path, name.to_string_lossy().replace('\\', "/")));
            }
        }
    }
    if files.iter().any(|(path, _)| path == &output) {
        return Err(serialized_error("The archive output cannot replace a model file", "INVALID_PATH"));
    }
    let total = files.len() as u64;
    let output_text = output.to_string_lossy().to_string();
    emit_model_progress(window, response_channel, 0, total, Some(json!(output_text)))?;
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent).await
            .map_err(|error| serialized_error(error.to_string(), "MODEL_EXPORT_FAILED"))?;
    }
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<u64>();
    let worker_cancelled = cancelled.clone();
    let worker_output = output.clone();
    let worker = tokio::task::spawn_blocking(move || write_model_archive(&worker_output, files, worker_cancelled, sender));
    let mut progress_error = None;
    while let Some(current) = receiver.recv().await {
        if let Err(error) = emit_model_progress(window, response_channel, current, total, Some(json!(output_text))) {
            cancelled.store(true, Ordering::Relaxed);
            progress_error = Some(error);
        }
    }
    let archived = worker.await
        .map_err(|error| serialized_error(error.to_string(), "MODEL_EXPORT_FAILED"))?;
    if let Some(error) = progress_error { return Err(error); }
    archived.map_err(|error| serialized_error(error, "MODEL_EXPORT_FAILED"))?;
    Ok(Value::Null)
}

pub async fn delete_models<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    args: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let models = args.as_array()
        .ok_or_else(|| serialized_error("Models must be an array", "INVALID_MODELS"))?;
    let total = models.len() as u64;
    let mut deleted = Vec::with_capacity(models.len());
    emit_model_progress(window, response_channel, 0, total, Some(json!(deleted)))?;
    for (index, model) in models.iter().enumerate() {
        if cancelled.load(Ordering::Relaxed) {
            return Err(serialized_error("Model deletion was cancelled", "REQUEST_CANCELLED"));
        }
        let kind = model_type(model)?;
        let version = model.get("version").filter(|value| !value.is_null());
        let root = model_folder(app, kind, version)?;
        let path = model.get("path").and_then(Value::as_str)
            .ok_or_else(|| serialized_error("Model path is required", "INVALID_PATH"))?;
        let path = resolve_model_file(&root, Path::new(path), kind)?;
        tokio::fs::remove_file(&path).await
            .map_err(|error| serialized_error(error.to_string(), "MODEL_DELETE_FAILED"))?;
        deleted.push(model.clone());
        emit_model_progress(window, response_channel, index as u64 + 1, total, Some(json!(deleted)))?;
    }
    Ok(Value::Null)
}

fn write_model_archive(
    output: &Path,
    files: Vec<(PathBuf, String)>,
    cancelled: Arc<AtomicBool>,
    progress: tokio::sync::mpsc::UnboundedSender<u64>,
) -> Result<(), String> {
    let result = (|| {
        let mut archive = ZipWriter::new(File::create(output).map_err(|error| error.to_string())?);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (index, (path, name)) in files.into_iter().enumerate() {
            if cancelled.load(Ordering::Relaxed) {
                return Err("Model export was cancelled".to_owned());
            }
            archive.start_file(name, options).map_err(|error| error.to_string())?;
            let mut input = File::open(path).map_err(|error| error.to_string())?;
            std::io::copy(&mut input, &mut archive).map_err(|error| error.to_string())?;
            let _ = progress.send(index as u64 + 1);
        }
        archive.finish().map_err(|error| error.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(output);
    }
    result
}

fn collect_model_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_model_files_recursive(root, &mut files)?;
    Ok(files)
}

fn collect_model_files_recursive(directory: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_model_files_recursive(&path, files)?;
        } else if kind.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn resolve_model_file(root: &Path, path: &Path, kind: &str) -> Result<PathBuf, SerializedError> {
    let root = fs::canonicalize(root).map_err(|error| serialized_error(error.to_string(), "INVALID_PATH"))?;
    let path = fs::canonicalize(path).map_err(|error| serialized_error(error.to_string(), "INVALID_PATH"))?;
    if !path.starts_with(&root) || !path.is_file() {
        return Err(serialized_error("Model path is outside its content folder", "INVALID_PATH"));
    }
    if path.extension().and_then(|extension| extension.to_str()) != Some(model_extension(kind)?.trim_start_matches('.')) {
        return Err(serialized_error("Model file extension does not match its type", "INVALID_MODEL"));
    }
    Ok(path)
}

pub async fn get_version_models<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    args: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let kind = args.get("type").and_then(Value::as_str)
        .filter(|kind| matches!(*kind, "avatar" | "bloq" | "platform" | "saber"))
        .ok_or_else(|| serialized_error("Model type is invalid", "INVALID_MODEL"))?;
    let version = args.get("version").filter(|version| !version.is_null());
    let folder = model_folder(app, kind, version)?;
    tokio::fs::create_dir_all(&folder).await
        .map_err(|error| serialized_error(error.to_string(), "MODEL_PATH_FAILED"))?;
    let mut entries = tokio::fs::read_dir(&folder).await
        .map_err(|error| serialized_error(error.to_string(), "MODEL_PATH_FAILED"))?;
    let extension = model_extension(kind)?;
    let mut paths = Vec::new();
    while let Some(entry) = entries.next_entry().await
        .map_err(|error| serialized_error(error.to_string(), "MODEL_PATH_FAILED"))? {
        let file_type = entry.file_type().await
            .map_err(|error| serialized_error(error.to_string(), "MODEL_PATH_FAILED"))?;
        if file_type.is_file() && entry.path().extension().and_then(|ext| ext.to_str()) == Some(extension.trim_start_matches('.')) {
            paths.push(entry.path());
        }
    }
    let total = paths.len() as u64;
    emit_model_progress(window, response_channel, 0, total, None)?;
    let mut models = Vec::with_capacity(paths.len());
    for (index, path) in paths.into_iter().enumerate() {
        if cancelled.load(Ordering::Relaxed) {
            return Err(serialized_error("Model listing was cancelled", "REQUEST_CANCELLED"));
        }
        let hash_path = path.clone();
        let hash = tokio::task::spawn_blocking(move || hash_file_md5(&hash_path)).await
            .map_err(|error| serialized_error(error.to_string(), "MODEL_HASH_FAILED"))?
            .map_err(|error| serialized_error(error.to_string(), "MODEL_HASH_FAILED"))?;
        let model = model_by_hash(&hash).await;
        let filename = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        let local_filename = path.file_stem().and_then(|name| name.to_str()).unwrap_or(filename);
        models.push(local_model(&path, local_filename, &hash, kind, model, version));
        emit_model_progress(window, response_channel, index as u64 + 1, total, None)?;
    }
    emit_model_progress(window, response_channel, total, total, Some(json!(models)))?;
    Ok(Value::Null)
}

pub async fn download_model<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    args: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let model = args.get("model")
        .ok_or_else(|| serialized_error("Model data is required", "INVALID_MODEL"))?;
    let version = args.get("version").filter(|version| !version.is_null())
        .ok_or_else(|| serialized_error("Beat Saber version is required", "INVALID_VERSION"))?;
    let downloaded = download_model_file(app, window, response_channel, model, Some(version), &cancelled).await?;
    Ok(downloaded)
}

pub async fn one_click_install_model<R: Runtime>(
    app: &AppHandle<R>,
    model: &Value,
    cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let mut installed = versions::get_installed_versions(app)?;
    let installed = installed.as_array_mut()
        .ok_or_else(|| serialized_error("Installed versions could not be read", "VERSION_SCAN_FAILED"))?;
    let version = installed.pop()
        .ok_or_else(|| serialized_error("No installed Beat Saber versions were found", "NO_INSTALLED_VERSIONS"))?;
    let downloaded = download_model_file(app, &app.get_webview_window("main")
        .or_else(|| app.get_webview_window("oneclick-download-model"))
        .ok_or_else(|| serialized_error("The application window is unavailable", "WINDOW_NOT_FOUND"))?,
        None, model, Some(&version), &cancelled).await?;
    let source = downloaded.get("path").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Downloaded model path is unavailable", "DOWNLOAD_FAILED"))?;
    let filename = Path::new(source).file_name()
        .ok_or_else(|| serialized_error("Downloaded model filename is invalid", "INVALID_MODEL"))?;
    for installed_version in installed {
        if cancelled.load(Ordering::Relaxed) {
            return Err(serialized_error("Model installation was cancelled", "REQUEST_CANCELLED"));
        }
        let destination = model_folder(app, model_type(model)?, Some(installed_version))?.join(filename);
        tokio::fs::create_dir_all(destination.parent().unwrap_or(Path::new("."))).await
            .map_err(|error| serialized_error(error.to_string(), "MODEL_PATH_FAILED"))?;
        tokio::fs::copy(source, &destination).await
            .map_err(|error| serialized_error(error.to_string(), "MODEL_COPY_FAILED"))?;
    }
    Ok(Value::Null)
}

async fn download_model_file<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    model: &Value,
    version: Option<&Value>,
    cancelled: &AtomicBool,
) -> Result<Value, SerializedError> {
    let kind = model_type(model)?;
    let download_url = model.get("download").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Model download URL is missing", "INVALID_MODEL"))?;
    let download_url = Url::parse(download_url)
        .map_err(|error| serialized_error(error.to_string(), "INVALID_MODEL_URL"))?;
    if download_url.scheme() != "https" {
        return Err(serialized_error("Model downloads must use HTTPS", "INVALID_MODEL_URL"));
    }
    let filename = download_url.path_segments().and_then(Iterator::last)
        .map(sanitize_filename).filter(|name| !name.is_empty())
        .ok_or_else(|| serialized_error("Model download filename is invalid", "INVALID_MODEL"))?;
    let destination = model_folder(app, kind, version)?.join(&filename);
    tokio::fs::create_dir_all(destination.parent().unwrap_or(Path::new("."))).await
        .map_err(|error| serialized_error(error.to_string(), "MODEL_PATH_FAILED"))?;
    if cancelled.load(Ordering::Relaxed) {
        return Err(serialized_error("Model download was cancelled", "REQUEST_CANCELLED"));
    }

    let client = Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(1800))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 || attempt.url().scheme() != "https" {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .user_agent("BSManager/1.6.0")
        .build()
        .map_err(|error| serialized_error(error.to_string(), "MODEL_DOWNLOAD_CLIENT_FAILED"))?;
    let response = client.get(download_url).send().await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| serialized_error(error.to_string(), "MODEL_DOWNLOAD_FAILED"))?;
    let total = response.content_length().unwrap_or_default();
    let mut stream = response.bytes_stream();
    let mut output = tokio::fs::File::create(&destination).await
        .map_err(|error| serialized_error(error.to_string(), "MODEL_DOWNLOAD_FAILED"))?;
    let mut hasher = md5::Context::new();
    let mut current = 0_u64;
    if let Err(error) = emit_download_progress(window, response_channel, 0, total, &destination, Value::Null) {
        drop(output);
        let _ = tokio::fs::remove_file(&destination).await;
        return Err(error);
    }
    while let Some(chunk) = stream.next().await {
        if cancelled.load(Ordering::Relaxed) {
            drop(output);
            let _ = tokio::fs::remove_file(&destination).await;
            return Err(serialized_error("Model download was cancelled", "REQUEST_CANCELLED"));
        }
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                drop(output);
                let _ = tokio::fs::remove_file(&destination).await;
                return Err(serialized_error(error.to_string(), "MODEL_DOWNLOAD_FAILED"));
            }
        };
        if let Err(error) = output.write_all(&chunk).await {
            drop(output);
            let _ = tokio::fs::remove_file(&destination).await;
            return Err(serialized_error(error.to_string(), "MODEL_DOWNLOAD_FAILED"));
        }
        hasher.consume(&chunk);
        current = current.saturating_add(chunk.len() as u64);
        if let Err(error) = emit_download_progress(window, response_channel, current, total, &destination, Value::Null) {
            drop(output);
            let _ = tokio::fs::remove_file(&destination).await;
            return Err(error);
        }
    }
    if let Err(error) = output.flush().await {
        drop(output);
        let _ = tokio::fs::remove_file(&destination).await;
        return Err(serialized_error(error.to_string(), "MODEL_DOWNLOAD_FAILED"));
    }
    drop(output);
    if total > 0 && current != total {
        let _ = tokio::fs::remove_file(&destination).await;
        return Err(serialized_error("Model download size did not match the server response", "MODEL_DOWNLOAD_TRUNCATED"));
    }
    if cancelled.load(Ordering::Relaxed) {
        let _ = tokio::fs::remove_file(&destination).await;
        return Err(serialized_error("Model download was cancelled", "REQUEST_CANCELLED"));
    }
    let digest = format!("{:x}", hasher.compute());
    let downloaded = local_model(&destination, &filename, &digest, kind, model.clone(), version);
    emit_download_progress(window, response_channel, current, total.max(current), &destination, downloaded.clone())?;
    Ok(downloaded)
}

fn emit_download_progress<R: Runtime>(
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    current: u64,
    total: u64,
    path: &Path,
    data: Value,
) -> Result<(), SerializedError> {
    if let Some(channel) = response_channel {
        window.emit(channel, json!({ "current": current, "total": total, "data": if data.is_null() { json!(path.to_string_lossy()) } else { data } }))
            .map_err(|error| serialized_error(error.to_string(), "IPC_EMIT_FAILED"))?;
    }
    Ok(())
}

fn model_folder<R: Runtime>(
    app: &AppHandle<R>,
    kind: &str,
    version: Option<&Value>,
) -> Result<PathBuf, SerializedError> {
    let root = match version {
        Some(version) => versions::version_path(app, version)?,
        None => linker::shared_folder(app)?,
    };
    Ok(root.join(model_folder_name(kind)?))
}

fn model_type(model: &Value) -> Result<&str, SerializedError> {
    model.get("type").and_then(Value::as_str)
        .filter(|kind| matches!(*kind, "avatar" | "bloq" | "platform" | "saber"))
        .ok_or_else(|| serialized_error("Model type is invalid", "INVALID_MODEL"))
}

fn model_folder_name(kind: &str) -> Result<&'static str, SerializedError> {
    match kind {
        "avatar" => Ok("CustomAvatars"),
        "bloq" => Ok("CustomNotes"),
        "platform" => Ok("CustomPlatforms"),
        "saber" => Ok("CustomSabers"),
        _ => Err(serialized_error("Model type is invalid", "INVALID_MODEL")),
    }
}

fn model_extension(kind: &str) -> Result<&'static str, SerializedError> {
    match kind {
        "avatar" => Ok(".avatar"),
        "bloq" => Ok(".bloq"),
        "platform" => Ok(".plat"),
        "saber" => Ok(".saber"),
        _ => Err(serialized_error("Model type is invalid", "INVALID_MODEL")),
    }
}

fn sanitize_filename(filename: &str) -> String {
    let mut result = filename.chars()
        .filter(|character| !character.is_control() && !matches!(character, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'))
        .collect::<String>();
    result = result.trim().trim_end_matches(['.', ' ']).to_owned();
    if matches!(result.split('.').next().unwrap_or_default().to_ascii_uppercase().as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        result.insert(0, '_');
    }
    result
}

fn local_model(
    path: &Path,
    filename: &str,
    hash: &str,
    kind: &str,
    model: Value,
    version: Option<&Value>,
) -> Value {
    let mut local = json!({
        "path": path.to_string_lossy(),
        "fileName": filename,
        "hash": hash,
        "type": kind,
        "model": model
    });
    if let (Some(version), Some(object)) = (version, local.as_object_mut()) {
        object.insert("version".to_owned(), version.clone());
    }
    local
}

fn emit_model_progress<R: Runtime>(
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    current: u64,
    total: u64,
    data: Option<Value>,
) -> Result<(), SerializedError> {
    if let Some(channel) = response_channel {
        let mut progression = json!({ "current": current, "total": total });
        if let (Some(data), Some(object)) = (data, progression.as_object_mut()) {
            object.insert("data".to_owned(), data);
        }
        window.emit(channel, progression)
            .map_err(|error| serialized_error(error.to_string(), "IPC_EMIT_FAILED"))?;
    }
    Ok(())
}

async fn model_by_hash(hash: &str) -> Value {
    let cache = MODEL_HASH_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock() {
        if let Some(model) = cache.get(hash) { return model.clone(); }
    }
    let query = json!({
        "start": 0,
        "end": 1,
        "platform": "pc",
        "filter": [{ "type": "hash", "value": hash }]
    });
    let result = query_models(&query).await.ok()
        .and_then(|results| results.as_object().and_then(|values| values.values().next().cloned()))
        .map(|mut model| {
            sanitize_field(&mut model, "name");
            sanitize_field(&mut model, "author");
            model
        }).unwrap_or(Value::Null);
    if !result.is_null() {
        if let Ok(mut cache) = cache.lock() { cache.insert(hash.to_owned(), result.clone()); }
    }
    result
}

fn hash_file_md5(path: &Path) -> std::io::Result<String> {
    let mut input = File::open(path)?;
    let mut hasher = md5::Context::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 { break; }
        hasher.consume(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.compute()))
}

pub async fn search_models(query: &Value) -> Result<Value, SerializedError> {
    let results = query_models(query).await?;
    let models = results.as_object().map(|values| {
        values.values().filter_map(|value| {
            if value.get("name").and_then(Value::as_str).is_none() { return None; }
            let mut model = value.clone();
            sanitize_field(&mut model, "name");
            sanitize_field(&mut model, "author");
            Some(model)
        }).collect::<Vec<_>>()
    }).unwrap_or_default();
    Ok(json!(models))
}

pub async fn get_model_by_id(id: &Value) -> Value {
    let query = json!({
        "start": 0,
        "end": 1,
        "platform": "pc",
        "filter": [{ "type": "id", "value": id }]
    });
    match query_models(&query).await {
        Ok(results) => {
            let key = id.as_str().map(str::to_owned).unwrap_or_else(|| id.to_string());
            results.get(&key).cloned().unwrap_or(Value::Null)
        }
        Err(error) => {
            log::warn!("ModelSaber lookup failed: {}", error.message);
            Value::Null
        }
    }
}

async fn query_models(query: &Value) -> Result<Value, SerializedError> {
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(45))
        .user_agent("BSManager/1.6.0")
        .build()
        .map_err(|error| serialized_error(error.to_string(), "MODEL_SABER_CLIENT_FAILED"))?;
    let mut parameters = Vec::<(String, String)>::new();
    for name in ["type", "platform", "start", "end", "sort", "sortDirection"] {
        if let Some(value) = query.get(name).filter(|value| !value.is_null()) {
            if let Some(value) = value.as_str() {
                if !value.is_empty() { parameters.push((name.to_owned(), value.to_owned())); }
            } else if value.is_number() {
                parameters.push((name.to_owned(), value.to_string()));
            }
        }
    }
    let filters = query.get("filter").and_then(Value::as_array).map(|filters| {
        filters.iter().filter_map(|filter| {
            let kind = filter.get("type")?.as_str()?;
            let value = filter.get("value")?;
            if value.is_null() { return None; }
            let value = value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string());
            let expression = if kind == "searchName" { value } else { format!("{kind}:{value}") };
            Some(if filter.get("isNegative").and_then(Value::as_bool).unwrap_or(false) {
                format!("-{expression}")
            } else { expression })
        }).collect::<Vec<_>>().join(",")
    }).unwrap_or_default();
    if !filters.is_empty() { parameters.push(("filter".to_owned(), filters)); }

    let mut url = Url::parse(MODEL_SABER_API)
        .map_err(|error| serialized_error(error.to_string(), "MODEL_SABER_URL_FAILED"))?;
    url.query_pairs_mut().extend_pairs(parameters);
    let response = client.get(url).send().await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| serialized_error(error.to_string(), "MODEL_SABER_REQUEST_FAILED"))?;
    let body = response.bytes().await
        .map_err(|error| serialized_error(error.to_string(), "MODEL_SABER_RESPONSE_FAILED"))?;
    serde_json::from_slice(&body)
        .map_err(|error| serialized_error(error.to_string(), "MODEL_SABER_INVALID_RESPONSE"))
}

fn sanitize_field(model: &mut Value, field: &str) {
    let Some(value) = model.get(field).and_then(Value::as_str).map(strip_tags) else { return; };
    if let Some(object) = model.as_object_mut() {
        object.insert(field.to_owned(), json!(value));
    }
}

fn strip_tags(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut in_tag = false;
    for character in value.chars() {
        match character {
            '<' if !in_tag => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => output.push(character),
            _ => {}
        }
    }
    output
}
