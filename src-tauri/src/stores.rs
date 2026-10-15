use std::{fs, path::{Path, PathBuf}};

use winreg::{RegKey, enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ}};

const STEAM_APP_ID: &str = "620980";
const OCULUS_GAME_FOLDER: &str = "hyperbolic-magnetism-beat-saber";
const OCULUS_BACKUP_FOLDER: &str = "hyperbolic-magnetism-beat-saber.bsmbak";

pub fn steam_game_folder() -> Option<PathBuf> {
    let steam_root = steam_install_path()?;
    let mut libraries = vec![steam_root.clone()];
    let vdf_path = steam_root.join("steamapps").join("libraryfolders.vdf");
    if let Ok(contents) = fs::read_to_string(vdf_path) {
        for library in parse_library_folders(&contents) {
            add_unique_path(&mut libraries, library);
        }
    }

    libraries.into_iter()
        .map(|library| library.join("steamapps").join("common").join("Beat Saber"))
        .find(|game| game.join("Beat Saber.exe").is_file())
}

pub fn oculus_game_folder() -> Option<PathBuf> {
    oculus_library_paths().into_iter().find_map(|library| {
        let game = library.join("Software").join(OCULUS_GAME_FOLDER);
        if game.join("Beat Saber.exe").is_file() && !is_symlink(&game) {
            return Some(game);
        }
        let backup = library.join("Software").join(OCULUS_BACKUP_FOLDER);
        if backup.join("Beat Saber.exe").is_file() && !is_symlink(&backup) {
            return Some(backup);
        }
        None
    })
}

fn steam_install_path() -> Option<PathBuf> {
    let machine = RegKey::predef(HKEY_LOCAL_MACHINE);
    [r"SOFTWARE\Valve\Steam", r"SOFTWARE\WOW6432Node\Valve\Steam"]
        .into_iter()
        .find_map(|key| {
            machine.open_subkey_with_flags(key, KEY_READ).ok()
                .and_then(|registry| registry.get_value::<String, _>("InstallPath").ok())
                .map(PathBuf::from)
                .filter(|path| path.is_dir())
        })
}

fn oculus_library_paths() -> Vec<PathBuf> {
    let current_user = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(libraries) = current_user.open_subkey_with_flags(r"SOFTWARE\Oculus VR, LLC\Oculus\Libraries", KEY_READ) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for key in libraries.enum_keys().flatten() {
        let Ok(library) = libraries.open_subkey_with_flags(key, KEY_READ) else {
            continue;
        };
        let original_path = library.get_value::<String, _>("OriginalPath").ok();
        let path = original_path.or_else(|| library.get_value::<String, _>("Path").ok());
        if let Some(path) = path.filter(|path| !path.trim().is_empty()) {
            add_unique_path(&mut paths, PathBuf::from(path));
        }
    }
    paths
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

fn add_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    let path = fs::canonicalize(&path).unwrap_or(path);
    let identity = path.to_string_lossy();
    if paths.iter().any(|existing| existing.to_string_lossy().eq_ignore_ascii_case(&identity)) {
        return;
    }
    paths.push(path);
}

#[derive(Default)]
struct VdfNode {
    value: Option<String>,
    children: Vec<(String, VdfNode)>,
}

enum VdfToken {
    Text(String),
    Open,
    Close,
}

fn parse_library_folders(contents: &str) -> Vec<PathBuf> {
    let Some(tokens) = tokenize_vdf(contents) else {
        return Vec::new();
    };
    let mut cursor = 0;
    let Ok(root) = parse_vdf_object(&tokens, &mut cursor, false) else {
        return Vec::new();
    };
    let Some(libraries) = root.iter().find(|(key, _)| key.eq_ignore_ascii_case("libraryfolders"))
        .map(|(_, node)| node)
    else {
        return Vec::new();
    };

    let mut paths = Vec::new();
    for (_, library) in &libraries.children {
        let path = library.value.as_ref().or_else(|| {
            library.children.iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("path"))
                .and_then(|(_, node)| node.value.as_ref())
        });
        if let Some(path) = path.filter(|path| Path::new(path).is_dir()) {
            let has_app_manifest = Path::new(path)
                .join("steamapps")
                .join(format!("appmanifest_{STEAM_APP_ID}.acf"))
                .is_file();
            let listed_app = library.children.iter().any(|(key, value)| {
                key.eq_ignore_ascii_case("apps")
                    && value.children.iter().any(|(app_id, _)| app_id == STEAM_APP_ID)
            });
            if has_app_manifest || listed_app {
                add_unique_path(&mut paths, PathBuf::from(path));
            }
        }
    }
    paths
}

fn tokenize_vdf(contents: &str) -> Option<Vec<VdfToken>> {
    let characters = contents.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < characters.len() {
        match characters[cursor] {
            character if character.is_whitespace() => cursor += 1,
            '/' if characters.get(cursor + 1) == Some(&'/') => {
                cursor += 2;
                while cursor < characters.len() && characters[cursor] != '\n' { cursor += 1; }
            }
            '{' => { tokens.push(VdfToken::Open); cursor += 1; }
            '}' => { tokens.push(VdfToken::Close); cursor += 1; }
            '"' => {
                cursor += 1;
                let mut value = String::new();
                let mut closed = false;
                while cursor < characters.len() {
                    match characters[cursor] {
                        '"' => { cursor += 1; closed = true; break; }
                        '\\' if cursor + 1 < characters.len() => {
                            let next = characters[cursor + 1];
                            if next == '"' || next == '\\' {
                                value.push(next);
                            } else {
                                value.push('\\');
                                value.push(next);
                            }
                            cursor += 2;
                        }
                        character => { value.push(character); cursor += 1; }
                    }
                }
                if !closed { return None; }
                tokens.push(VdfToken::Text(value));
            }
            _ => {
                let start = cursor;
                while cursor < characters.len() && !characters[cursor].is_whitespace() && !matches!(characters[cursor], '{' | '}') {
                    cursor += 1;
                }
                if start == cursor { return None; }
                tokens.push(VdfToken::Text(characters[start..cursor].iter().collect()));
            }
        }
    }
    Some(tokens)
}

fn parse_vdf_object(tokens: &[VdfToken], cursor: &mut usize, expect_close: bool) -> Result<Vec<(String, VdfNode)>, ()> {
    let mut values = Vec::new();
    while *cursor < tokens.len() {
        match &tokens[*cursor] {
            VdfToken::Close if expect_close => {
                *cursor += 1;
                return Ok(values);
            }
            VdfToken::Close => return Err(()),
            VdfToken::Open => return Err(()),
            VdfToken::Text(key) => {
                let key = key.clone();
                *cursor += 1;
                let Some(value) = tokens.get(*cursor) else { return Err(()); };
                match value {
                    VdfToken::Text(value) => {
                        values.push((key, VdfNode { value: Some(value.clone()), children: Vec::new() }));
                        *cursor += 1;
                    }
                    VdfToken::Open => {
                        *cursor += 1;
                        let children = parse_vdf_object(tokens, cursor, true)?;
                        values.push((key, VdfNode { value: None, children }));
                    }
                    VdfToken::Close => return Err(()),
                }
            }
        }
    }
    if expect_close { Err(()) } else { Ok(values) }
}
