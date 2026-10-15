use std::time::Duration;

use reqwest::Client;
use serde_json::{Value, json};
use url::Url;

use crate::{SerializedError, serialized_error};

const MODEL_SABER_API: &str = "https://modelsaber.com/api/v2/get.php";

pub async fn search_models(query: &Value) -> Result<Value, SerializedError> {
    let results = query_models(query).await?;
    let models = results.as_object().map(|values| {
        values.values().filter_map(|value| {
            if value.get("name").and_then(Value::as_str).is_none() { return None; }
            let mut model = value.clone();
            sanitize_field(&mut model, "name");
            sanitize_field(&mut model, "author");
            Some(model)
        }).collect::<Vec<_>>()
    }).unwrap_or_default();
    Ok(json!(models))
}

pub async fn get_model_by_id(id: &Value) -> Value {
    let query = json!({
        "start": 0,
        "end": 1,
        "platform": "pc",
        "filter": [{ "type": "id", "value": id }]
    });
    match query_models(&query).await {
        Ok(results) => {
            let key = id.as_str().map(str::to_owned).unwrap_or_else(|| id.to_string());
            results.get(&key).cloned().unwrap_or(Value::Null)
        }
        Err(error) => {
            log::warn!("ModelSaber lookup failed: {}", error.message);
            Value::Null
        }
    }
}

async fn query_models(query: &Value) -> Result<Value, SerializedError> {
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(45))
        .user_agent("BSManager/1.6.0")
        .build()
        .map_err(|error| serialized_error(error.to_string(), "MODEL_SABER_CLIENT_FAILED"))?;
    let mut parameters = Vec::<(String, String)>::new();
    for name in ["type", "platform", "start", "end", "sort", "sortDirection"] {
        if let Some(value) = query.get(name).filter(|value| !value.is_null()) {
            if let Some(value) = value.as_str() {
                if !value.is_empty() { parameters.push((name.to_owned(), value.to_owned())); }
            } else if value.is_number() {
                parameters.push((name.to_owned(), value.to_string()));
            }
        }
    }
    let filters = query.get("filter").and_then(Value::as_array).map(|filters| {
        filters.iter().filter_map(|filter| {
            let kind = filter.get("type")?.as_str()?;
            let value = filter.get("value")?;
            if value.is_null() { return None; }
            let value = value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string());
            let expression = if kind == "searchName" { value } else { format!("{kind}:{value}") };
            Some(if filter.get("isNegative").and_then(Value::as_bool).unwrap_or(false) {
                format!("-{expression}")
            } else { expression })
        }).collect::<Vec<_>>().join(",")
    }).unwrap_or_default();
    if !filters.is_empty() { parameters.push(("filter".to_owned(), filters)); }

    let mut url = Url::parse(MODEL_SABER_API)
        .map_err(|error| serialized_error(error.to_string(), "MODEL_SABER_URL_FAILED"))?;
    url.query_pairs_mut().extend_pairs(parameters);
    let response = client.get(url).send().await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| serialized_error(error.to_string(), "MODEL_SABER_REQUEST_FAILED"))?;
    let body = response.bytes().await
        .map_err(|error| serialized_error(error.to_string(), "MODEL_SABER_RESPONSE_FAILED"))?;
    serde_json::from_slice(&body)
        .map_err(|error| serialized_error(error.to_string(), "MODEL_SABER_INVALID_RESPONSE"))
}

fn sanitize_field(model: &mut Value, field: &str) {
    let Some(value) = model.get(field).and_then(Value::as_str).map(strip_tags) else { return; };
    if let Some(object) = model.as_object_mut() {
        object.insert(field.to_owned(), json!(value));
    }
}

fn strip_tags(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut in_tag = false;
    for character in value.chars() {
        match character {
            '<' if !in_tag => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => output.push(character),
            _ => {}
        }
    }
    output
}
