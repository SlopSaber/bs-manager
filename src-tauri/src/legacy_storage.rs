use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use rusty_leveldb::{DB, LdbIterator, Options};
use uuid::Uuid;

const ELECTRON_FILE_ORIGIN_PREFIX: &[u8] = b"_file://\0";
const PREFERENCE_KEYS: &[&str] = &[
    // Renderer preferences from the Electron baseline. Keep this explicit so
    // unrelated localStorage values and legacy credentials are not imported.
    "first-color",
    "second-color",
    "theme",
    "language",
    "supported_languages",
    "default_mods",
    "default-shared-folders",
    "playlist-sort-order",
    "map-sort-order",
    "pinned-launch-mods",
    "custom-versions",
    "launch-mods",
    "custom-launch-options",
    "launch-command",
    "selectedStoreToDownload",
    "audio-level",
    "not-remind-oneclick",
    "not-remind-mods-models",
    "not-show-bs-version-outdated-modal",
    "not-show-bs-version-outdated-notification",
    "not-show-outdated-mods-notification",
    "not-confirm-delete-map",
    "not-confirm-delete-model",
    "dont-remind-admin",
    "dont-remind-vr-runtime",
    "accepted-mods-disclaimer",
    "STEAM-USERNAME",
];

pub fn read_preferences(source: &Path, cache_root: &Path) -> Result<BTreeMap<String, String>, String> {
    if !source.join("CURRENT").is_file() {
        return Ok(BTreeMap::new());
    }

    let snapshot_parent = cache_root.join("legacy-localstorage-import");
    fs::create_dir_all(&snapshot_parent)
        .map_err(|error| format!("Could not create temporary preference folder: {error}"))?;
    let snapshot_path = snapshot_parent.join(Uuid::new_v4().to_string());
    let _cleanup = TemporaryDirectory(snapshot_path.clone());
    copy_stable_snapshot(source, &snapshot_path)?;

    let mut options = Options::default();
    options.create_if_missing = false;
    options.error_if_exists = false;
    let mut database = DB::open(&snapshot_path, options)
        .map_err(|error| format!("Could not read the legacy preference database: {error}"))?;

    let mut preferences = BTreeMap::new();
    {
        let mut iterator = database.new_iter()
            .map_err(|error| format!("Could not scan the legacy preference database: {error}"))?;
        while let Some((key, value)) = iterator.next() {
            let Some(encoded_key) = key.strip_prefix(ELECTRON_FILE_ORIGIN_PREFIX) else { continue };
            let Some(key) = decode_chromium_string(encoded_key) else { continue };
            if !PREFERENCE_KEYS.contains(&key.as_str()) {
                continue;
            }
            let Some(value) = decode_chromium_string(&value) else { continue };
            preferences.insert(key, value);
        }
    }
    database.close()
        .map_err(|error| format!("Could not finish reading legacy preferences: {error}"))?;
    Ok(preferences)
}

fn decode_chromium_string(bytes: &[u8]) -> Option<String> {
    let (&format, encoded) = bytes.split_first()?;
    match format {
        1 => Some(encoded.iter().copied().map(char::from).collect()),
        0 if encoded.len() % 2 == 0 => {
            let units = encoded.chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>();
            String::from_utf16(&units).ok()
        }
        _ => String::from_utf8(bytes.to_vec()).ok(),
    }
}

#[derive(PartialEq, Eq)]
struct FileStamp {
    name: OsString,
    length: u64,
    modified: SystemTime,
}

fn file_stamps(directory: &Path) -> io::Result<Vec<FileStamp>> {
    let mut stamps = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Legacy preference database contains a symbolic link"));
        }
        if metadata.is_file() {
            stamps.push(FileStamp {
                name: entry.file_name(),
                length: metadata.len(),
                modified: metadata.modified()?,
            });
        }
    }
    stamps.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(stamps)
}

fn copy_stable_snapshot(source: &Path, destination: &Path) -> Result<(), String> {
    for _ in 0..3 {
        let before = file_stamps(source)
            .map_err(|error| format!("Could not inspect the legacy preference database: {error}"))?;
        fs::create_dir_all(destination)
            .map_err(|error| format!("Could not create a preference snapshot: {error}"))?;
        let copy_result = before.iter().try_for_each(|stamp| {
            fs::copy(source.join(&stamp.name), destination.join(&stamp.name)).map(|_| ())
        });
        if let Err(error) = copy_result {
            let _ = fs::remove_dir_all(destination);
            return Err(format!("Could not copy the legacy preference database: {error}"));
        }

        let after = file_stamps(source)
            .map_err(|error| format!("Could not verify the legacy preference database: {error}"))?;
        if before == after {
            return Ok(());
        }
        fs::remove_dir_all(destination)
            .map_err(|error| format!("Could not retry the preference snapshot: {error}"))?;
    }
    Err("The legacy preference database kept changing; close the old BSManager and retry".to_owned())
}

struct TemporaryDirectory(PathBuf);

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
