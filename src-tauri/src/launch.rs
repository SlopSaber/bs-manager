use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;
use serde_json::{Value, json};
use tauri::{AppHandle, Manager, Runtime, WebviewWindow, path::BaseDirectory};
use tokio::{process::Command, time::{Instant as TokioInstant, sleep, timeout, timeout_at}};
use chrono::{DateTime, Utc};

use crate::{
    AppState, SerializedError, serialized_error, versions, windows_integration,
};

const GAME_EXE: &str = "Beat Saber.exe";
const STEAM_APP_ID: &str = "620980";
const PROCESS_QUERY: &str = "[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false); $rows = @(Get-CimInstance Win32_Process -Filter \"Name = 'Beat Saber.exe'\" | ForEach-Object { [pscustomobject]@{ pid = $_.ProcessId; startedAt = $_.CreationDate.ToUniversalTime().ToString('o'); path = $_.ExecutablePath } }); ConvertTo-Json -InputObject $rows -Compress";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GameProcess {
    pid: u32,
    started_at: String,
    path: Option<String>,
}

struct GameProcessIdentity {
    pid: u32,
    started_at: String,
    path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LaunchRequest {
    version: Value,
    #[serde(default)]
    launch_mods: Vec<String>,
    command: Option<String>,
    #[serde(default)]
    admin: bool,
}

pub async fn launch<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    args: &Value,
) -> Result<Value, SerializedError> {
    let state = app.state::<AppState>();
    if state.launch_in_progress.compare_exchange(false, true, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire).is_err() {
        return Err(serialized_error("Beat Saber launch already in progress", "LAUNCH_IN_PROGRESS"));
    }
    let result = launch_inner(app, window, response_channel, args).await;
    state.launch_in_progress.store(false, std::sync::atomic::Ordering::Release);
    result
}

async fn launch_inner<R: Runtime>(
    app: &AppHandle<R>,
    window: &WebviewWindow<R>,
    response_channel: Option<&str>,
    args: &Value,
) -> Result<Value, SerializedError> {
    let request: LaunchRequest = serde_json::from_value(args.clone())
        .map_err(|error| serialized_error(error.to_string(), "INVALID_LAUNCH_OPTIONS"))?;
    let game_folder = versions::version_path(app, &request.version)?;
    let game_executable = game_folder.join(GAME_EXE);
    if !game_executable.is_file() {
        return Err(serialized_error(format!("Beat Saber executable was not found in {}", game_folder.display()), "EXE_NOT_FOUND"));
    }

    let is_oculus = request.version.get("oculus").and_then(Value::as_bool).unwrap_or(false)
        || request.version.get("metadata").and_then(|metadata| metadata.get("store")).and_then(Value::as_str) == Some("oculus");
    let is_meta_install = is_oculus;
    if is_meta_install && process_is_running(GAME_EXE).await {
        return Err(serialized_error("Beat Saber is already running", "BS_ALREADY_RUNNING"));
    }

    if is_meta_install {
        ensure_oculus_running().await?;
    } else if !request.launch_mods.iter().any(|launch_mod| launch_mod == "skip_steam") {
        ensure_steam_running(window, response_channel).await;
    }

    persist_last_launched_version(app, &request.version)?;
    emit_event(window, response_channel, "BS_LAUNCHING");

    let mut launch_args = build_game_arguments(&request.version, &request.launch_mods);
    let (environment_overrides, custom_command) = windows_integration::parse_leading_environment(request.command.as_deref().unwrap_or_default());
    let mut environment = HashMap::new();
    environment.insert("SteamAppId".to_owned(), STEAM_APP_ID.to_owned());
    environment.insert("SteamOverlayGameId".to_owned(), STEAM_APP_ID.to_owned());
    environment.insert("SteamGameId".to_owned(), STEAM_APP_ID.to_owned());
    environment.extend(environment_overrides);
    let fpfc = request.launch_mods.iter().any(|launch_mod| launch_mod == "fpfc");
    let oculus_mode = request.launch_mods.iter().any(|launch_mod| launch_mod == "oculus");
    if fpfc && !oculus_mode {
        environment.insert("VR_OVERRIDE".to_owned(), game_executable.to_string_lossy().to_string());
        environment.insert("XR_RUNTIME_JSON".to_owned(), game_executable.join("disabled-openxr.json").to_string_lossy().to_string());
        push_unique(&mut launch_args, "-vrmode");
        push_unique(&mut launch_args, "None");
    }

    if request.admin {
        let helper = app.path().resolve("../assets/scripts/start_beat_saber_admin.exe", BaseDirectory::Resource)
            .ok().filter(|path| path.is_file()).unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../assets/scripts/start_beat_saber_admin.exe")
            });
        if !helper.is_file() {
            return Err(serialized_error("Elevated launch helper was not found", "ADMIN_HELPER_NOT_FOUND"));
        }
        let log_directory = app.path().app_log_dir()
            .map_err(|error| serialized_error(error.to_string(), "LOG_PATH_FAILED"))?;
        std::fs::create_dir_all(&log_directory)
            .map_err(|error| serialized_error(error.to_string(), "LOG_PATH_FAILED"))?;
        let existing_processes = game_processes().await.ok().map(|processes| processes.into_iter().map(|process| process.pid).collect::<HashSet<_>>());
        let launched_after = Utc::now();
        let mut helper_args = vec![game_executable.to_string_lossy().to_string()];
        helper_args.append(&mut launch_args);
        helper_args.push("--log-path".to_owned());
        helper_args.push(log_directory.join("bs-admin-start.log").to_string_lossy().to_string());
        windows_integration::run_as_admin(&helper, &helper_args)?;
        if let Some(existing_processes) = existing_processes {
            if let Some(launched_game) = wait_for_new_game_process(&existing_processes, &game_executable, launched_after).await {
                if should_close_manager_after_launch(app) {
                    app.exit(0);
                } else {
                    wait_for_game_process_exit(&launched_game).await;
                }
            }
        }
        return Ok(Value::Null);
    }

    let replacement = quote_windows_argument(&game_executable.to_string_lossy());
    let command_line = if custom_command.contains("%command%") {
        custom_command.replace("%command%", &replacement)
    } else if custom_command.is_empty() {
        replacement
    } else {
        format!("{replacement} {custom_command}")
    };
    let mut parsed = split_windows_command_line(&command_line);
    if parsed.is_empty() {
        return Err(serialized_error("Launch command is empty", "INVALID_LAUNCH_OPTIONS"));
    }
    let executable = parsed.remove(0);
    parsed.append(&mut launch_args);
    let existing_processes = game_processes().await.ok().map(|processes| processes.into_iter().map(|process| process.pid).collect::<HashSet<_>>());
    let launched_after = Utc::now();
    let mut process = Command::new(executable);
    process.args(parsed)
        .current_dir(&game_folder)
        .envs(environment)
        .kill_on_drop(false)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    process.creation_flags(0x08000000);
    let child = process.spawn()
        .map_err(|error| serialized_error(error.to_string(), "EXIT"))?;
    if let Some(existing_processes) = existing_processes {
        if let Some(launched_game) = wait_for_new_game_process(&existing_processes, &game_executable, launched_after).await {
            if should_close_manager_after_launch(app) {
                app.exit(0);
            } else {
                wait_for_game_process_exit(&launched_game).await;
            }
            return Ok(Value::Null);
        }
    }
    let mut child = child;
    child.wait().await.map_err(|error| serialized_error(error.to_string(), "EXIT"))?;
    Ok(Value::Null)
}

async fn ensure_steam_running<R: Runtime>(window: &WebviewWindow<R>, response_channel: Option<&str>) {
    if process_is_running("steam.exe").await { return; }
    emit_event(window, response_channel, "STEAM_LAUNCHING");
    if windows_integration::open_external_uri("steam://open/games").is_ok() {
        for _ in 0..150 {
            if process_is_running("steam.exe").await {
                emit_event(window, response_channel, "STEAM_LAUNCHED");
                return;
            }
            sleep(Duration::from_millis(400)).await;
        }
    }
    emit_event(window, response_channel, "UNABLE_TO_LAUNCH_STEAM");
}

async fn ensure_oculus_running() -> Result<(), SerializedError> {
    if process_is_running("OculusClient.exe").await || process_is_running("Client.exe").await {
        return Ok(());
    }
    if !windows_integration::oculus_is_installed() {
        return Err(serialized_error("Meta Quest Link is not installed", "OCULUS_NOT_INSTALLED"));
    }
    if windows_integration::open_external_uri("oculus://view/homepage").is_ok() {
        for _ in 0..75 {
            if process_is_running("OculusClient.exe").await || process_is_running("Client.exe").await {
                return Ok(());
            }
            sleep(Duration::from_millis(400)).await;
        }
    }
    log::warn!("Unable to start Meta Quest Link; continuing with the Beat Saber launch");
    Ok(())
}

async fn process_is_running(executable: &str) -> bool {
    !process_ids(executable).await.is_empty()
}

async fn process_ids(executable: &str) -> HashSet<u32> {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {executable}"), "/FO", "CSV", "/NH"])
        .creation_flags(0x08000000)
        .output()
        .await;
    output.ok().map(|output| {
        String::from_utf8_lossy(&output.stdout).lines().filter_map(|line| {
            let mut columns = line.split(',');
            let name = columns.next()?.trim().trim_matches('"');
            if !name.eq_ignore_ascii_case(executable) { return None; }
            columns.next()?.trim().trim_matches('"').parse::<u32>().ok()
        }).collect()
    }).unwrap_or_default()
}

async fn game_processes() -> Result<Vec<GameProcess>, String> {
    let powershell = std::env::var_os("WINDIR")
        .map(|windir| PathBuf::from(windir).join("System32\\WindowsPowerShell\\v1.0\\powershell.exe"))
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"));
    let mut command = Command::new(powershell);
    command.args(["-NoProfile", "-NonInteractive", "-Command", PROCESS_QUERY])
        .creation_flags(0x08000000)
        .kill_on_drop(true);
    let output = timeout(Duration::from_secs(10), command.output()).await
        .map_err(|_| "Process enumeration timed out".to_owned())?
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let value: Value = serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())?;
    let values = match value {
        Value::Array(values) => values,
        Value::Null => Vec::new(),
        value => vec![value],
    };
    values.into_iter().map(|value| serde_json::from_value(value).map_err(|error| error.to_string())).collect()
}

async fn wait_for_new_game_process(existing: &HashSet<u32>, executable: &Path, launched_after: DateTime<Utc>) -> Option<GameProcessIdentity> {
    let expected_path = normalize_process_path(&std::fs::canonicalize(executable).unwrap_or_else(|_| executable.to_path_buf()).to_string_lossy());
    let deadline = TokioInstant::now() + Duration::from_secs(60);
    while TokioInstant::now() < deadline {
        match timeout_at(deadline, game_processes()).await {
            Err(_) => return None,
            Ok(Ok(processes)) => {
                let candidates = processes.into_iter().filter_map(|process| {
                    if existing.contains(&process.pid) { return None; }
                    let path = process.path?;
                    if normalize_process_path(&path) != expected_path { return None; }
                    let started_at = DateTime::parse_from_rfc3339(&process.started_at).ok()?;
                    if started_at.timestamp_millis() < launched_after.timestamp_millis() { return None; }
                    Some(GameProcessIdentity { pid: process.pid, started_at: process.started_at, path })
                }).collect::<Vec<_>>();
                if candidates.len() == 1 { return candidates.into_iter().next(); }
            }
            Ok(Err(error)) => log::debug!("Could not inspect Beat Saber process ownership: {error}"),
        }
        if timeout_at(deadline, sleep(Duration::from_millis(500))).await.is_err() { return None; }
    }
    None
}

async fn wait_for_game_process_exit(identity: &GameProcessIdentity) {
    loop {
        match game_processes().await {
            Ok(processes) => {
                let still_running = processes.iter().any(|process| {
                    process.pid == identity.pid
                        && process.started_at == identity.started_at
                        && process.path.as_deref().is_some_and(|path| normalize_process_path(path) == normalize_process_path(&identity.path))
                });
                if !still_running { return; }
            }
            Err(error) => log::debug!("Could not inspect Beat Saber process exit: {error}"),
        }
        sleep(Duration::from_secs(1)).await;
    }
}

fn normalize_process_path(path: &str) -> String {
    let normalized = path.trim_matches('"').replace('/', "\\").to_ascii_lowercase();
    normalized.strip_prefix("\\\\?\\").unwrap_or(&normalized).to_owned()
}

fn build_game_arguments(version: &Value, launch_mods: &[String]) -> Vec<String> {
    let mut args = Vec::new();
    if !version.get("steam").and_then(Value::as_bool).unwrap_or(false)
        && !version.get("oculus").and_then(Value::as_bool).unwrap_or(false)
    {
        push_unique(&mut args, "--no-yeet");
    }
    if launch_mods.iter().any(|launch_mod| launch_mod == "oculus") {
        push_unique(&mut args, "-vrmode");
        push_unique(&mut args, "oculus");
    }
    if launch_mods.iter().any(|launch_mod| launch_mod == "fpfc") { push_unique(&mut args, "fpfc"); }
    if launch_mods.iter().any(|launch_mod| launch_mod == "debug") { push_unique(&mut args, "--verbose"); }
    if launch_mods.iter().any(|launch_mod| launch_mod == "editor") { push_unique(&mut args, "editor"); }
    args
}

fn push_unique(args: &mut Vec<String>, value: &str) {
    if !args.iter().any(|argument| argument == value) { args.push(value.to_owned()); }
}

fn split_windows_command_line(command: &str) -> Vec<String> {
    let characters = command.chars().collect::<Vec<_>>();
    let mut args = Vec::new();
    let mut position = 0;
    while position < characters.len() {
        while position < characters.len() && characters[position].is_whitespace() { position += 1; }
        if position == characters.len() { break; }
        let mut argument = String::new();
        let mut in_quotes = false;
        let mut started = false;
        while position < characters.len() {
            if characters[position].is_whitespace() && !in_quotes { break; }
            if characters[position] == '\\' {
                let start = position;
                while position < characters.len() && characters[position] == '\\' { position += 1; }
                let count = position - start;
                if position < characters.len() && characters[position] == '"' {
                    argument.push_str(&"\\".repeat(count / 2));
                    if count % 2 == 1 {
                        argument.push('"');
                    } else {
                        in_quotes = !in_quotes;
                    }
                    position += 1;
                    started = true;
                } else {
                    argument.push_str(&"\\".repeat(count));
                    started = true;
                }
            } else if characters[position] == '"' {
                in_quotes = !in_quotes;
                started = true;
                position += 1;
            } else {
                argument.push(characters[position]);
                started = true;
                position += 1;
            }
        }
        if started { args.push(argument); }
        while position < characters.len() && characters[position].is_whitespace() { position += 1; }
    }
    args
}

fn quote_windows_argument(value: &str) -> String {
    if !value.chars().any(|character| character.is_whitespace() || character == '"') {
        return value.to_owned();
    }
    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for character in value.chars() {
        if character == '\\' { backslashes += 1; }
        else if character == '"' {
            quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
            quoted.push('"');
            backslashes = 0;
        } else {
            quoted.push_str(&"\\".repeat(backslashes));
            quoted.push(character);
            backslashes = 0;
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

fn emit_event<R: Runtime>(window: &WebviewWindow<R>, response_channel: Option<&str>, kind: &str) {
    if let Some(channel) = response_channel {
        let _ = crate::emit_response_value(window, Some(channel), json!({ "type": kind }));
    }
}

fn persist_last_launched_version<R: Runtime>(app: &AppHandle<R>, version: &Value) -> Result<(), SerializedError> {
    let mut settings = crate::read_settings(app)
        .map_err(|error| serialized_error(error, "SETTINGS_READ_FAILED"))?;
    settings.insert("last-version-launched".to_owned(), version.clone());
    crate::write_settings(app, &settings)
        .map_err(|error| serialized_error(error, "SETTINGS_WRITE_FAILED"))
}

fn should_close_manager_after_launch<R: Runtime>(app: &AppHandle<R>) -> bool {
    crate::read_settings(app).ok()
        .and_then(|settings| settings.get("close-bs-manager-on-launch").and_then(Value::as_bool))
        .unwrap_or(false)
}
