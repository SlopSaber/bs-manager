
use std::{fs, process::Command, path::Path};
use std::env::current_exe;
use std::fs::File;
use std::path::PathBuf;
use log::{error, info, LevelFilter};
use simplelog::{CombinedLogger, Config, WriteLogger};

const EXECUTABLE_NAME: &str = "Beat Saber.exe";
const ENV_FILE_PREFIX: &str = "--bsmanager-env-file=";
const MAX_ENVIRONMENT_FILE_SIZE: usize = 1024 * 1024;
const AUTORIZED_ARGS: [&str; 8] = [
    "fpfc",
    "-vrmode",
    "oculus",
    "--verbose",
    "--no-yeet",
    "--disable-autoupdate",
    "--combine-logs",
    "editor"
];

fn main() {

    let default_log_file_path = current_exe().unwrap().with_file_name("bs-admin-start.log");
    let args: Vec<String> = std::env::args().collect();

    let log_file_path: PathBuf = if let Some(pos) = args.iter().position(|arg| arg == "--log-path") {
        if let Some(log_path) = args.get(pos + 1) {
            PathBuf::from(log_path)
        } else {
            default_log_file_path
        }
    } else {
        default_log_file_path
    };

    let environment_result = read_environment_file(&args);
    CombinedLogger::init(
        vec![
            WriteLogger::new(LevelFilter::Info, Config::default(), File::create(log_file_path).unwrap()),
        ]
    ).unwrap();

    let environment = match environment_result {
        Ok(environment) => environment,
        Err(message) => return error!("Failed to load launch environment: {}", message),
    };

    let executable_path = Path::new(&args[1]);

    if executable_path.file_name().unwrap() != EXECUTABLE_NAME {
        return error!("Executable path is not Beat Saber.exe.");
    }

    if !executable_path.exists() || !executable_path.is_file() {
        return error!("Executable path is not valid.");
    }

    let mut command = Command::new(executable_path);

    if let Some(parent_dir) = executable_path.parent() {
        command.current_dir(parent_dir);
    }

    for arg in args.iter().skip(2) {
        if !AUTORIZED_ARGS.contains(&arg.as_str()) {
            continue;
        }
        command.arg(arg);
    }

    command.env("SteamAppId", "620980").envs(environment);

    let res = command.spawn();

    if let Err(e) = res {
        return error!("Failed to start Beat Saber ({}): {}", executable_path.display(), e);
    }

    info!("Beat Saber started ({})", executable_path.display());
}

fn read_environment_file(arguments: &[String]) -> Result<Vec<(String, String)>, String> {
    let Some(path) = arguments.iter().find_map(|argument| argument.strip_prefix(ENV_FILE_PREFIX)) else {
        return Ok(Vec::new());
    };
    let path = Path::new(path);
    let temporary_directory = std::env::temp_dir().canonicalize().map_err(|error| error.to_string())?;
    let canonical_path = path.canonicalize().map_err(|error| error.to_string())?;
    let file_name = canonical_path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    if canonical_path.parent() != Some(temporary_directory.as_path())
        || !file_name.starts_with("bsmanager-admin-env-")
        || canonical_path.extension().and_then(|extension| extension.to_str()) != Some("bin")
    {
        return Err("environment file is outside the expected temporary location".to_owned());
    }
    let metadata = fs::metadata(&canonical_path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_ENVIRONMENT_FILE_SIZE as u64 {
        let _ = fs::remove_file(&canonical_path);
        return Err("environment file is invalid or too large".to_owned());
    }
    let read_result = fs::read(&canonical_path);
    let _ = fs::remove_file(&canonical_path);
    let bytes = read_result.map_err(|error| error.to_string())?;

    let mut position = 0usize;
    let count = read_u32(&bytes, &mut position)? as usize;
    if count > 4096 {
        return Err("environment variable count exceeds limit".to_owned());
    }

    let mut environment = Vec::with_capacity(count);
    for _ in 0..count {
        let name_length = read_u32(&bytes, &mut position)? as usize;
        let value_length = read_u32(&bytes, &mut position)? as usize;
        let name = read_string(&bytes, &mut position, name_length)?;
        let value = read_string(&bytes, &mut position, value_length)?;
        if name.is_empty() || name.contains('=') || name.contains('\0') || value.contains('\0') {
            return Err("environment variable contains an invalid name or value".to_owned());
        }
        environment.push((name, value));
    }
    if position != bytes.len() {
        return Err("environment file contains trailing data".to_owned());
    }
    Ok(environment)
}

fn read_u32(bytes: &[u8], position: &mut usize) -> Result<u32, String> {
    let end = position.checked_add(4).ok_or_else(|| "environment file offset overflow".to_owned())?;
    let value = bytes.get(*position..end).ok_or_else(|| "environment file is truncated".to_owned())?;
    *position = end;
    Ok(u32::from_le_bytes(value.try_into().map_err(|_| "environment file is truncated".to_owned())?))
}

fn read_string(bytes: &[u8], position: &mut usize, length: usize) -> Result<String, String> {
    let end = position.checked_add(length).ok_or_else(|| "environment file offset overflow".to_owned())?;
    let value = bytes.get(*position..end).ok_or_else(|| "environment file is truncated".to_owned())?;
    *position = end;
    String::from_utf8(value.to_vec()).map_err(|error| error.to_string())
}
