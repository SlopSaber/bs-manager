use std::fs;

use serde_json::Value;
use tauri::{AppHandle, Manager, Runtime};
use winreg::{RegKey, enums::{HKEY_CURRENT_USER, KEY_READ}};

use crate::stores;

pub async fn create_launch_shortcut<R: Runtime>(app: &AppHandle<R>, args: &Value) -> bool {
    let Some(options) = args.get("options") else { return false };
    let Some(version) = options.get("version") else { return false };
    let Some(version_id) = version.get("BSVersion").and_then(Value::as_str).filter(|value| !value.is_empty()) else { return false };
    let launch_url = build_launch_url(app, options);
    let version_name = version.get("name").and_then(Value::as_str).unwrap_or_default();
    let shortcut_name = sanitize_shortcut_name(&format!("Beat Saber {version_id} {version_name}"));
    if shortcut_name.is_empty() { return false; }

    if args.get("steamShortcut").and_then(Value::as_bool).unwrap_or(false) {
        create_steam_shortcut(app, &shortcut_name, &launch_url).await
    } else {
        create_desktop_shortcut(app, &shortcut_name, &launch_url)
    }
}

pub(crate) fn build_launch_url<R: Runtime>(app: &AppHandle<R>, options: &Value) -> String {
    let version = options.get("version").unwrap_or(&Value::Null);
    let mut params = url::form_urlencoded::Serializer::new(String::new());
    if let Some(value) = version.get("BSVersion").and_then(Value::as_str) { params.append_pair("version", value); }
    if let Some(value) = version.get("name").and_then(Value::as_str).filter(|value| !value.is_empty()) { params.append_pair("versionName", value); }
    if version.get("steam").and_then(Value::as_bool).unwrap_or(false) { params.append_pair("versionSteam", "true"); }
    if version.get("oculus").and_then(Value::as_bool).unwrap_or(false) { params.append_pair("versionOculus", "true"); }
    if let Some(ino) = version.get("ino").and_then(Value::as_u64) { params.append_pair("versionIno", &ino.to_string()); }
    if let Some(command) = options.get("command").and_then(Value::as_str).filter(|value| !value.is_empty()) { params.append_pair("command", command); }
    if let Some(mods) = options.get("launchMods").and_then(Value::as_array) {
        for (mod_name, key) in [
            ("oculus", "oculusMode"), ("fpfc", "desktopMode"), ("debug", "debug"),
            ("skip_steam", "skipSteam"), ("editor", "mapEditor"), ("proton_logs", "protonLogs"),
            ("parallel_views", "parallelViews"),
        ] {
            if mods.iter().any(|value| value.as_str() == Some(mod_name)) { params.append_pair(key, "true"); }
        }
    }
    format!("{}://launch?{}", launch_protocol(app), params.finish())
}

pub(crate) fn launch_protocol<R: Runtime>(app: &AppHandle<R>) -> &'static str {
    if app.config().identifier == "io.bsmanager.bsmanager.preview" {
        "bsmanager-preview"
    } else {
        "bsmanager"
    }
}

fn create_desktop_shortcut<R: Runtime>(app: &AppHandle<R>, name: &str, launch_url: &str) -> bool {
    let Ok(desktop) = app.path().desktop_dir() else { return false };
    let Ok(executable) = std::env::current_exe() else { return false };
    let content = format!("[InternetShortcut]\r\nURL={launch_url}\r\nIconFile={}\r\nIconIndex=0\r\n", executable.display());
    fs::write(desktop.join(format!("{name}.url")), content).is_ok()
}

async fn create_steam_shortcut<R: Runtime>(app: &AppHandle<R>, name: &str, launch_url: &str) -> bool {
    let Some(steam_root) = stores::steam_install_path() else { return false };
    let current_user = RegKey::predef(HKEY_CURRENT_USER);
    let active_user = current_user.open_subkey_with_flags(r"Software\Valve\Steam\ActiveProcess", KEY_READ)
        .ok().and_then(|key| key.get_value::<u32, _>("ActiveUser").ok()).filter(|id| *id > 0);
    let user_data = steam_root.join("userdata");
    let Ok(entries) = fs::read_dir(&user_data) else { return false };
    let mut folders = entries.flatten().map(|entry| entry.path()).filter(|path| {
        path.is_dir() && path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
    }).collect::<Vec<_>>();
    if let Some(active_user) = active_user {
        folders.retain(|folder| folder.file_name().and_then(|name| name.to_str()).and_then(|name| name.parse::<u32>().ok()) == Some(active_user));
    }
    if folders.is_empty() { return false; }
    let Ok(executable) = std::env::current_exe() else { return false };
    let Some(start_directory) = executable.parent() else { return false };
    let name = name.replace('\0', "");
    let executable_path = executable.to_string_lossy().to_string();
    let executable = format!("\"{executable_path}\"");
    let start_directory = format!("\"{}\"", start_directory.display());
    let icon = executable_path;
    let launch_options = format!("\"{launch_url}\"");
    let mut success = true;
    for folder in folders {
        let config = folder.join("config");
        if fs::create_dir_all(&config).is_err() { success = false; continue; }
        let shortcuts_path = config.join("shortcuts.vdf");
        let original = fs::read_to_string(&shortcuts_path).unwrap_or_default();
        let updated = append_shortcut(&original, &name, &executable, &start_directory, &icon, &launch_options);
        if fs::write(shortcuts_path, updated).is_err() { success = false; }
    }
    let _ = app;
    success
}

fn append_shortcut(raw: &str, name: &str, executable: &str, start_directory: &str, icon: &str, launch_options: &str) -> String {
    let header = "\0shortcuts\0";
    let mut data = if raw.contains(header) { raw.to_owned() } else { format!("{header}\x08\x08") };
    let end = data.rfind("\x08\x08").unwrap_or(data.len());
    let index = next_shortcut_index(&data[..end]);
    let mut entry = format!("\0{index}\0");
    entry.push_str("\x02appid\0\0\0\0\0");
    append_string_field(&mut entry, "AppName", name);
    append_string_field(&mut entry, "Exe", executable);
    append_string_field(&mut entry, "StartDir", start_directory);
    append_string_field(&mut entry, "icon", icon);
    append_string_field(&mut entry, "ShortcutPath", "");
    append_string_field(&mut entry, "LaunchOptions", launch_options);
    append_flag_field(&mut entry, "IsHidden", false);
    append_flag_field(&mut entry, "AllowDesktopConfig", false);
    append_flag_field(&mut entry, "AllowOverlay", false);
    append_flag_field(&mut entry, "OpenVR", true);
    append_flag_field(&mut entry, "Devkit", false);
    append_string_field(&mut entry, "DevkitGameID", "");
    entry.push_str("\x02DevkitOverrideAppID\0\0\0\0\0");
    entry.push_str("\x02LastPlayTime\0\0\0\0\0");
    entry.push_str("\0tags\0\x08\x08");
    data.insert_str(end, &entry);
    data
}

fn next_shortcut_index(data: &str) -> u32 {
    let bytes = data.as_bytes();
    let mut index = 0_u32;
    for start in 0..bytes.len() {
        if bytes[start] != 0 { continue; }
        let Some(end_offset) = bytes[start + 1..].iter().position(|byte| *byte == 0) else { continue };
        let end = start + 1 + end_offset;
        let Some(prefix) = bytes.get(end + 1..end + 8) else { continue };
        if prefix != b"\x02appid\0" { continue; }
        if let Ok(value) = std::str::from_utf8(&bytes[start + 1..end])
            && let Ok(parsed) = value.parse::<u32>()
        {
            index = index.max(parsed.saturating_add(1));
        }
    }
    index
}

fn append_string_field(output: &mut String, key: &str, value: &str) {
    output.push('\x01');
    output.push_str(key);
    output.push('\0');
    output.push_str(value);
    output.push('\0');
}

fn append_flag_field(output: &mut String, key: &str, enabled: bool) {
    output.push('\x02');
    output.push_str(key);
    output.push('\0');
    output.push(if enabled { '\x01' } else { '\0' });
    output.push_str("\0\0\0");
}

fn sanitize_shortcut_name(value: &str) -> String {
    let sanitized = value.chars().map(|character| {
        if matches!(character, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') { '_' } else { character }
    }).collect::<String>();
    sanitized.trim().trim_end_matches(['.', ' ']).to_owned()
}
