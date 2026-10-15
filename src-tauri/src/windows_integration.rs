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
use winreg::{RegKey, enums::{HKEY_LOCAL_MACHINE, KEY_READ}};
use windows_sys::Win32::{
    UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
};

use crate::{SerializedError, serialized_error};

const SIDELOAD_REGISTRY_KEY: &str = r"SOFTWARE\WOW6432Node\Oculus VR, LLC\Oculus";
const OPENXR_REGISTRY_KEY: &str = r"SOFTWARE\Khronos\OpenXR\1";

pub fn sideloaded_apps_enabled() -> Result<Value, SerializedError> {
    Ok(json!(read_sideload_setting()?))
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
    let overrides = parse_leading_environment(command.unwrap_or_default());
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

fn parse_leading_environment(command: &str) -> Vec<(String, String)> {
    let characters = command.chars().collect::<Vec<_>>();
    let mut position = 0;
    let mut values = Vec::new();
    while position < characters.len() {
        while position < characters.len() && characters[position].is_whitespace() { position += 1; }
        if position >= characters.len() { break; }
        let start = position;
        while position < characters.len() && (characters[position].is_ascii_alphanumeric() || characters[position] == '_') {
            position += 1;
        }
        if start == position || position >= characters.len() || characters[position] != '=' {
            break;
        }
        let name = characters[start..position].iter().collect::<String>();
        position += 1;
        let value = if position < characters.len() && matches!(characters[position], '\'' | '"') {
            let quote = characters[position];
            position += 1;
            let start = position;
            while position < characters.len() && characters[position] != quote { position += 1; }
            if position >= characters.len() { break; }
            let value = characters[start..position].iter().collect::<String>();
            position += 1;
            if position < characters.len() && !characters[position].is_whitespace() { break; }
            value
        } else {
            let start = position;
            while position < characters.len() && !characters[position].is_whitespace() { position += 1; }
            characters[start..position].iter().collect::<String>()
        };
        values.push((name, value));
    }
    values
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
