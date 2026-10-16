use std::{
    fs,
    path::PathBuf,
    process::Command,
};

use serde_json::Value;
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_deep_link::DeepLinkExt;
use winreg::{RegKey, enums::{HKEY_CURRENT_USER, KEY_READ}};

const BPLIST_EXTENSION: &str = ".bplist";
const BPLIST_PROG_ID: &str = "BSManager.BeatSaberPlaylist";

pub fn migrate_legacy_registered_handlers<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    if app.config().identifier != "io.bsmanager.bsmanager" {
        return Ok(());
    }

    let backup_root = app.path().local_data_dir().map_err(|error| error.to_string())?
        .join("BSManager")
        .join("migration-backups");
    let mut backups = match fs::read_dir(&backup_root) {
        Ok(entries) => entries
            .map(|entry| entry.map(|entry| entry.path()).map_err(|error| error.to_string()))
            .collect::<Result<Vec<_>, _>>()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    backups.retain(|directory| directory.is_dir() && directory.join("migration.json").is_file());
    backups.sort_by_key(|directory| directory.file_name().map(|name| name.to_os_string()));
    let Some(latest_backup) = backups.pop() else { return Ok(()); };

    let migration_path = latest_backup.join("migration.json");
    let contents = fs::read_to_string(&migration_path).map_err(|error| error.to_string())?;
    let contents = contents.trim_start_matches('\u{feff}');
    let migration: Value = serde_json::from_str(contents).map_err(|error| error.to_string())?;
    let Some(registered_protocols) = migration.get("legacyRegisteredProtocols").and_then(Value::as_array) else {
        return Ok(());
    };
    let Some(legacy_executable) = migration.get("legacyExecutable").and_then(Value::as_str) else {
        return Ok(());
    };

    let completed_marker = latest_backup.join("protocols-migrated");
    if completed_marker.is_file() {
        return Ok(());
    }

    for protocol in registered_protocols.iter().filter_map(Value::as_str) {
        if matches!(protocol, "bsmanager" | "beatsaver" | "web+bsmap" | "bsplaylist" | "modelsaber") {
            migrate_protocol(app, protocol, legacy_executable)?;
        }
    }

    if let Some(legacy_prog_id) = migration.get("legacyBplistProgId").and_then(Value::as_str) {
        migrate_bplist_association(app, legacy_prog_id, legacy_executable)?;
    }

    fs::write(completed_marker, b"migrated\n").map_err(|error| error.to_string())
}

fn migrate_protocol<R: Runtime>(app: &AppHandle<R>, protocol: &str, legacy_executable: &str) -> Result<(), String> {
    let current = current_executable()?;
    match registry_command(protocol)? {
        Some(command) if command_matches(&command, &current) => return set(app, protocol, true),
        Some(command) if command_uses_executable(&command, legacy_executable) => {
            delete_current_user_class_key(&format!("Software\\Classes\\{protocol}"))?;
        }
        Some(_) => return Ok(()),
        None => {}
    }
    set(app, protocol, true)
}

fn migrate_bplist_association<R: Runtime>(app: &AppHandle<R>, legacy_prog_id: &str, legacy_executable: &str) -> Result<(), String> {
    if legacy_prog_id.is_empty() || legacy_prog_id.contains('\\') || legacy_prog_id.contains('/') {
        return Ok(());
    }

    let current = current_executable()?;
    let current_prog_id = class_default_value(BPLIST_EXTENSION)?;
    if let Some(prog_id) = current_prog_id.as_deref() {
        let current_command = class_command(prog_id)?;
        if current_command.as_deref().is_some_and(|command| command_matches(command, &current)) {
            return set_bplist_association(app, true);
        }
        let legacy_owner_matches = prog_id.eq_ignore_ascii_case(legacy_prog_id)
            && current_command.as_deref().is_none_or(|command| command_uses_executable(command, legacy_executable));
        if !legacy_owner_matches {
            return Ok(());
        }
        delete_current_user_class_key(&format!("Software\\Classes\\{prog_id}"))?;
    }

    delete_current_user_class_key("Software\\Classes\\.bplist")?;
    set_bplist_association(app, true)
}

fn command_uses_executable(command: &str, executable: &str) -> bool {
    command.to_ascii_lowercase().contains(&executable.to_ascii_lowercase()) && command.contains("%1")
}

fn delete_current_user_class_key(key_path: &str) -> Result<(), String> {
    let user = RegKey::predef(HKEY_CURRENT_USER);
    match user.delete_subkey_all(key_path) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(2) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

pub fn set<R: Runtime>(app: &AppHandle<R>, protocol: &str, enabled: bool) -> Result<(), String> {
    if enabled {
        register(app, protocol)
    } else {
        unregister_if_owned(app, protocol)
    }
}

pub fn is_registered<R: Runtime>(app: &AppHandle<R>, protocol: &str) -> Result<bool, String> {
    let command = registry_command(protocol)?;
    let current = current_executable()?;
    if command.as_deref().is_some_and(|command| command_matches(command, &current)) {
        return Ok(true);
    }
    if let Some(owner) = read_owner(app, protocol)?
        && command.as_deref().is_some_and(|command| command_matches(command, &owner)) {
        return Ok(true);
    }
    app.deep_link().is_registered(protocol).map_err(|error| error.to_string())
}

pub fn set_bplist_association<R: Runtime>(app: &AppHandle<R>, enabled: bool) -> Result<(), String> {
    if enabled { register_bplist_association(app) } else { unregister_bplist_association_if_owned(app) }
}

pub fn is_bplist_associated<R: Runtime>(app: &AppHandle<R>) -> Result<bool, String> {
    let current = current_executable()?;
    let owner = read_owner(app, "bplist")?;
    let Some(prog_id) = class_default_value(BPLIST_EXTENSION)? else { return Ok(false); };
    if !prog_id.eq_ignore_ascii_case(BPLIST_PROG_ID) { return Ok(false); }
    let command = class_command(BPLIST_PROG_ID)?;
    Ok(command.as_deref().is_some_and(|command| {
        command_matches(command, &current)
            || owner.as_deref().is_some_and(|owner| command_matches(command, owner))
    }))
}

fn register_bplist_association<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let directory = backup_directory(app)?;
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let current = current_executable()?;
    if !is_bplist_associated(app)? {
        backup_class_key(BPLIST_EXTENSION, &directory.join("bplist-extension.reg"))?;
        backup_class_key(BPLIST_PROG_ID, &directory.join("bplist-progid.reg"))?;
    }

    let result = (|| {
        let user = RegKey::predef(HKEY_CURRENT_USER);
        let (extension, _) = user.create_subkey(format!("Software\\Classes\\{BPLIST_EXTENSION}"))
            .map_err(|error| error.to_string())?;
        extension.set_value("", &BPLIST_PROG_ID).map_err(|error| error.to_string())?;

        let (class, _) = user.create_subkey(format!("Software\\Classes\\{BPLIST_PROG_ID}"))
            .map_err(|error| error.to_string())?;
        class.set_value("", &"Beat Saber Playlist").map_err(|error| error.to_string())?;
        let (icon, _) = class.create_subkey("DefaultIcon").map_err(|error| error.to_string())?;
        icon.set_value("", &format!("\"{current}\",0")).map_err(|error| error.to_string())?;
        let (command, _) = class.create_subkey("shell\\open\\command").map_err(|error| error.to_string())?;
        command.set_value("", &format!("\"{current}\" \"%1\"")).map_err(|error| error.to_string())?;
        write_owner(app, "bplist", &current)
    })();
    if let Err(error) = result {
        let _ = restore_class_key(BPLIST_PROG_ID, &directory.join("bplist-progid.reg"));
        let _ = restore_class_key(BPLIST_EXTENSION, &directory.join("bplist-extension.reg"));
        return Err(error);
    }
    Ok(())
}

fn unregister_bplist_association_if_owned<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let Some(owner) = read_owner(app, "bplist")?.or_else(|| current_executable().ok()) else { return Ok(()); };
    if !is_bplist_associated(app)? { return Ok(()); }
    let command = class_command(BPLIST_PROG_ID)?;
    if !command.as_deref().is_some_and(|command| command_matches(command, &owner)) { return Ok(()); }
    let directory = backup_directory(app)?;
    restore_class_key(BPLIST_PROG_ID, &directory.join("bplist-progid.reg"))?;
    restore_class_key(BPLIST_EXTENSION, &directory.join("bplist-extension.reg"))
}

fn class_default_value(class_name: &str) -> Result<Option<String>, String> {
    let user = RegKey::predef(HKEY_CURRENT_USER);
    match user.open_subkey_with_flags(format!("Software\\Classes\\{class_name}"), KEY_READ) {
        Ok(key) => match key.get_value::<String, _>("") {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.raw_os_error() == Some(2) => Ok(None),
            Err(error) => Err(error.to_string()),
        },
        Err(error) if error.raw_os_error() == Some(2) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn class_command(class_name: &str) -> Result<Option<String>, String> {
    let key_path = format!("Software\\Classes\\{class_name}\\shell\\open\\command");
    let user = RegKey::predef(HKEY_CURRENT_USER);
    match user.open_subkey_with_flags(key_path, KEY_READ) {
        Ok(key) => match key.get_value::<String, _>("") {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.raw_os_error() == Some(2) => Ok(None),
            Err(error) => Err(error.to_string()),
        },
        Err(error) if error.raw_os_error() == Some(2) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn backup_class_key(class_name: &str, destination: &PathBuf) -> Result<(), String> {
    let key_path = format!("Software\\Classes\\{class_name}");
    let user = RegKey::predef(HKEY_CURRENT_USER);
    match user.open_subkey_with_flags(&key_path, KEY_READ) {
        Ok(_) => {
            let export_key = format!("HKCU\\{key_path}");
            let result = Command::new("reg.exe").args(["export", &export_key])
                .arg(destination).arg("/y").output().map_err(|error| error.to_string())?;
            if !result.status.success() {
                return Err(format!("Could not back up the {class_name} file association: {}", String::from_utf8_lossy(&result.stderr).trim()));
            }
        }
        Err(error) if error.raw_os_error() == Some(2) => {
            if destination.exists() { fs::remove_file(destination).map_err(|error| error.to_string())?; }
        }
        Err(error) => return Err(error.to_string()),
    }
    Ok(())
}

fn restore_class_key(class_name: &str, backup: &PathBuf) -> Result<(), String> {
    let key_path = format!("Software\\Classes\\{class_name}");
    let user = RegKey::predef(HKEY_CURRENT_USER);
    match user.delete_subkey_all(&key_path) {
        Ok(()) => {},
        Err(error) if error.raw_os_error() == Some(2) => {},
        Err(error) => return Err(error.to_string()),
    }
    if backup.is_file() {
        let result = Command::new("reg.exe").arg("import").arg(backup)
            .output().map_err(|error| error.to_string())?;
        if !result.status.success() {
            return Err(format!("Could not restore the previous {class_name} file association: {}", String::from_utf8_lossy(&result.stderr).trim()));
        }
    }
    Ok(())
}

fn register<R: Runtime>(app: &AppHandle<R>, protocol: &str) -> Result<(), String> {
    let directory = backup_directory(app)?;
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let current = current_executable()?;
    let existing_command = registry_command(protocol)?;
    let saved_owner = read_owner(app, protocol)?;
    let already_owned = existing_command.as_deref().is_some_and(|command| {
        command_matches(command, &current)
            || saved_owner.as_deref().is_some_and(|owner| command_matches(command, owner))
    });
    let registry_backup = directory.join(format!("{protocol}.reg"));

    if !already_owned {
        backup_current_user_key(protocol, &registry_backup)?;
    }
    write_owner(app, protocol, &current)?;
    if let Err(error) = app.deep_link().register(protocol) {
        if !already_owned {
            restore_current_user_key(protocol, &registry_backup)?;
        }
        return Err(error.to_string());
    }
    Ok(())
}

fn unregister_if_owned<R: Runtime>(app: &AppHandle<R>, protocol: &str) -> Result<(), String> {
    let directory = backup_directory(app)?;
    let backup = directory.join(format!("{protocol}.reg"));
    let current_command = registry_command(protocol)?;
    let owner = read_owner(app, protocol)?.or_else(|| current_executable().ok());
    let Some(owner) = owner else { return Ok(()); };
    if !current_command.as_deref().is_some_and(|command| command_matches(command, &owner)) {
        return Ok(());
    }
    restore_current_user_key(protocol, &backup)
}

fn backup_current_user_key(protocol: &str, destination: &PathBuf) -> Result<(), String> {
    let key_path = format!("Software\\Classes\\{protocol}");
    let user = RegKey::predef(HKEY_CURRENT_USER);
    match user.open_subkey_with_flags(&key_path, KEY_READ) {
        Ok(_) => {
            let export_key = format!("HKCU\\{key_path}");
            let result = Command::new("reg.exe").args(["export", &export_key])
                .arg(destination).arg("/y").output().map_err(|error| error.to_string())?;
            if !result.status.success() {
                return Err(format!("Could not back up the existing {protocol} protocol registration: {}", String::from_utf8_lossy(&result.stderr).trim()));
            }
        }
        Err(error) if error.raw_os_error() == Some(2) => {
            if destination.exists() { fs::remove_file(destination).map_err(|error| error.to_string())?; }
        }
        Err(error) => return Err(error.to_string()),
    }
    Ok(())
}

fn restore_current_user_key(protocol: &str, backup: &PathBuf) -> Result<(), String> {
    let key_path = format!("Software\\Classes\\{protocol}");
    let user = RegKey::predef(HKEY_CURRENT_USER);
    match user.delete_subkey_all(&key_path) {
        Ok(()) => {},
        Err(error) if error.raw_os_error() == Some(2) => {},
        Err(error) => return Err(error.to_string()),
    }
    if backup.is_file() {
        let result = Command::new("reg.exe").arg("import").arg(backup)
            .output().map_err(|error| error.to_string())?;
        if !result.status.success() {
            return Err(format!("Could not restore the previous {protocol} protocol registration: {}", String::from_utf8_lossy(&result.stderr).trim()));
        }
    }
    Ok(())
}

fn registry_command(protocol: &str) -> Result<Option<String>, String> {
    let path = format!("Software\\Classes\\{protocol}\\shell\\open\\command");
    let user = RegKey::predef(HKEY_CURRENT_USER);
    match user.open_subkey_with_flags(path, KEY_READ) {
        Ok(command) => command.get_value::<String, _>("").map(Some).map_err(|error| error.to_string()),
        Err(error) if error.raw_os_error() == Some(2) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn backup_directory<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    let product = if app.config().identifier == "io.bsmanager.bsmanager.preview" {
        "BSManager Preview"
    } else {
        "BSManager"
    };
    app.path().local_data_dir().map(|path| path.join(product).join("protocol-backups"))
        .map_err(|error| error.to_string())
}

fn owner_path<R: Runtime>(app: &AppHandle<R>, protocol: &str) -> Result<PathBuf, String> {
    Ok(backup_directory(app)?.join(format!("{protocol}.owner")))
}

fn read_owner<R: Runtime>(app: &AppHandle<R>, protocol: &str) -> Result<Option<String>, String> {
    match fs::read_to_string(owner_path(app, protocol)?) {
        Ok(owner) => Ok(Some(owner.trim().to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn write_owner<R: Runtime>(app: &AppHandle<R>, protocol: &str, owner: &str) -> Result<(), String> {
    let path = owner_path(app, protocol)?;
    if let Some(parent) = path.parent() { fs::create_dir_all(parent).map_err(|error| error.to_string())?; }
    fs::write(path, owner).map_err(|error| error.to_string())
}

fn current_executable() -> Result<String, String> {
    tauri::utils::platform::current_exe().map(|path| path.display().to_string()).map_err(|error| error.to_string())
}

fn command_matches(command: &str, executable: &str) -> bool {
    command.trim().eq_ignore_ascii_case(&format!("\"{executable}\" \"%1\""))
}
