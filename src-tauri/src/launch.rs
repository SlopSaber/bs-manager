use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager, Runtime, WebviewWindow, path::BaseDirectory};
use tokio::{process::Command, time::sleep};

use crate::{
    AppState, SerializedError, serialized_error, versions, windows_integration,
};

const GAME_EXE: &str = "Beat Saber.exe";
const STEAM_APP_ID: &str = "620980";

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
        let close_on_launch = should_close_manager_after_launch(app);
        let existing_processes = if close_on_launch { process_ids(GAME_EXE).await } else { HashSet::new() };
        let mut helper_args = vec![game_executable.to_string_lossy().to_string()];
        helper_args.append(&mut launch_args);
        helper_args.push("--log-path".to_owned());
        helper_args.push(log_directory.join("bs-admin-start.log").to_string_lossy().to_string());
        windows_integration::run_as_admin(&helper, &helper_args)?;
        if close_on_launch && wait_for_new_game_process(&existing_processes).await {
            app.exit(0);
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
    let close_on_launch = should_close_manager_after_launch(app);
    let existing_processes = if close_on_launch { process_ids(GAME_EXE).await } else { HashSet::new() };
    let launches_game_directly = Path::new(&executable).file_name().and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(GAME_EXE));
    let mut process = Command::new(executable);
    process.args(parsed)
        .current_dir(&game_folder)
        .envs(environment)
        .kill_on_drop(false)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    process.creation_flags(0x08000000);
    let mut child = process.spawn()
        .map_err(|error| serialized_error(error.to_string(), "EXIT"))?;
    if close_on_launch {
        if launches_game_directly || wait_for_new_game_process(&existing_processes).await {
            app.exit(0);
        }
    }
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

async fn wait_for_new_game_process(existing: &HashSet<u32>) -> bool {
    for _ in 0..120 {
        if process_ids(GAME_EXE).await.iter().any(|pid| !existing.contains(pid)) { return true; }
        sleep(Duration::from_millis(500)).await;
    }
    false
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
        let _ = window.emit(channel, json!({ "type": kind }));
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
