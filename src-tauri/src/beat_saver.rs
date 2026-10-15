use std::sync::{Mutex, OnceLock};

use reqwest::Client;
use serde_json::{Value, json};
use url::Url;

use crate::{SerializedError, serialized_error};

const API_BASE: &str = "https://api.beatsaver.com";
static CLIENT: OnceLock<Client> = OnceLock::new();
static MAP_CACHE: OnceLock<Mutex<std::collections::HashMap<String, Value>>> = OnceLock::new();

pub async fn dispatch(channel: &str, args: &Value) -> Result<Value, SerializedError> {
    match channel {
        "bsv-search-map" => Ok(search_maps(args).await),
        "bsv-get-map-details-from-hashs" => get_map_details_from_hashes(args).await,
        "bsv-get-map-details-by-id" => get_map_details_by_id(args).await,
        "bsv-search-playlist" => search_playlists(args).await,
        "bsv-get-playlist-details-by-id" => get_playlist_details_by_id(args).await,
        _ => Err(serialized_error("Unknown BeatSaver route", "UNKNOWN_ROUTE")),
    }
}

async fn search_maps(args: &Value) -> Value {
    let query = args.get("q").and_then(Value::as_str).unwrap_or_default().trim();
    if let Some(id) = parse_bsr_code(query) {
        if args.get("page").and_then(Value::as_u64).unwrap_or_default() > 0 {
            return json!([]);
        }
        return match map_by_id(id).await {
            Ok(map) => json!([map]),
            Err(error) => {
                log::error!("BeatSaver map lookup failed: {}", error.message);
                json!([])
            }
        };
    }

    match search_maps_request(args).await {
        Ok(response) => {
            if let Some(id) = response.get("redirect").and_then(Value::as_str) {
                if args.get("page").and_then(Value::as_u64).unwrap_or_default() > 0 {
                    return json!([]);
                }
                match map_by_id(id).await {
                    Ok(map) => json!([map]),
                    Err(error) => {
                        log::error!("BeatSaver map redirect failed: {}", error.message);
                        json!([])
                    }
                }
            } else {
                response.get("docs").cloned().unwrap_or_else(|| json!([]))
            }
        }
        Err(error) => {
            log::error!("BeatSaver map search failed: {}", error.message);
            json!([])
        }
    }
}

async fn search_maps_request(args: &Value) -> Result<Value, SerializedError> {
    let page = args.get("page").and_then(Value::as_u64).unwrap_or_default();
    let mut url = api_url(&["search", "text", &page.to_string()])?;
    let mut params = Vec::new();
    append_value(&mut params, "includeEmpty", args.get("includeEmpty"));
    append_value(&mut params, "sortOrder", args.get("sortOrder"));
    append_value(&mut params, "q", args.get("q"));

    if let Some(filter) = args.get("filter").and_then(Value::as_object) {
        const FILTER_KEYS: &[&str] = &[
            "automapper", "chroma", "cinema", "noodle", "me", "curated", "verified",
            "from", "to", "fullSpread", "leaderboard", "installed", "minDuration",
            "maxDuration", "minNps", "maxNps",
        ];
        for key in FILTER_KEYS {
            let value = filter.get(*key);
            if *key == "leaderboard" && value.is_none_or(Value::is_null) {
                params.push(("leaderboard".to_owned(), "All".to_owned()));
            } else {
                append_value(&mut params, key, value);
            }
        }
        let mut tags = Vec::new();
        collect_tags(filter.get("enabledTags"), false, &mut tags);
        collect_tags(filter.get("excludedTags"), true, &mut tags);
        if !tags.is_empty() {
            params.push(("tags".to_owned(), tags.join("|")));
        }
    }
    url.query_pairs_mut().extend_pairs(params.iter().map(|(key, value)| (key.as_str(), value.as_str())));
    get_json(url).await
}

async fn get_map_details_from_hashes(args: &Value) -> Result<Value, SerializedError> {
    let hashes = args.as_array()
        .ok_or_else(|| serialized_error("Map hashes must be an array", "INVALID_HASHES"))?;
    let cache = MAP_CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut result = Vec::new();
    let mut missing = Vec::new();
    {
        let cache = cache.lock().map_err(|_| serialized_error("BeatSaver cache is unavailable", "CACHE_FAILED"))?;
        for hash in hashes.iter().filter_map(Value::as_str) {
            let normalized = hash.to_lowercase();
            if let Some(map) = cache.get(&normalized) {
                result.push(map.clone());
            } else {
                missing.push(normalized);
            }
        }
    }

    for chunk in missing.chunks(50) {
        if chunk.is_empty() {
            continue;
        }
        let joined = chunk.join(",");
        let response = match get_json(api_url(&["maps", "hash", &joined])?).await {
            Ok(response) => response,
            Err(error) => {
                log::error!("BeatSaver hash lookup failed: {}", error.message);
                continue;
            }
        };
        let maps = extract_maps(response);
        {
            let mut cache = cache.lock().map_err(|_| serialized_error("BeatSaver cache is unavailable", "CACHE_FAILED"))?;
            for map in &maps {
                if let Some(hash) = map.pointer("/versions/0/hash").and_then(Value::as_str) {
                    cache.insert(hash.to_lowercase(), map.clone());
                }
            }
        }
        result.extend(maps);
    }
    Ok(json!(result))
}

async fn get_map_details_by_id(args: &Value) -> Result<Value, SerializedError> {
    let id = args.as_str().ok_or_else(|| serialized_error("Map ID must be a string", "INVALID_ID"))?;
    map_by_id(id).await
}

async fn map_by_id(id: &str) -> Result<Value, SerializedError> {
    let url = api_url(&["maps", "id", id])?;
    get_json(url).await
}

async fn search_playlists(args: &Value) -> Result<Value, SerializedError> {
    let page = args.get("page").and_then(Value::as_u64).unwrap_or_default();
    let mut url = api_url(&["playlists", "search", &page.to_string()])?;
    let mut params = Vec::new();
    if let Some(values) = args.as_object() {
        for (key, value) in values {
            if key != "page" {
                append_value(&mut params, key, Some(value));
            }
        }
    }
    url.query_pairs_mut().extend_pairs(params.iter().map(|(key, value)| (key.as_str(), value.as_str())));
    let response = get_json(url).await?;
    Ok(response.get("docs").cloned().unwrap_or_else(|| json!([])))
}

async fn get_playlist_details_by_id(args: &Value) -> Result<Value, SerializedError> {
    let id = args.get("id").and_then(Value::as_str)
        .ok_or_else(|| serialized_error("Playlist ID must be a string", "INVALID_ID"))?;
    let page = args.get("page").and_then(Value::as_u64).unwrap_or_default();
    let url = api_url(&["playlists", "id", id, &page.to_string()])?;
    get_json(url).await
}

async fn get_json(url: Url) -> Result<Value, SerializedError> {
    let response = CLIENT.get_or_init(Client::new)
        .get(url)
        .header(reqwest::header::USER_AGENT, format!("BSManager/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(15))
        .send().await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| serialized_error(error.to_string(), "BEATSAVER_REQUEST_FAILED"))?;
    let body = response.text().await
        .map_err(|error| serialized_error(error.to_string(), "BEATSAVER_RESPONSE_FAILED"))?;
    serde_json::from_str(&body)
        .map_err(|error| serialized_error(error.to_string(), "BEATSAVER_RESPONSE_FAILED"))
}

fn api_url(segments: &[&str]) -> Result<Url, SerializedError> {
    let mut url = Url::parse(API_BASE)
        .map_err(|error| serialized_error(error.to_string(), "BEATSAVER_URL_FAILED"))?;
    url.path_segments_mut().map_err(|_| serialized_error("BeatSaver API URL is invalid", "BEATSAVER_URL_FAILED"))?
        .extend(segments.iter().copied());
    Ok(url)
}

fn append_value(params: &mut Vec<(String, String)>, key: &str, value: Option<&Value>) {
    let Some(value) = value.filter(|value| !value.is_null()) else { return; };
    let scalar = match value {
        Value::String(value) => value.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        _ => return,
    };
    params.push((key.to_owned(), scalar));
}

fn collect_tags(value: Option<&Value>, excluded: bool, tags: &mut Vec<String>) {
    let Some(values) = value.and_then(Value::as_array) else { return; };
    tags.extend(values.iter().filter_map(Value::as_str).map(|tag| {
        if excluded { format!("!{tag}") } else { tag.to_owned() }
    }));
}

fn extract_maps(value: Value) -> Vec<Value> {
    if value.get("id").is_some() {
        return vec![value];
    }
    match value {
        Value::Array(values) => values.into_iter().filter(|map| map.get("id").is_some()).collect(),
        Value::Object(values) => values.into_values().filter(|map| map.get("id").is_some()).collect(),
        _ => Vec::new(),
    }
}

fn parse_bsr_code(value: &str) -> Option<&str> {
    let mut parts = value.split_whitespace();
    let command = parts.next()?;
    let id = parts.next()?;
    if command.eq_ignore_ascii_case("!bsr") && parts.next().is_none() {
        Some(id)
    } else {
        None
    }
}
