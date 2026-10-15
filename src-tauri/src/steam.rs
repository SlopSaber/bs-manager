use std::{
    fs,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, atomic::{AtomicBool, Ordering}},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager, Runtime, WebviewWindow, path::BaseDirectory};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, Command},
    sync::mpsc,
    time::sleep,
};
use uuid::Uuid;

use crate::{AppState, SerializedError, serialized_error, versions};

const PROTOCOL_VERSION: u64 = 1;
const APP_ID: u32 = 620980;
const DEPOT_ID: u32 = 620981;
const MAX_LINE_LENGTH: usize = 128 * 1024;

#[derive(Clone)]
pub struct SteamControl {
    commands: mpsc::UnboundedSender<SteamCommand>,
    cancelled: Arc<AtomicBool>,
}

enum SteamCommand {
    Input(String),
    Cancel,
}

enum DownloadWake {
    Command(Option<SteamCommand>),
    Tick,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SteamSession {
    username: String,
    refresh_token: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadRequest {
    username: Option<String>,
    password: Option<String>,
    #[serde(default)]
    stay: bool,
    #[serde(default)]
    is_verification: bool,
    bs_version: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SidecarEvent {
    version: u64,
    #[serde(rename = "type")]
    kind: String,
    sub_type: String,
    #[serde(default)]
    data: Value,
}

pub async fn download<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    args: &Value,
    cancelled: Arc<AtomicBool>,
    qr: bool,
    automatic: bool,
) -> Result<Value, SerializedError> {
    let request: DownloadRequest = serde_json::from_value(args.clone())
        .map_err(|error| serialized_error(error.to_string(), "INVALID_DOWNLOAD_REQUEST"))?;
    let manifest = request.bs_version.get("BSManifest")
        .and_then(|value| value.as_str().map(str::to_owned).or_else(|| value.as_u64().map(|id| id.to_string())))
        .filter(|manifest| !manifest.is_empty() && manifest.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| serialized_error("Beat Saber manifest ID is invalid", "InvalidManifest"))?;

    let version_path = versions::version_path(app, &request.bs_version)?;
    let pending_path = version_path.join(".bs-download").join("pending.json");
    let pending = fs::read(&pending_path).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let resumable = pending.as_ref().is_some_and(|pending| {
        pending.get("depot").and_then(Value::as_str) == Some(DEPOT_ID.to_string().as_str())
            && pending.get("manifest").and_then(Value::as_str) == Some(manifest.as_str())
    });
    let destination = if request.is_verification || resumable {
        version_path
    } else {
        unique_destination(&version_path)
    };
    fs::create_dir_all(destination.parent().ok_or_else(|| serialized_error("Installation path is invalid", "DirectoryCreate"))?)
        .map_err(|error| serialized_error(error.to_string(), "DirectoryCreate"))?;

    let mut version = request.bs_version.clone();
    let folder_name = destination.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    if version.get("BSVersion").and_then(Value::as_str) != Some(folder_name) {
        version.as_object_mut()
            .ok_or_else(|| serialized_error("Version data is invalid", "InvalidManifest"))?
            .insert("name".to_owned(), json!(folder_name));
    }
    let version_object = version.as_object_mut()
        .ok_or_else(|| serialized_error("Version data is invalid", "InvalidManifest"))?;
    version_object.insert("path".to_owned(), json!(destination));
    version_object.insert("steam".to_owned(), json!(false));
    version_object.insert("oculus".to_owned(), json!(false));
    version_object.insert("metadata".to_owned(), json!({ "store": "steam", "id": "" }));

    let requested_username = request.username.as_deref().unwrap_or_default();
    let requested_password = request.password.as_deref().unwrap_or_default();
    let saved_session = if !qr && requested_password.is_empty() && !requested_username.trim().is_empty() {
        load_session(app, requested_username).await
    } else {
        None
    };
    let username = saved_session.as_ref().map(|session| session.username.as_str())
        .unwrap_or(requested_username);
    let password = requested_password;
    let options = json!({
        "app": APP_ID,
        "depot": DEPOT_ID,
        "manifest": manifest,
        "directory": destination,
        "username": username,
        "password": password,
        "refreshToken": saved_session.as_ref().map(|session| session.refresh_token.as_str()),
        "qr": qr,
    });

    let executable = downloader_path(app)?;
    let mut command = Command::new(executable);
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn()
        .map_err(|error| serialized_error(format!("Could not run bs-downloader: {error}"), "ExeNotFoundWindows"))?;
    let mut stdin = child.stdin.take().ok_or_else(|| serialized_error("Could not communicate with bs-downloader", "NotCompleted"))?;
    let stdout = child.stdout.take().ok_or_else(|| serialized_error("Could not read bs-downloader response", "NotCompleted"))?;
    let (commands, mut command_rx) = mpsc::unbounded_channel();
    let control = SteamControl { commands: commands.clone(), cancelled: cancelled.clone() };
    let start = json!({ "command": "start", "version": PROTOCOL_VERSION, "options": options });
    if let Err(error) = write_command(&mut stdin, &start).await {
        let _ = child.start_kill();
        return Err(serialized_error(error.to_string(), "NotCompleted"));
    }
    let active_set = {
        let state = app.state::<AppState>();
        match state.steam_download.lock() {
            Ok(mut active) => { *active = Some(control); true }
            Err(_) => false,
        }
    };
    if !active_set {
        let _ = child.start_kill();
        return Err(serialized_error("Steam downloader state is unavailable", "NotCompleted"));
    }
    if let Some(channel) = response_channel {
        let start_event = json!({ "type": "Info", "subType": "Start", "data": serde_json::to_string(&version).unwrap_or_default() });
        if window.emit(channel, start_event).is_err() {
            cancelled.store(true, Ordering::Relaxed);
            let _ = commands.send(SteamCommand::Cancel);
        }
    }

    let mut lines = BufReader::new(stdout).lines();
    let mut finished_event: Option<Value> = None;
    let mut failure: Option<SerializedError> = None;
    let mut cancel_deadline = None;
    let mut cancel_sent = false;
    let mut force_killed = false;
    let mut input_open = true;

    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if line.len() > MAX_LINE_LENGTH {
                            failure = Some(serialized_error("bs-downloader response too large", "NotCompleted"));
                            cancelled.store(true, Ordering::Relaxed);
                        } else if !line.trim().is_empty() {
                            match handle_event(&line, app, window, response_channel, automatic, request.stay || automatic, &mut finished_event).await {
                                Ok(()) => {}
                                Err(error) => {
                                    if error.code != "REQUEST_CANCELLED" {
                                        failure = Some(error);
                                    }
                                    cancelled.store(true, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        failure = Some(serialized_error(error.to_string(), "NotCompleted"));
                        cancelled.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
            wake = async {
                tokio::select! {
                    command = command_rx.recv() => DownloadWake::Command(command),
                    _ = sleep(Duration::from_millis(100)) => DownloadWake::Tick,
                }
            }, if input_open => {
                let (command, channel_closed) = match wake {
                    DownloadWake::Command(command) => {
                        let channel_closed = command.is_none();
                        (command, channel_closed)
                    }
                    DownloadWake::Tick => (None, false),
                };
                if channel_closed {
                    input_open = false;
                }
                match command {
                    Some(SteamCommand::Input(value)) => {
                        let input = json!({ "command": "input", "value": value });
                        if let Err(error) = write_command(&mut stdin, &input).await {
                            failure = Some(serialized_error(error.to_string(), "NotCompleted"));
                            cancelled.store(true, Ordering::Relaxed);
                        }
                    }
                    Some(SteamCommand::Cancel) => cancelled.store(true, Ordering::Relaxed),
                    None => {}
                }
                if cancelled.load(Ordering::Relaxed) {
                    if !cancel_sent {
                        let _ = write_command(&mut stdin, &json!({ "command": "cancel" })).await;
                        cancel_sent = true;
                        cancel_deadline = Some(Instant::now() + Duration::from_secs(5));
                    } else if !force_killed && cancel_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        let _ = child.start_kill();
                        force_killed = true;
                    }
                }
            }
        }
    }

    let status = child.wait().await;
    {
        let state = app.state::<AppState>();
        if let Ok(mut active) = state.steam_download.lock() {
            *active = None;
        }
    }
    let status = status.map_err(|error| serialized_error(error.to_string(), "NotCompleted"))?;

    if let Some(error) = failure {
        return Err(error);
    }
    if cancelled.load(Ordering::Relaxed) {
        return Ok(Value::Null);
    }
    if !status.success() {
        return Err(serialized_error("bs-downloader exited without a completed download", "NotCompleted"));
    }
    let Some(finished) = finished_event else {
        return Err(serialized_error("bs-downloader exited without a completed download", "NotCompleted"));
    };

    let metadata = json!({ "id": Uuid::new_v4().to_string(), "store": "steam" });
    let metadata_path = destination.join("metadata.config");
    fs::write(&metadata_path, serde_json::to_vec_pretty(&metadata).unwrap_or_default())
        .map_err(|error| serialized_error(error.to_string(), "VERSION_METADATA_WRITE_FAILED"))?;
    if let Some(response_channel) = response_channel {
        window.emit(response_channel, finished)
            .map_err(|error| serialized_error(error.to_string(), "IPC_EMIT_FAILED"))?;
    }
    Ok(Value::Null)
}

pub fn stop_download<R: Runtime>(app: &AppHandle<R>) -> Value {
    if let Ok(active) = app.state::<AppState>().steam_download.lock() {
        if let Some(active) = active.as_ref() {
            active.cancelled.store(true, Ordering::Relaxed);
            let _ = active.commands.send(SteamCommand::Cancel);
        }
    }
    Value::Null
}

pub fn send_input<R: Runtime>(app: &AppHandle<R>, args: &Value) -> Value {
    let Some(value) = args.as_str() else { return json!(false) };
    let sent = app.state::<AppState>().steam_download.lock().ok()
        .and_then(|active| active.as_ref().map(|active| active.commands.send(SteamCommand::Input(value.to_owned())).is_ok()))
        .unwrap_or(false);
    json!(sent)
}

pub fn protect_secret(value: &[u8]) -> io::Result<Vec<u8>> {
    dpapi::protect(value)
}

pub fn unprotect_secret(value: &[u8]) -> io::Result<Vec<u8>> {
    dpapi::unprotect(value)
}

async fn handle_event<R: Runtime>(
    line: &str,
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    automatic: bool,
    persist_session: bool,
    finished_event: &mut Option<Value>,
) -> Result<(), SerializedError> {
    let event = serde_json::from_str::<SidecarEvent>(line)
        .map_err(|error| serialized_error(format!("Invalid bs-downloader response: {error}"), "NotCompleted"))?;
    if event.version != PROTOCOL_VERSION {
        return Err(serialized_error("Unsupported bs-downloader protocol", "NotCompleted"));
    }
    match event.kind.as_str() {
        "Session" if event.sub_type == "Authenticated" => {
            if !persist_session { return Ok(()); }
            if let Ok(session) = serde_json::from_value::<SteamSession>(event.data) {
                if let Err(error) = save_session(app, &session).await {
                    log::warn!("Could not save Steam session: {}", error.message);
                }
            } else {
                log::warn!("bs-downloader returned an invalid Steam session");
            }
            Ok(())
        }
        "Diagnostic" => Ok(()),
        "Error" => {
            let code = event.sub_type;
            Err(serialized_error("bs-downloader failed", &code))
        }
        "Info" | "Warning" => {
            if event.kind == "Info" && event.sub_type == "Password" && automatic {
                return Err(serialized_error("Ask for password while auto download", "Unknown"));
            }
            if !valid_event_type(&event.kind, &event.sub_type) || !event.data.is_string() {
                return Err(serialized_error("Invalid bs-downloader event", "NotCompleted"));
            }
            let value = json!({ "type": event.kind, "subType": event.sub_type, "data": event.data });
            if event.kind == "Info" && event.sub_type == "Finished" {
                *finished_event = Some(value);
            } else if let Some(channel) = response_channel {
                window.emit(channel, value)
                    .map_err(|error| serialized_error(error.to_string(), "IPC_EMIT_FAILED"))?;
            }
            Ok(())
        }
        _ => Err(serialized_error("Invalid bs-downloader event", "NotCompleted")),
    }
}

fn valid_event_type(kind: &str, sub_type: &str) -> bool {
    match kind {
        "Info" => ["Start", "Password", "Guard", "2FA", "Progress", "Validated", "Finished", "SteamID", "QRCode", "MobileApp"].contains(&sub_type),
        "Warning" => ["ManifestChecksum", "ConnectionTimeout", "Unknown"].contains(&sub_type),
        _ => false,
    }
}

async fn write_command(stdin: &mut ChildStdin, command: &Value) -> io::Result<()> {
    let command = serde_json::to_string(command)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    stdin.write_all(command.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

fn unique_destination(path: &Path) -> PathBuf {
    let mut destination = path.to_path_buf();
    let mut suffix = 0;
    while fs::symlink_metadata(&destination).is_ok() {
        suffix += 1;
        destination = PathBuf::from(format!("{} ({suffix})", path.display()));
    }
    destination
}

fn downloader_path<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, SerializedError> {
    let resource = app.path().resolve("../assets/scripts/bs-downloader.exe", BaseDirectory::Resource).ok();
    let development = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../assets/scripts/bs-downloader.exe");
    resource.filter(|path| path.is_file()).or_else(|| development.is_file().then_some(development))
        .ok_or_else(|| serialized_error("bs-downloader executable not found", "ExeNotFoundWindows"))
}

async fn session_path<R: Runtime>(app: &AppHandle<R>, username: &str) -> Result<PathBuf, SerializedError> {
    let config = app.path().app_config_dir().map_err(|error| serialized_error(error.to_string(), "SESSION_STORAGE_FAILED"))?;
    let key = format!("{:x}", Sha256::digest(username.trim().to_lowercase().as_bytes()));
    Ok(config.join("steam-sessions").join(format!("{key}.bin")))
}

async fn load_session<R: Runtime>(app: &AppHandle<R>, username: &str) -> Option<SteamSession> {
    let path = session_path(app, username).await.ok()?;
    let encrypted = fs::read(path).ok()?;
    let decrypted = dpapi::unprotect(&encrypted).ok()?;
    let session = serde_json::from_slice::<SteamSession>(&decrypted).ok()?;
    if session.username.eq_ignore_ascii_case(username.trim()) && !session.refresh_token.is_empty() {
        Some(session)
    } else {
        None
    }
}

async fn save_session<R: Runtime>(app: &AppHandle<R>, session: &SteamSession) -> Result<(), SerializedError> {
    let path = session_path(app, &session.username).await?;
    let parent = path.parent().ok_or_else(|| serialized_error("Steam session storage path is invalid", "SESSION_STORAGE_FAILED"))?;
    fs::create_dir_all(parent).map_err(|error| serialized_error(error.to_string(), "SESSION_STORAGE_FAILED"))?;
    let bytes = serde_json::to_vec(session).map_err(|error| serialized_error(error.to_string(), "SESSION_STORAGE_FAILED"))?;
    let encrypted = dpapi::protect(&bytes).map_err(|error| serialized_error(error.to_string(), "SESSION_STORAGE_FAILED"))?;
    fs::write(path, encrypted).map_err(|error| serialized_error(error.to_string(), "SESSION_STORAGE_FAILED"))
}

#[cfg(windows)]
mod dpapi {
    use std::{io, ptr::{null, null_mut}, slice};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB, CryptProtectData, CryptUnprotectData,
        },
    };

    pub fn protect(plaintext: &[u8]) -> io::Result<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB { cbData: plaintext.len() as u32, pbData: plaintext.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
        let success = unsafe { CryptProtectData(&input, null(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output) };
        if success == 0 {
            return Err(io::Error::last_os_error());
        }
        let encrypted = unsafe { slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
        unsafe { LocalFree(output.pbData.cast()); }
        Ok(encrypted)
    }

    pub fn unprotect(encrypted: &[u8]) -> io::Result<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB { cbData: encrypted.len() as u32, pbData: encrypted.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
        let success = unsafe { CryptUnprotectData(&input, null_mut(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output) };
        if success == 0 {
            return Err(io::Error::last_os_error());
        }
        let decrypted = unsafe { slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
        unsafe { LocalFree(output.pbData.cast()); }
        Ok(decrypted)
    }
}

#[cfg(not(windows))]
mod dpapi {
    use std::io;
    pub fn protect(_plaintext: &[u8]) -> io::Result<Vec<u8>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "Secure credential storage is available only on Windows"))
    }
    pub fn unprotect(_encrypted: &[u8]) -> io::Result<Vec<u8>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "Secure credential storage is available only on Windows"))
    }
}
