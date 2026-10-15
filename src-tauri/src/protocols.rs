use std::{
    fs,
    path::PathBuf,
    process::Command,
};

use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_deep_link::DeepLinkExt;
use winreg::{RegKey, enums::{HKEY_CURRENT_USER, KEY_READ}};

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
