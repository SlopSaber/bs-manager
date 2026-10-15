use std::{
    collections::BTreeMap,
    io::{Cursor, Read},
    path::{Component, Path, PathBuf},
    sync::{Arc, atomic::{AtomicBool, Ordering}},
    time::Duration,
};

use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager, Runtime, WebviewWindow};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, time::sleep};
use zip::ZipArchive;

use crate::{AppState, SerializedError, meta, serialized_error, versions};

const MANIFEST_URL: &str = "https://securecdn.oculus.com/binaries/download/";
const SEGMENT_URL: &str = "https://securecdn.oculus.com/binaries/segment/";
const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;
const MAX_SEGMENT_BYTES: usize = 64 * 1024 * 1024;
const CONCURRENT_FILES: usize = 15;
const VERSION_METADATA_FILE: &str = "metadata.config";

#[derive(Clone)]
pub struct OculusControl {
    cancelled: Arc<AtomicBool>,
    explicitly_cancelled: Arc<AtomicBool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadRequest {
    token: String,
    bs_version: Value,
    #[serde(default)]
    is_verification: bool,
}

#[derive(Deserialize)]
struct Manifest {
    files: BTreeMap<String, ManifestFile>,
}

#[derive(Clone, Deserialize)]
struct ManifestFile {
    sha256: String,
    size: u64,
    #[serde(default)]
    segments: Vec<ManifestSegment>,
}

#[derive(Clone, Deserialize)]
struct ManifestSegment(u64, String, u64);

pub async fn download<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    args: &Value,
    request_cancelled: Arc<AtomicBool>,
) -> Result<Value, SerializedError> {
    let request: DownloadRequest = serde_json::from_value(args.clone())
        .map_err(|error| serialized_error(error.to_string(), "INVALID_DOWNLOAD_REQUEST"))?;
    if !meta::valid_token(&request.token) {
        return Err(serialized_error("Meta access token is invalid", "NO_META_AUTH_TOKEN"));
    }
    let binary_id = request.bs_version.get("OculusBinaryId")
        .and_then(|value| value.as_str().map(str::to_owned).or_else(|| value.as_u64().map(|id| id.to_string())))
        .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| serialized_error("Meta binary ID is invalid", "INVALID_BINARY_ID"))?;

    let explicitly_cancelled = Arc::new(AtomicBool::new(false));
    let control = OculusControl {
        cancelled: request_cancelled,
        explicitly_cancelled,
    };
    {
        let state = app.state::<AppState>();
        let mut active = state.oculus_download.lock()
            .map_err(|_| serialized_error("Meta download state is unavailable", "DOWNLOAD_STATE_FAILED"))?;
        if active.is_some() {
            return Err(serialized_error("Already downloading", "ALREADY_DOWNLOADING"));
        }
        *active = Some(control.clone());
    }

    let result = download_inner(
        app,
        window,
        response_channel,
        request,
        binary_id,
        control.clone(),
    ).await;
    {
        let state = app.state::<AppState>();
        if let Ok(mut active) = state.oculus_download.lock() {
            if active.as_ref().is_some_and(|current| Arc::ptr_eq(&current.cancelled, &control.cancelled)) {
                *active = None;
            }
        }
    }
    result
}

pub fn stop_download<R: Runtime>(app: &AppHandle<R>) -> Value {
    if let Ok(active) = app.state::<AppState>().oculus_download.lock() {
        if let Some(download) = active.as_ref() {
            download.explicitly_cancelled.store(true, Ordering::Relaxed);
            download.cancelled.store(true, Ordering::Relaxed);
        }
    }
    Value::Null
}

async fn download_inner<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    request: DownloadRequest,
    binary_id: String,
    control: OculusControl,
) -> Result<Value, SerializedError> {
    check_cancelled(&control)?;
    let (mut version, destination) = if request.is_verification {
        let destination = versions::version_path(app, &request.bs_version)?;
        (request.bs_version.clone(), destination)
    } else {
        let mut version = request.bs_version.clone();
        {
            let object = version.as_object_mut()
                .ok_or_else(|| serialized_error("Beat Saber version is invalid", "INVALID_VERSION"))?;
            object.insert("steam".to_owned(), json!(false));
            object.insert("oculus".to_owned(), json!(false));
            object.insert("metadata".to_owned(), json!({ "id": "", "store": "oculus" }));
        }
        let base = versions::version_path(app, &version)?;
        let mut destination = base.clone();
        let mut suffix = 0_u32;
        while tokio::fs::try_exists(&destination).await.unwrap_or(false) {
            suffix += 1;
            destination = PathBuf::from(format!("{} ({suffix})", base.display()));
        }
        let folder_name = destination.file_name().and_then(|name| name.to_str())
            .ok_or_else(|| serialized_error("Version folder name is invalid", "INVALID_VERSION"))?;
        if folder_name != version.get("BSVersion").and_then(Value::as_str).unwrap_or_default() {
            version.as_object_mut().unwrap().insert("name".to_owned(), json!(folder_name));
        }
        version.as_object_mut().unwrap().insert("path".to_owned(), json!(destination));
        (version, destination)
    };

    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await
            .map_err(|error| serialized_error(error.to_string(), "DOWNLOAD_PATH_FAILED"))?;
    }
    tokio::fs::create_dir_all(&destination).await
        .map_err(|error| serialized_error(error.to_string(), "DOWNLOAD_PATH_FAILED"))?;
    if let Some(object) = version.as_object_mut() {
        object.insert("path".to_owned(), json!(destination));
        object.insert("steam".to_owned(), json!(false));
        object.insert("oculus".to_owned(), json!(false));
    }
    let metadata = json!({ "id": uuid::Uuid::new_v4().to_string(), "store": "oculus" });
    if let Some(object) = version.as_object_mut() {
        object.insert("metadata".to_owned(), metadata.clone());
    }
    let metadata_path = destination.join(VERSION_METADATA_FILE);
    tokio::fs::write(&metadata_path, serde_json::to_vec_pretty(&metadata).unwrap_or_default()).await
        .map_err(|error| serialized_error(error.to_string(), "VERSION_METADATA_WRITE_FAILED"))?;

    let client = crate::proxy::client_builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(180))
        .user_agent("BSManager/1.6.0")
        .build()
        .map_err(|_| serialized_error("Could not initialize the Meta download client", "NETWORK_CLIENT_FAILED"))?;
    let manifest = fetch_manifest(&client, &request.token, &binary_id, &control).await?;
    let files = manifest.files.into_iter().collect::<Vec<_>>();
    let total = files.iter().fold(0_u64, |sum, (_, file)| sum.saturating_add(file.size));
    emit_progress(window, response_channel, 0, total, &version)?;

    let progress_current = Arc::new(tokio::sync::Mutex::new(0_u64));
    let window = window.clone();
    let response_channel = response_channel.map(str::to_owned);
    let download_results = futures_util::stream::iter(files)
        .map(|(name, file)| {
            let client = client.clone();
            let token = request.token.clone();
            let binary_id = binary_id.clone();
            let destination = destination.clone();
            let control = control.clone();
            let version = version.clone();
            let current = progress_current.clone();
            let window = window.clone();
            let response_channel = response_channel.clone();
            async move {
                let relative = safe_manifest_path(&name)?;
                let target = destination.join(relative);
                ensure_safe_parent(&destination, &target).await?;
                if file_integrity_valid(&target, &file.sha256, file.size).await.unwrap_or(false) {
                    advance_progress(&window, response_channel.as_deref(), &current, file.size, total, &version).await?;
                    return Ok::<(), SerializedError>(());
                }
                if let Some(parent) = target.parent() {
                    tokio::fs::create_dir_all(parent).await
                        .map_err(|error| serialized_error(error.to_string(), "DOWNLOAD_PATH_FAILED"))?;
                }
                let mut output = tokio::fs::File::create(&target).await
                    .map_err(|error| serialized_error(error.to_string(), "DOWNLOAD_FILE_FAILED"))?;
                let mut written = 0_u64;
                for segment in file.segments {
                    check_cancelled(&control)?;
                    let ManifestSegment(_offset, segment_hash, expected_size) = segment;
                    let compressed = fetch_segment(&client, &token, &binary_id, &segment_hash, &control).await?;
                    let inflated = tokio::task::spawn_blocking(move || inflate_segment(compressed, expected_size)).await
                        .map_err(|error| serialized_error(format!("Segment decompression failed: {error}"), "SEGMENT_DECOMPRESSION_FAILED"))??;
                    output.write_all(&inflated).await
                        .map_err(|error| serialized_error(error.to_string(), "DOWNLOAD_FILE_FAILED"))?;
                    written = written.saturating_add(inflated.len() as u64);
                    advance_progress(&window, response_channel.as_deref(), &current, inflated.len() as u64, total, &version).await?;
                }
                output.flush().await
                    .map_err(|error| serialized_error(error.to_string(), "DOWNLOAD_FILE_FAILED"))?;
                drop(output);
                if written != file.size || !file_integrity_valid(&target, &file.sha256, file.size).await.unwrap_or(false) {
                    return Err(serialized_error(format!("File integrity check failed: {name}"), "FILE_INTEGRITY_FAILED"));
                }
                Ok(())
            }
        })
        .buffer_unordered(CONCURRENT_FILES)
        .collect::<Vec<_>>()
        .await;

    check_cancelled(&control)?;
    if download_results.iter().any(Result::is_err) {
        return Err(serialized_error("Some files failed to download", "SOME_FILES_FAILED_TO_DOWNLOAD"));
    }
    if files_empty_total_is_inconsistent(total, *progress_current.lock().await) {
        return Err(serialized_error("Downloaded files did not match the manifest size", "SOME_FILES_FAILED_TO_DOWNLOAD"));
    }
    Ok(Value::Null)
}

fn files_empty_total_is_inconsistent(total: u64, current: u64) -> bool {
    total > 0 && current < total
}

async fn fetch_manifest(
    client: &Client,
    token: &str,
    binary_id: &str,
    control: &OculusControl,
) -> Result<Manifest, SerializedError> {
    let mut url = reqwest::Url::parse(MANIFEST_URL)
        .map_err(|_| serialized_error("Meta manifest URL is invalid", "NETWORK_REQUEST_FAILED"))?;
    url.query_pairs_mut()
        .append_pair("id", binary_id)
        .append_pair("access_token", token)
        .append_pair("get_manifest", "1");
    let response = retry_request(
        client.get(url),
        control,
    ).await.map_err(|_| if control.cancelled.load(Ordering::Relaxed) {
        cancellation_error(control)
    } else {
        serialized_error("Could not download the Meta manifest", "UNABLE_TO_GET_MANIFEST")
    })?;
    let bytes = bounded_response(response, MAX_MANIFEST_BYTES, control).await
        .map_err(|error| if control.cancelled.load(Ordering::Relaxed) {
            cancellation_error(control)
        } else {
            serialized_error(error.message, "UNABLE_TO_GET_MANIFEST")
        })?;
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|_| serialized_error("Meta manifest archive is invalid", "UNABLE_TO_GET_MANIFEST"))?;
    let mut entry = archive.by_name("manifest.json")
        .map_err(|_| serialized_error("Manifest file not found", "MANIFEST_FILE_NOT_FOUND"))?;
    if entry.size() as usize > MAX_MANIFEST_BYTES {
        return Err(serialized_error("Meta manifest is too large", "PARSE_MANIFEST_FILE_FAILED"));
    }
    let mut json_bytes = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut json_bytes)
        .map_err(|_| serialized_error("Could not read Meta manifest", "PARSE_MANIFEST_FILE_FAILED"))?;
    serde_json::from_slice(&json_bytes)
        .map_err(|_| serialized_error("Could not parse Meta manifest", "PARSE_MANIFEST_FILE_FAILED"))
}

async fn fetch_segment(
    client: &Client,
    token: &str,
    binary_id: &str,
    segment_hash: &str,
    control: &OculusControl,
) -> Result<Vec<u8>, SerializedError> {
    let mut url = reqwest::Url::parse(SEGMENT_URL)
        .map_err(|_| serialized_error("Meta segment URL is invalid", "NETWORK_REQUEST_FAILED"))?;
    url.query_pairs_mut()
        .append_pair("access_token", token)
        .append_pair("binary_id", binary_id)
        .append_pair("segment_sha256", segment_hash);
    let response = retry_request(
        client.get(url),
        control,
    ).await.map_err(|_| if control.cancelled.load(Ordering::Relaxed) {
        cancellation_error(control)
    } else {
        serialized_error("Could not download a Meta file segment", "SEGMENT_DOWNLOAD_FAILED")
    })?;
    bounded_response(response, MAX_SEGMENT_BYTES, control).await
        .map_err(|error| if control.cancelled.load(Ordering::Relaxed) {
            cancellation_error(control)
        } else {
            serialized_error(error.message, "SEGMENT_DOWNLOAD_FAILED")
        })
}

async fn retry_request(
    mut current: reqwest::RequestBuilder,
    control: &OculusControl,
) -> Result<reqwest::Response, SerializedError> {
    // RequestBuilder can be cloned until sent; retry transient transport and server errors.
    for attempt in 0..4 {
        check_cancelled(control)?;
        let retry = if attempt < 3 { current.try_clone() } else { None };
        let response = tokio::select! {
            result = current.send() => result,
            _ = poll_cancelled(control) => return Err(cancellation_error(control)),
        };
        match response {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response) if attempt < 3 && is_retryable(response.status()) && retry.is_some() => {
                current = retry.unwrap();
                sleep(Duration::from_millis(250 * (1 << attempt))).await;
            }
            Ok(response) => return Err(serialized_error(
                format!("Meta service returned HTTP {}", response.status()),
                "NETWORK_REQUEST_FAILED",
            )),
            Err(error) if attempt < 3 && (error.is_timeout() || error.is_connect()) && retry.is_some() => {
                current = retry.unwrap();
                sleep(Duration::from_millis(250 * (1 << attempt))).await;
            }
            Err(_) => return Err(serialized_error("Meta network request failed", "NETWORK_REQUEST_FAILED")),
        }
    }
    Err(serialized_error("Meta network request failed", "NETWORK_REQUEST_FAILED"))
}

fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

async fn bounded_response(
    response: reqwest::Response,
    maximum: usize,
    control: &OculusControl,
) -> Result<Vec<u8>, SerializedError> {
    if response.content_length().is_some_and(|size| size > maximum as u64) {
        return Err(serialized_error("Meta response exceeded the size limit", "RESPONSE_TOO_LARGE"));
    }
    let bytes = tokio::select! {
        result = response.bytes() => result.map_err(|_| serialized_error("Meta response was interrupted", "NETWORK_REQUEST_FAILED"))?,
        _ = poll_cancelled(control) => return Err(cancellation_error(control)),
    };
    if bytes.len() > maximum {
        return Err(serialized_error("Meta response exceeded the size limit", "RESPONSE_TOO_LARGE"));
    }
    Ok(bytes.to_vec())
}

async fn poll_cancelled(control: &OculusControl) {
    loop {
        if control.cancelled.load(Ordering::Relaxed) { return; }
        sleep(Duration::from_millis(100)).await;
    }
}

fn check_cancelled(control: &OculusControl) -> Result<(), SerializedError> {
    if control.cancelled.load(Ordering::Relaxed) {
        Err(cancellation_error(control))
    } else {
        Ok(())
    }
}

fn cancellation_error(control: &OculusControl) -> SerializedError {
    let explicit = control.explicitly_cancelled.load(Ordering::Relaxed);
    serialized_error("Meta download was cancelled", if explicit { "DOWNLOAD_CANCELLED" } else { "REQUEST_CANCELLED" })
}

fn safe_manifest_path(name: &str) -> Result<PathBuf, SerializedError> {
    if name.trim().is_empty() || name.contains('\0') {
        return Err(serialized_error("Manifest contains an invalid file path", "INVALID_MANIFEST_PATH"));
    }
    let path = Path::new(name);
    if path.is_absolute() || path.components().any(|component| !matches!(component, Component::Normal(_))) {
        return Err(serialized_error("Manifest contains an unsafe file path", "INVALID_MANIFEST_PATH"));
    }
    Ok(path.to_path_buf())
}

async fn ensure_safe_parent(root: &Path, target: &Path) -> Result<(), SerializedError> {
    let relative = target.strip_prefix(root)
        .map_err(|_| serialized_error("Manifest file escaped the installation folder", "INVALID_MANIFEST_PATH"))?;
    let mut current = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            let Component::Normal(name) = component else {
                return Err(serialized_error("Manifest contains an unsafe file path", "INVALID_MANIFEST_PATH"));
            };
            current.push(name);
            match tokio::fs::symlink_metadata(&current).await {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(serialized_error("Manifest path crosses a symbolic link", "INVALID_MANIFEST_PATH"));
                }
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(serialized_error("Manifest path parent is not a folder", "INVALID_MANIFEST_PATH"));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(serialized_error(error.to_string(), "DOWNLOAD_PATH_FAILED")),
            }
        }
    }
    Ok(())
}

async fn file_integrity_valid(path: &Path, expected_hash: &str, expected_size: u64) -> Result<bool, SerializedError> {
    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(serialized_error(error.to_string(), "FILE_READ_FAILED")),
    };
    if file.metadata().await
        .map_err(|error| serialized_error(error.to_string(), "FILE_READ_FAILED"))?.len() != expected_size
    {
        return Ok(false);
    }
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).await
            .map_err(|error| serialized_error(error.to_string(), "FILE_READ_FAILED"))?;
        if read == 0 { break; }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(expected_hash))
}

fn inflate_segment(compressed: Vec<u8>, expected_size: u64) -> Result<Vec<u8>, SerializedError> {
    if expected_size > MAX_SEGMENT_BYTES as u64 {
        return Err(serialized_error("Meta segment size is invalid", "PARSE_MANIFEST_FILE_FAILED"));
    }
    let decoders = [
        decompress_zlib as fn(&[u8], u64) -> Result<Vec<u8>, std::io::Error>,
        decompress_gzip,
        decompress_deflate,
    ];
    for decompress in decoders {
        if let Ok(inflated) = decompress(&compressed, expected_size) {
            if inflated.len() as u64 == expected_size {
                return Ok(inflated);
            }
        }
    }
    Err(serialized_error("Meta file segment did not decompress to its expected size", "SEGMENT_DECOMPRESSION_FAILED"))
}

fn read_decompressed(reader: impl Read, expected_size: u64) -> Result<Vec<u8>, std::io::Error> {
    let mut limited = reader.take(expected_size.saturating_add(1));
    let mut output = Vec::with_capacity(expected_size as usize);
    limited.read_to_end(&mut output)?;
    Ok(output)
}

fn decompress_zlib(bytes: &[u8], expected_size: u64) -> Result<Vec<u8>, std::io::Error> {
    let mut decoder = ZlibDecoder::new(bytes);
    read_decompressed(&mut decoder, expected_size)
}

fn decompress_gzip(bytes: &[u8], expected_size: u64) -> Result<Vec<u8>, std::io::Error> {
    let mut decoder = GzDecoder::new(bytes);
    read_decompressed(&mut decoder, expected_size)
}

fn decompress_deflate(bytes: &[u8], expected_size: u64) -> Result<Vec<u8>, std::io::Error> {
    let mut decoder = DeflateDecoder::new(bytes);
    read_decompressed(&mut decoder, expected_size)
}

fn emit_progress<R: Runtime>(
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    current: u64,
    total: u64,
    version: &Value,
) -> Result<(), SerializedError> {
    if let Some(channel) = response_channel {
        window.emit(channel, json!({ "current": current, "total": total, "data": version }))
            .map_err(|error| serialized_error(error.to_string(), "IPC_EMIT_FAILED"))?;
    }
    Ok(())
}

async fn advance_progress<R: Runtime>(
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    current: &tokio::sync::Mutex<u64>,
    increment: u64,
    total: u64,
    version: &Value,
) -> Result<(), SerializedError> {
    let mut current = current.lock().await;
    *current = current.saturating_add(increment).min(total);
    emit_progress(window, response_channel, *current, total, version)
}
