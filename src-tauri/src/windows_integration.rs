use std::{
    env,
    io::ErrorKind,
    path::{Path, PathBuf},
    ptr::null_mut,
    time::Duration,
};

use serde_json::{Value, json};
use tauri::{AppHandle, Manager, Runtime, path::BaseDirectory};
use tokio::time::sleep;
use winreg::{RegKey, enums::{HKEY_CLASSES_ROOT, HKEY_LOCAL_MACHINE, KEY_READ}};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
    System::Threading::{GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION},
    UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
};

use crate::{SerializedError, serialized_error};

const SIDELOAD_REGISTRY_KEY: &str = r"SOFTWARE\WOW6432Node\Oculus VR, LLC\Oculus";
const OPENXR_REGISTRY_KEY: &str = r"SOFTWARE\Khronos\OpenXR\1";

pub fn sideloaded_apps_enabled() -> Result<Value, SerializedError> {
    Ok(json!(read_sideload_setting()?))
}

pub async fn need_start_as_admin() -> Value {
    if process_token_elevated(unsafe { GetCurrentProcess() }).unwrap_or(false) {
        return json!(false);
    }
    let Some(pid) = process_id_by_name("steam.exe").await else { return json!(false) };
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() { return json!(false); }
    let elevated = process_token_elevated(process).unwrap_or(false);
    unsafe { CloseHandle(process); }
    json!(elevated)
}

pub fn open_external_uri(uri: &str) -> Result<(), SerializedError> {
    let operation = "open".encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let target = uri.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let empty = [0_u16];
    let result = unsafe {
        ShellExecuteW(null_mut(), operation.as_ptr(), target.as_ptr(), empty.as_ptr(), empty.as_ptr(), SW_SHOWNORMAL)
    };
    check_shell_execute_result(result, "EXTERNAL_URI_FAILED")
}

pub fn run_as_admin(helper: &Path, arguments: &[String]) -> Result<(), SerializedError> {
    let operation = "runas".encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let executable = helper.to_string_lossy().encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let parameters = arguments.iter().map(|argument| quote_windows_argument(argument)).collect::<Vec<_>>().join(" ");
    let parameters = parameters.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let directory = helper.parent().unwrap_or(Path::new(".")).to_string_lossy()
        .encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let result = unsafe {
        ShellExecuteW(null_mut(), operation.as_ptr(), executable.as_ptr(), parameters.as_ptr(), directory.as_ptr(), SW_SHOWNORMAL)
    };
    check_shell_execute_result(result, "LAUNCH_ELEVATION_FAILED")
}

pub fn oculus_is_installed() -> bool {
    let classes = RegKey::predef(HKEY_CLASSES_ROOT);
    let Ok(protocol) = classes.open_subkey_with_flags("oculus", KEY_READ) else { return false };
    if protocol.get_value::<String, _>("URL Protocol").is_err() { return false; }
    classes.open_subkey_with_flags("oculus\\shell\\open\\command", KEY_READ)
        .ok().and_then(|command| command.get_value::<String, _>("").ok())
        .is_some_and(|command| !command.trim().is_empty())
}

fn check_shell_execute_result(result: windows_sys::Win32::Foundation::HINSTANCE, code: &str) -> Result<(), SerializedError> {
    if (result as isize) <= 32 {
        return Err(serialized_error(format!("Windows could not start the requested program ({})", result as isize), code));
    }
    Ok(())
}

fn quote_windows_argument(argument: &str) -> String {
    if !argument.is_empty() && !argument.chars().any(|character| character.is_whitespace() || character == '"') {
        return argument.to_owned();
    }
    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for character in argument.chars() {
        if character == '\\' {
            backslashes += 1;
        } else if character == '"' {
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

pub async fn enable_sideloaded_apps<R: Runtime>(app: &AppHandle<R>) -> Result<Value, SerializedError> {
    if read_sideload_setting()? {
        return Ok(Value::Null);
    }

    let helper = app.path().resolve(
        "../assets/scripts/oculus-allow-dev-sideloaded.exe",
        BaseDirectory::Resource,
    ).ok().filter(|path| path.is_file()).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../assets/scripts/oculus-allow-dev-sideloaded.exe")
    });
    if !helper.is_file() {
        return Err(serialized_error("Oculus sideload helper was not found", "SIDELOAD_HELPER_NOT_FOUND"));
    }

    let log_directory = app.path().app_log_dir()
        .map_err(|error| serialized_error(error.to_string(), "LOG_PATH_FAILED"))?;
    std::fs::create_dir_all(&log_directory)
        .map_err(|error| serialized_error(error.to_string(), "LOG_PATH_FAILED"))?;
    let log_path = log_directory.join("oculus-allow-dev-sideloaded.log");
    let parameters = format!("--log-path \"{}\"", log_path.display());
    let operation = "runas".encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let executable = helper.to_string_lossy().encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let parameters = parameters.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let working_directory = helper.parent().unwrap_or(Path::new(".")).to_string_lossy()
        .encode_utf16().chain(Some(0)).collect::<Vec<_>>();

    let result = unsafe {
        ShellExecuteW(
            null_mut(),
            operation.as_ptr(),
            executable.as_ptr(),
            parameters.as_ptr(),
            working_directory.as_ptr(),
            SW_SHOWNORMAL,
        )
    };
    if (result as isize) <= 32 {
        return Err(serialized_error(
            format!("Windows could not start the elevated sideload helper ({})", result as isize),
            "SIDELOAD_ELEVATION_FAILED",
        ));
    }

    for _ in 0..240 {
        if read_sideload_setting()? {
            return Ok(Value::Null);
        }
        sleep(Duration::from_millis(250)).await;
    }
    Err(serialized_error(
        "Oculus sideload setting was not enabled. Check the Windows elevation prompt and helper log.",
        "SIDELOAD_ENABLE_TIMED_OUT",
    ))
}

pub fn active_vr_runtime(command: Option<&str>) -> Value {
    let (overrides, _) = parse_leading_environment(command.unwrap_or_default());
    if let Some(runtime_path) = environment_value(&overrides, "XR_RUNTIME_JSON").filter(|path| !path.is_empty()) {
        return json!(runtime_from_path(&expand_environment(&runtime_path, &overrides)));
    }

    let machine = RegKey::predef(HKEY_LOCAL_MACHINE);
    let Ok(key) = machine.open_subkey_with_flags(OPENXR_REGISTRY_KEY, KEY_READ) else {
        return json!("not-set");
    };
    let Ok(runtime_path) = key.get_value::<String, _>("ActiveRuntime") else {
        return json!("not-set");
    };
    json!(runtime_from_path(&expand_environment(&runtime_path, &overrides)))
}

fn read_sideload_setting() -> Result<bool, SerializedError> {
    let machine = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = match machine.open_subkey_with_flags(SIDELOAD_REGISTRY_KEY, KEY_READ) {
        Ok(key) => key,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(serialized_error(error.to_string(), "SIDELOAD_REGISTRY_READ_FAILED")),
    };
    Ok(key.get_value::<u32, _>("AllowDevSideloaded").unwrap_or_default() == 1)
}

fn process_token_elevated(process: HANDLE) -> Option<bool> {
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 { return None; }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0_u32;
    let result = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut TOKEN_ELEVATION as *mut std::ffi::c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    unsafe { CloseHandle(token); }
    (result != 0).then_some(elevation.TokenIsElevated != 0)
}

async fn process_id_by_name(executable: &str) -> Option<u32> {
    let output = tokio::process::Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {executable}"), "/FO", "CSV", "/NH"])
        .creation_flags(0x08000000)
        .output().await.ok()?;
    let output = String::from_utf8_lossy(&output.stdout);
    output.lines().find_map(|line| {
        let mut columns = line.split(',');
        let name = columns.next()?.trim().trim_matches('"');
        if !name.eq_ignore_ascii_case(executable) { return None; }
        columns.next()?.trim().trim_matches('"').parse::<u32>().ok()
    })
}

pub fn parse_leading_environment(command: &str) -> (Vec<(String, String)>, String) {
    let characters = command.chars().collect::<Vec<_>>();
    let mut position = 0;
    let mut command_start = characters.len();
    let mut values = Vec::new();
    while position < characters.len() {
        while position < characters.len() && characters[position].is_whitespace() { position += 1; }
        if position >= characters.len() { break; }
        let start = position;
        if !(characters[position].is_ascii_alphabetic() || characters[position] == '_') {
            command_start = start;
            break;
        }
        while position < characters.len() && (characters[position].is_ascii_alphanumeric() || characters[position] == '_') {
            position += 1;
        }
        if start == position || position >= characters.len() || characters[position] != '=' {
            command_start = start;
            break;
        }
        let name = characters[start..position].iter().collect::<String>();
        position += 1;
        let value = if position < characters.len() && matches!(characters[position], '\'' | '"') {
            let quote = characters[position];
            position += 1;
            let start = position;
            while position < characters.len() && characters[position] != quote { position += 1; }
            if position >= characters.len() { command_start = start; break; }
            let value = characters[start..position].iter().collect::<String>();
            position += 1;
            if position < characters.len() && !characters[position].is_whitespace() { command_start = start; break; }
            value
        } else {
            let start = position;
            while position < characters.len() && !characters[position].is_whitespace() { position += 1; }
            characters[start..position].iter().collect::<String>()
        };
        values.push((name, value));
    }
    let remainder = characters[command_start..].iter().collect::<String>().trim().to_owned();
    (values, remainder)
}

fn environment_value(overrides: &[(String, String)], name: &str) -> Option<String> {
    overrides.iter().rev()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
        .or_else(|| env::vars().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value))
}

fn expand_environment(value: &str, overrides: &[(String, String)]) -> String {
    let mut expanded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('%') {
        expanded.push_str(&rest[..start]);
        let after_start = &rest[start + 1..];
        let Some(end) = after_start.find('%') else {
            expanded.push_str(&rest[start..]);
            return expanded;
        };
        let variable = &after_start[..end];
        if let Some(replacement) = environment_value(overrides, variable) {
            expanded.push_str(&replacement);
        } else {
            expanded.push_str(&rest[start..start + end + 2]);
        }
        rest = &after_start[end + 1..];
    }
    expanded.push_str(rest);
    expanded
}

fn runtime_from_path(path: &str) -> &'static str {
    if !Path::new(path).exists() {
        return "not-set";
    }
    let normalized = path.replace('/', "\\").to_ascii_lowercase();
    if normalized.contains("\\steamvr\\") || normalized.contains("\\steamxr_") {
        return "steam";
    }
    if normalized.contains("\\virtualdesktop\\")
        || normalized.contains("\\virtualdesktop-")
        || normalized.contains("\\virtual desktop\\")
        || normalized.contains("\\vdxr")
    {
        return "vdxr";
    }
    if normalized.contains("\\oculus\\")
        || normalized.contains("\\oculus-runtime\\")
        || normalized.contains("\\oculus_openxr_")
        || normalized.contains("\\meta\\quest link\\")
        || normalized.contains("\\meta_openxr_")
    {
        return "oculus";
    }
    "other"
}
