use std::path::Path;

use serde_json::{Value, json};
use tauri::{AppHandle, Runtime};
use url::Url;
use winreg::{RegKey, enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE}};

use crate::{shortcuts, versions};

const LIV_KEY: &str = r"Software\LIV.App";
const EXTERNAL_APPS_KEY: &str = "ExternalApplications";
const SHORTCUT_PREFIX: &str = "BSManager";

#[derive(Clone, Debug, Eq, PartialEq)]
struct LivEntry {
    id: String,
    name: String,
    install_path: String,
    executable: String,
    arguments: String,
}

struct ExistingEntry {
    id: String,
    value: LivEntry,
}

pub fn sync<R: Runtime>(app: &AppHandle<R>, installed_versions: &Value) -> Result<(), String> {
    let user = RegKey::predef(HKEY_CURRENT_USER);
    let liv = match user.open_subkey_with_flags(LIV_KEY, KEY_READ) {
        Ok(key) => key,
        Err(error) if error.raw_os_error() == Some(2) => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };

    let versions = installed_versions.as_array().map(Vec::as_slice).unwrap_or_default();
    let external = match liv.open_subkey_with_flags(EXTERNAL_APPS_KEY, KEY_READ | KEY_WRITE) {
        Ok(key) => key,
        Err(error) if error.raw_os_error() == Some(2) && versions.is_empty() => return Ok(()),
        Err(error) if error.raw_os_error() == Some(2) => liv.create_subkey(EXTERNAL_APPS_KEY)
            .map_err(|error| error.to_string())?.0,
        Err(error) => return Err(error.to_string()),
    };

    let existing = read_owned_entries(&external)?;
    let executable = std::env::current_exe().map_err(|error| error.to_string())?
        .to_string_lossy().into_owned();
    let mut desired = Vec::new();
    for version in versions {
        if version.get("steam").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }

        let install_path = versions::version_path(app, version)
            .map_err(|error| error.message)?.to_string_lossy().into_owned();
        let id = shortcut_id(version);
        let previous = existing.iter().map(|entry| &entry.value).find(|entry| {
            entry.id == id || same_path(&entry.install_path, &install_path)
        });
        let options = preserved_launch_options(version, previous);
        let name = shortcut_name(version);
        desired.push(LivEntry {
            id,
            name,
            install_path,
            executable: executable.clone(),
            arguments: shortcuts::build_launch_url(app, &options),
        });
    }

    if !needs_refresh(&existing, &desired) {
        return Ok(());
    }

    let update = (|| {
        delete_owned_entries(&external)?;
        for entry in &desired {
            write_entry(&external, entry)?;
        }
        Ok::<(), String>(())
    })();
    if let Err(error) = update {
        return match restore_entries(&external, &existing) {
            Ok(()) => Err(error),
            Err(restore_error) => Err(format!("{error}; could not restore previous LIV shortcuts: {restore_error}")),
        };
    }
    Ok(())
}

fn read_owned_entries(external: &RegKey) -> Result<Vec<ExistingEntry>, String> {
    let mut entries = Vec::new();
    for id in external.enum_keys().collect::<Result<Vec<_>, _>>().map_err(|error| error.to_string())? {
        if !id.starts_with(SHORTCUT_PREFIX) {
            continue;
        }
        let key = external.open_subkey_with_flags(&id, KEY_READ).map_err(|error| error.to_string())?;
        let value = LivEntry {
            id: id.clone(),
            name: key.get_value("Name").map_err(|error| format!("Could not read LIV shortcut {id} Name: {error}"))?,
            install_path: key.get_value("InstallPath").map_err(|error| format!("Could not read LIV shortcut {id} InstallPath: {error}"))?,
            executable: key.get_value("Executable").map_err(|error| format!("Could not read LIV shortcut {id} Executable: {error}"))?,
            arguments: key.get_value("Arguments").map_err(|error| format!("Could not read LIV shortcut {id} Arguments: {error}"))?,
        };
        entries.push(ExistingEntry { id, value });
    }
    Ok(entries)
}

fn needs_refresh(existing: &[ExistingEntry], desired: &[LivEntry]) -> bool {
    existing.len() != desired.len() || desired.iter().any(|entry| {
        existing.iter().find(|old| old.id == entry.id)
            .map(|old| &old.value) != Some(entry)
    })
}

fn delete_owned_entries(external: &RegKey) -> Result<(), String> {
    let ids = external.enum_keys().collect::<Result<Vec<_>, _>>().map_err(|error| error.to_string())?;
    for id in ids.into_iter().filter(|id| id.starts_with(SHORTCUT_PREFIX)) {
        match external.delete_subkey_all(&id) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(2) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

fn restore_entries(external: &RegKey, previous: &[ExistingEntry]) -> Result<(), String> {
    delete_owned_entries(external)?;
    for entry in previous {
        write_entry(external, &entry.value)?;
    }
    Ok(())
}

fn write_entry(external: &RegKey, entry: &LivEntry) -> Result<(), String> {
    let (key, _) = external.create_subkey(&entry.id).map_err(|error| error.to_string())?;
    key.set_value("InstallPath", &entry.install_path).map_err(|error| error.to_string())?;
    key.set_value("Executable", &entry.executable).map_err(|error| error.to_string())?;
    key.set_value("Arguments", &entry.arguments).map_err(|error| error.to_string())?;
    key.set_value("Name", &entry.name).map_err(|error| error.to_string())?;
    Ok(())
}

fn preserved_launch_options(version: &Value, previous: Option<&LivEntry>) -> Value {
    let params = previous.and_then(|entry| Url::parse(&entry.arguments).ok())
        .filter(|url| matches!(url.scheme(), "bsmanager" | "bsmanager-preview") && url.host_str() == Some("launch"))
        .map(|url| url.query_pairs().map(|(key, value)| (key.into_owned(), value.into_owned())).collect::<std::collections::HashMap<_, _>>())
        .unwrap_or_default();
    let mut launch_mods = Vec::new();
    for (parameter, name) in [
        ("oculusMode", "oculus"),
        ("desktopMode", "fpfc"),
        ("debug", "debug"),
        ("skipSteam", "skip_steam"),
        ("mapEditor", "editor"),
        ("protonLogs", "proton_logs"),
        ("parallelViews", "parallel_views"),
    ] {
        if params.get(parameter).is_some_and(|value| value == "true") {
            launch_mods.push(name);
        }
    }

    let mut options = json!({ "version": version, "launchMods": launch_mods });
    if let Some(command) = params.get("command").filter(|command| !command.is_empty()) {
        options["command"] = json!(command);
    }
    options
}

fn shortcut_id(version: &Value) -> String {
    let version_name = version.get("name").and_then(Value::as_str).unwrap_or_default();
    let store_name = if version.get("steam").and_then(Value::as_bool).unwrap_or(false) {
        "Steam"
    } else if version.get("oculus").and_then(Value::as_bool).unwrap_or(false) {
        "Oculus"
    } else {
        version_name
    };
    let version_id = version.get("BSVersion").and_then(Value::as_str).unwrap_or_default();
    let raw = [SHORTCUT_PREFIX, store_name, version_id].into_iter()
        .filter(|part| !part.is_empty()).collect::<Vec<_>>().join("-")
        .replace('.', "-").replace(' ', "-");
    raw.chars().map(|character| {
        if matches!(character, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') { '_' } else { character }
    }).collect::<String>().trim_end_matches(['.', ' ']).to_owned()
}

fn shortcut_name(version: &Value) -> String {
    let version_name = version.get("name").and_then(Value::as_str).unwrap_or_default();
    let store_name = if version.get("steam").and_then(Value::as_bool).unwrap_or(false) {
        "Steam"
    } else if version.get("oculus").and_then(Value::as_bool).unwrap_or(false) {
        "Oculus"
    } else {
        version_name
    };
    let version_id = version.get("BSVersion").and_then(Value::as_str).unwrap_or_default();
    let version_label = format!("v{version_id}");
    ["Beat Saber", store_name, version_label.as_str()].into_iter()
        .filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" ")
}

fn same_path(left: &str, right: &str) -> bool {
    let normalize = |value: &str| {
        let path = Path::new(value).canonicalize().unwrap_or_else(|_| Path::new(value).to_path_buf());
        path.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase()
    };
    normalize(left) == normalize(right)
}
