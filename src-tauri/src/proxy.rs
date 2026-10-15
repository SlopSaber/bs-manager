use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Runtime};

static USE_SYSTEM_PROXY: AtomicBool = AtomicBool::new(false);

pub fn configure<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let enabled = crate::read_settings(app)?
        .get("use-system-proxy")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    set_enabled(enabled);
    Ok(())
}

pub fn set_enabled(enabled: bool) {
    USE_SYSTEM_PROXY.store(enabled, Ordering::Relaxed);
}

pub fn client_builder() -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder();
    if USE_SYSTEM_PROXY.load(Ordering::Relaxed) {
        builder
    } else {
        builder.no_proxy()
    }
}
