use std::{
    collections::HashMap,
    io::Read,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

use flate2::read::GzDecoder;
use prost::{Enumeration, Message};
use reqwest::header::{ETAG, IF_NONE_MATCH};
use serde_json::{Value, json};
use tauri::{AppHandle, Runtime};
use tokio::sync::OnceCell;

const SOURCES: [&str; 2] = [
    "https://raw.githubusercontent.com/Zagrios/beat-saber-scraped-maps/master/song_details_cache_v1.gz",
    "https://cdn.jsdelivr.net/gh/Zagrios/beat-saber-scraped-maps@master/song_details_cache_v1.gz",
];
const ETAG_KEY: &str = "song-details-cache-etag";

static INITIALIZED: OnceCell<()> = OnceCell::const_new();
static CACHE: OnceLock<Mutex<CacheIndex>> = OnceLock::new();

#[derive(Default)]
struct CacheIndex {
    by_hash: HashMap<String, Arc<Value>>,
    by_id: HashMap<String, Arc<Value>>,
}

#[derive(Clone, PartialEq, Message)]
struct RawCache {
    #[prost(message, repeated, tag = "1")]
    songs: Vec<RawSong>,
    #[prost(uint32, tag = "2")]
    last_updated: u32,
    #[prost(uint32, tag = "3")]
    total: u32,
    #[prost(message, optional, tag = "4")]
    uploaders: Option<RawUploaders>,
    #[prost(string, repeated, tag = "5")]
    difficulty_labels: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
struct RawUploaders {
    #[prost(string, repeated, tag = "1")]
    names: Vec<String>,
    #[prost(uint32, repeated, tag = "2")]
    ids: Vec<u32>,
}

#[derive(Clone, PartialEq, Message)]
struct RawSong {
    #[prost(uint32, tag = "1")]
    id_int: u32,
    #[prost(uint32, repeated, tag = "2")]
    hash_indices: Vec<u32>,
    #[prost(string, tag = "3")]
    name: String,
    #[prost(uint32, tag = "4")]
    duration: u32,
    #[prost(message, optional, tag = "5")]
    uploader_ref: Option<RawUploaderRef>,
    #[prost(uint32, tag = "6")]
    uploaded_at: u32,
    #[prost(enumeration = "RawTag", repeated, tag = "7")]
    tags: Vec<i32>,
    #[prost(bool, tag = "8")]
    ranked: bool,
    #[prost(bool, tag = "9")]
    qualified: bool,
    #[prost(bool, tag = "10")]
    curated: bool,
    #[prost(bool, tag = "11")]
    bl_ranked: bool,
    #[prost(bool, tag = "12")]
    bl_qualified: bool,
    #[prost(uint32, tag = "13")]
    up_votes: u32,
    #[prost(uint32, tag = "14")]
    down_votes: u32,
    #[prost(uint32, tag = "15")]
    downloads: u32,
    #[prost(bool, tag = "16")]
    automapper: bool,
    #[prost(message, repeated, tag = "17")]
    difficulties: Vec<RawDifficulty>,
}

#[derive(Clone, PartialEq, Message)]
struct RawUploaderRef {
    #[prost(uint32, tag = "1")]
    uploader_ref_index: u32,
    #[prost(bool, tag = "2")]
    verified: bool,
}

#[derive(Clone, PartialEq, Message)]
struct RawDifficulty {
    #[prost(enumeration = "RawDifficultyLabel", tag = "1")]
    difficulty: i32,
    #[prost(enumeration = "RawCharacteristic", tag = "2")]
    characteristic: i32,
    #[prost(uint32, tag = "3")]
    label_index: u32,
    #[prost(uint32, tag = "4")]
    stars_t100: u32,
    #[prost(uint32, tag = "5")]
    stars_bl_t100: u32,
    #[prost(uint32, tag = "6")]
    njs_t100: u32,
    #[prost(uint32, tag = "7")]
    nps_t100: u32,
    #[prost(int32, tag = "8")]
    offset_t100: i32,
    #[prost(bool, tag = "9")]
    chroma: bool,
    #[prost(bool, tag = "10")]
    cinema: bool,
    #[prost(bool, tag = "11")]
    me: bool,
    #[prost(bool, tag = "12")]
    ne: bool,
    #[prost(uint32, tag = "13")]
    bombs: u32,
    #[prost(uint32, tag = "14")]
    notes: u32,
    #[prost(uint32, tag = "15")]
    obstacles: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
enum RawDifficultyLabel {
    Unknown = 0,
    Easy = 1,
    Normal = 2,
    Hard = 3,
    Expert = 4,
    ExpertPlus = 5,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
enum RawCharacteristic {
    Unknown = 0,
    Standard = 1,
    OneSaber = 2,
    NoArrows = 3,
    Lawless = 4,
    Lightshow = 5,
    Legacy = 6,
    NinetyDegree = 7,
    ThreeSixtyDegree = 8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
enum RawTag {
    Unknown = 0,
    Dance = 1,
    Swing = 2,
    Nightcore = 3,
    Folk = 4,
    Family = 5,
    Ambient = 6,
    Funk = 7,
    Jazz = 8,
    Soul = 9,
    Speedcore = 10,
    Punk = 11,
    Rb = 12,
    Holiday = 13,
    Vocaloid = 14,
    JRock = 15,
    Trance = 16,
    DrumBass = 17,
    Comedy = 18,
    Instrumental = 19,
    Hardcore = 20,
    KPop = 21,
    Indie = 22,
    Techno = 23,
    House = 24,
    Game = 25,
    Film = 26,
    Alt = 27,
    Dubstep = 28,
    Metal = 29,
    Anime = 30,
    Hiphop = 31,
    JPop = 32,
    Rock = 33,
    Pop = 34,
    Electronic = 35,
    ClassicalOrchestral = 36,
    Accuracy = 37,
    Balanced = 38,
    Challenge = 39,
    Dancestyle = 40,
    Fitness = 41,
    Speed = 42,
    Tech = 43,
}

pub async fn initialize<R: Runtime>(app: &AppHandle<R>) {
    INITIALIZED.get_or_init(|| async {
        if let Err(error) = initialize_inner(app).await {
            log::warn!("Could not initialize the scraped song details cache: {error}");
        }
    }).await;
}

pub fn by_hash(hash: &str) -> Option<Value> {
    CACHE.get_or_init(|| Mutex::new(CacheIndex::default())).lock().ok()?
        .by_hash.get(&hash.to_ascii_lowercase()).map(|details| details.as_ref().clone())
}

pub fn by_id(id: &str) -> Option<Value> {
    CACHE.get_or_init(|| Mutex::new(CacheIndex::default())).lock().ok()?
        .by_id.get(&id.to_ascii_lowercase()).map(|details| details.as_ref().clone())
}

async fn initialize_inner<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let path = cache_path(app)?;
    let backup = path.with_extension("bak");
    let mut cache_is_valid = false;
    for candidate in [&path, &backup] {
        match tokio::fs::read(candidate).await {
            Ok(bytes) => match tokio::task::spawn_blocking(move || decode_cache(&bytes)).await {
                Ok(Ok(index)) => {
                    replace_cache(index)?;
                    cache_is_valid = true;
                    break;
                }
                Ok(Err(error)) => log::warn!("Could not decode scraped song details cache: {error}"),
                Err(error) => log::warn!("Could not decode scraped song details cache: {error}"),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => log::warn!("Could not read scraped song details cache: {error}"),
        }
    }

    let etag = if cache_is_valid {
        crate::read_settings(app).ok()
            .and_then(|settings| settings.get(ETAG_KEY).and_then(Value::as_str).map(str::to_owned))
    } else { None };
    refresh_cache(app, &path, etag.as_deref()).await
}

async fn refresh_cache<R: Runtime>(app: &AppHandle<R>, path: &PathBuf, etag: Option<&str>) -> Result<(), String> {
    let client = crate::proxy::client_builder()
        .connect_timeout(std::time::Duration::from_secs(20))
        .timeout(std::time::Duration::from_secs(120))
        .build().map_err(|error| error.to_string())?;
    let mut last_error = None;

    for source in SOURCES {
        let mut request = client.get(source);
        if let Some(etag) = etag { request = request.header(IF_NONE_MATCH, etag); }
        let response = match request.send().await {
            Ok(response) if response.status() == reqwest::StatusCode::NOT_MODIFIED => return Ok(()),
            Ok(response) => response,
            Err(error) => { last_error = Some(error.to_string()); continue; }
        };
        if !response.status().is_success() {
            last_error = Some(format!("source returned HTTP {}", response.status()));
            continue;
        }
        let new_etag = response.headers().get(ETAG).and_then(|value| value.to_str().ok()).map(str::to_owned);
        let compressed = match response.bytes().await {
            Ok(bytes) => bytes.to_vec(),
            Err(error) => { last_error = Some(error.to_string()); continue; }
        };
        let decoded = match tokio::task::spawn_blocking(move || decode_gzip_cache(&compressed)).await {
            Ok(Ok(decoded)) => decoded,
            Ok(Err(error)) => { last_error = Some(error); continue; }
            Err(error) => { last_error = Some(error.to_string()); continue; }
        };
        let (proto_bytes, index) = decoded;
        if let Err(error) = persist_cache(path, &proto_bytes).await {
            last_error = Some(error);
            continue;
        }
        replace_cache(index)?;
        persist_etag(app, new_etag).await?;
        log::info!("Scraped song details cache refreshed from {source}");
        return Ok(());
    }

    if let Some(error) = last_error {
        log::warn!("Could not refresh scraped song details cache: {error}");
    }
    Ok(())
}

async fn persist_cache(path: &PathBuf, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| "Song cache path is invalid".to_owned())?;
    tokio::fs::create_dir_all(parent).await.map_err(|error| error.to_string())?;
    let temporary = path.with_extension("tmp");
    let backup = path.with_extension("bak");
    tokio::fs::write(&temporary, bytes).await.map_err(|error| error.to_string())?;
    let had_previous = tokio::fs::try_exists(path).await.map_err(|error| error.to_string())?;
    if had_previous {
        if tokio::fs::try_exists(&backup).await.map_err(|error| error.to_string())? {
            tokio::fs::remove_file(&backup).await.map_err(|error| error.to_string())?;
        }
        tokio::fs::rename(path, &backup).await.map_err(|error| error.to_string())?;
    }
    if let Err(error) = tokio::fs::rename(&temporary, path).await {
        if had_previous { let _ = tokio::fs::rename(&backup, path).await; }
        return Err(error.to_string());
    }
    if had_previous { let _ = tokio::fs::remove_file(backup).await; }
    Ok(())
}

async fn persist_etag<R: Runtime>(app: &AppHandle<R>, etag: Option<String>) -> Result<(), String> {
    let mut settings = crate::read_settings(app)?;
    if let Some(etag) = etag {
        settings.insert(ETAG_KEY.to_owned(), json!(etag));
    } else {
        settings.remove(ETAG_KEY);
    }
    crate::write_settings(app, &settings)
}

fn cache_path<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    let settings = crate::settings_path(app)?;
    let directory = settings.parent().ok_or_else(|| "Settings path is invalid".to_owned())?;
    Ok(directory.join("CachedData").join("song-details-cache"))
}

fn replace_cache(index: CacheIndex) -> Result<(), String> {
    let cache = CACHE.get_or_init(|| Mutex::new(CacheIndex::default()));
    *cache.lock().map_err(|_| "Song cache is unavailable".to_owned())? = index;
    Ok(())
}

fn decode_gzip_cache(compressed: &[u8]) -> Result<(Vec<u8>, CacheIndex), String> {
    let mut decoder = GzDecoder::new(compressed);
    let mut proto = Vec::new();
    decoder.read_to_end(&mut proto).map_err(|error| error.to_string())?;
    let index = decode_cache(&proto)?;
    Ok((proto, index))
}

fn decode_cache(bytes: &[u8]) -> Result<CacheIndex, String> {
    let cache = RawCache::decode(bytes).map_err(|error| error.to_string())?;
    let uploaders = cache.uploaders.unwrap_or_default();
    let mut index = CacheIndex::default();
    for song in cache.songs {
        let hash = song.hash_indices.iter()
            .filter_map(|value| char::from_digit(*value, 16))
            .collect::<String>();
        let id = format!("{:x}", song.id_int);
        let uploader = song.uploader_ref.unwrap_or(RawUploaderRef { uploader_ref_index: 0, verified: false });
        let uploader_index = uploader.uploader_ref_index as usize;
        let difficulties = song.difficulties.into_iter().map(|difficulty| {
            let label = cache.difficulty_labels.get(difficulty.label_index as usize).cloned().unwrap_or_default();
            json!({
                "difficulty": difficulty_name(difficulty.difficulty),
                "characteristic": characteristic_name(difficulty.characteristic),
                "label": label,
                "stars": f64::from(difficulty.stars_t100) / 100.0,
                "starsBL": f64::from(difficulty.stars_bl_t100) / 100.0,
                "njs": f64::from(difficulty.njs_t100) / 100.0,
                "nps": f64::from(difficulty.nps_t100) / 100.0,
                "offset": f64::from(difficulty.offset_t100) / 100.0,
                "chroma": difficulty.chroma,
                "cinema": difficulty.cinema,
                "me": difficulty.me,
                "ne": difficulty.ne,
                "bombs": difficulty.bombs,
                "notes": difficulty.notes,
                "obstacles": difficulty.obstacles,
            })
        }).collect::<Vec<_>>();
        let tags = song.tags.into_iter().filter_map(tag_name).collect::<Vec<_>>();
        let details = json!({
            "id": id,
            "hash": hash,
            "name": song.name,
            "duration": song.duration,
            "uploader": {
                "name": uploaders.names.get(uploader_index).cloned().unwrap_or_default(),
                "id": uploaders.ids.get(uploader_index).copied().unwrap_or_default(),
                "verified": uploader.verified,
            },
            "uploadedAt": song.uploaded_at,
            "tags": tags,
            "ranked": song.ranked,
            "qualified": song.qualified,
            "curated": song.curated,
            "blRanked": song.bl_ranked,
            "blQualified": song.bl_qualified,
            "upVotes": song.up_votes,
            "downVotes": song.down_votes,
            "downloads": song.downloads,
            "automapper": song.automapper,
            "difficulties": difficulties,
        });
        let details = Arc::new(details);
        index.by_hash.insert(details["hash"].as_str().unwrap_or_default().to_ascii_lowercase(), details.clone());
        index.by_id.insert(id.to_ascii_lowercase(), details);
    }
    Ok(index)
}

fn difficulty_name(value: i32) -> &'static str {
    match RawDifficultyLabel::try_from(value).unwrap_or(RawDifficultyLabel::Unknown) {
        RawDifficultyLabel::Easy => "Easy",
        RawDifficultyLabel::Normal => "Normal",
        RawDifficultyLabel::Hard => "Hard",
        RawDifficultyLabel::Expert => "Expert",
        _ => "ExpertPlus",
    }
}

fn characteristic_name(value: i32) -> &'static str {
    match RawCharacteristic::try_from(value).unwrap_or(RawCharacteristic::Unknown) {
        RawCharacteristic::OneSaber => "OneSaber",
        RawCharacteristic::NoArrows => "NoArrows",
        RawCharacteristic::Lawless => "Lawless",
        RawCharacteristic::Lightshow => "Lightshow",
        RawCharacteristic::Legacy => "Legacy",
        RawCharacteristic::NinetyDegree => "90Degree",
        RawCharacteristic::ThreeSixtyDegree => "360Degree",
        _ => "Standard",
    }
}

fn tag_name(value: i32) -> Option<&'static str> {
    Some(match RawTag::try_from(value).ok()? {
        RawTag::Dance => "dance",
        RawTag::Swing => "swing",
        RawTag::Nightcore => "nightcore",
        RawTag::Folk => "folk-acoustic",
        RawTag::Family => "kids-family",
        RawTag::Ambient => "ambient",
        RawTag::Funk => "funk-disco",
        RawTag::Jazz => "jazz",
        RawTag::Soul => "soul",
        RawTag::Speedcore => "speedcore",
        RawTag::Punk => "punk",
        RawTag::Rb => "rb",
        RawTag::Holiday => "holiday",
        RawTag::Vocaloid => "vocaloid",
        RawTag::JRock => "j-rock",
        RawTag::Trance => "trance",
        RawTag::DrumBass => "drum-and-bass",
        RawTag::Comedy => "comedy-meme",
        RawTag::Instrumental => "instrumental",
        RawTag::Hardcore => "hardcore",
        RawTag::KPop => "k-pop",
        RawTag::Indie => "indie",
        RawTag::Techno => "techno",
        RawTag::House => "house",
        RawTag::Game => "video-game-soundtrack",
        RawTag::Film => "tv-movie-soundtrack",
        RawTag::Alt => "alternative",
        RawTag::Dubstep => "dubstep",
        RawTag::Metal => "metal",
        RawTag::Anime => "anime",
        RawTag::Hiphop => "hip-hop-rap",
        RawTag::JPop => "j-pop",
        RawTag::Rock => "rock",
        RawTag::Pop => "pop",
        RawTag::Electronic => "electronic",
        RawTag::ClassicalOrchestral => "classical-orchestral",
        RawTag::Accuracy => "accuracy",
        RawTag::Balanced => "balanced",
        RawTag::Challenge => "challenge",
        RawTag::Dancestyle => "dance-style",
        RawTag::Fitness => "fitness",
        RawTag::Speed => "speed",
        RawTag::Tech => "tech",
        RawTag::Unknown => return None,
    })
}
