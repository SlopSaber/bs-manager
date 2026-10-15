use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{Mutex, OnceLock, mpsc::{self, Receiver, Sender}},
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Runtime};
use serde_json::Value;

#[derive(Clone, Deserialize, Serialize)]
pub struct CachedMapInfo {
    #[serde(rename = "hash")]
    pub hash: String,
    #[serde(rename = "mapInfo")]
    pub map_info: Value,
}

#[derive(Default)]
struct CacheState {
    path: Option<PathBuf>,
    entries: HashMap<String, CachedMapInfo>,
}

static CACHE: OnceLock<Mutex<CacheState>> = OnceLock::new();
static SAVE_CHANNEL: OnceLock<Sender<()>> = OnceLock::new();

pub fn initialize<R: Runtime>(app: &AppHandle<R>) {
    let Ok(settings_path) = crate::settings_path(app) else { return; };
    let Some(settings_directory) = settings_path.parent() else { return; };
    let path = settings_directory.join("CachedData").join("map-info-cache.json");
    let entries = match fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<HashMap<String, CachedMapInfo>>(&bytes) {
            Ok(entries) => entries,
            Err(error) => {
                log::warn!("Could not read local map-info cache: {error}");
                HashMap::new()
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
        Err(error) => {
            log::warn!("Could not read local map-info cache: {error}");
            HashMap::new()
        }
    };
    if let Ok(mut cache) = CACHE.get_or_init(|| Mutex::new(CacheState::default())).lock() {
        cache.path = Some(path);
        cache.entries = entries;
    }
    start_save_worker();
}

pub fn get(dirname: &str) -> Option<CachedMapInfo> {
    CACHE.get_or_init(|| Mutex::new(CacheState::default())).lock().ok()?
        .entries.get(dirname).cloned()
}

pub fn remember(dirname: &str, map_info: Value, hash: &str) {
    let cache = CACHE.get_or_init(|| Mutex::new(CacheState::default()));
    let Ok(mut cache) = cache.lock() else { return; };
    if cache.path.is_none() { return; }
    cache.entries.insert(dirname.to_owned(), CachedMapInfo { hash: hash.to_owned(), map_info });
    drop(cache);
    if let Some(sender) = SAVE_CHANNEL.get() { let _ = sender.send(()); }
}

pub fn remove(dirname: &str) {
    let cache = CACHE.get_or_init(|| Mutex::new(CacheState::default()));
    let Ok(mut cache) = cache.lock() else { return; };
    if cache.entries.remove(dirname).is_some() {
        drop(cache);
        if let Some(sender) = SAVE_CHANNEL.get() { let _ = sender.send(()); }
    }
}

fn start_save_worker() {
    if SAVE_CHANNEL.get().is_some() { return; }
    let (sender, receiver) = mpsc::channel();
    if thread::Builder::new().name("bsmanager-map-cache".to_owned())
        .spawn(move || save_worker(receiver)).is_ok()
    {
        let _ = SAVE_CHANNEL.set(sender);
    }
}

fn save_worker(receiver: Receiver<()>) {
    loop {
        if receiver.recv().is_err() { return; }
        while matches!(receiver.recv_timeout(Duration::from_secs(1)), Ok(())) {}
        let snapshot = {
            let cache = CACHE.get_or_init(|| Mutex::new(CacheState::default()));
            let Ok(cache) = cache.lock() else { continue; };
            cache.path.as_ref().map(|path| (path.clone(), cache.entries.clone()))
        };
        let Some((path, entries)) = snapshot else { continue; };
        let Some(parent) = path.parent() else { continue; };
        if let Err(error) = fs::create_dir_all(parent)
            .and_then(|()| serde_json::to_vec(&entries).map_err(std::io::Error::other))
            .and_then(|bytes| fs::write(&path, bytes))
        {
            log::warn!("Could not save local map-info cache: {error}");
        }
    }
}
