use std::{fs, path::PathBuf, sync::atomic::{AtomicBool, Ordering}, time::Duration};

use tauri::{AppHandle, Manager, Runtime, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent, webview::PageLoadEvent};
use tokio::{sync::mpsc, time::sleep};
use url::Url;

use crate::{SerializedError, serialized_error, steam};

const LOGIN_URL: &str = "https://secure.oculus.com";
const REDIRECT_HOST: &str = "secure.oculus.com";
const REDIRECT_PATH: &str = "/my/profile";
const COOKIE_NAME: &str = "oc_ac_at";
const TOKEN_FILE: &str = "meta-session.bin";
const AUTH_WINDOW: &str = "meta-auth";
const CLEANER_WINDOW: &str = "meta-cookie-cleaner";

enum AuthEvent {
    Page(Url),
    Closed,
}

pub async fn login<R: Runtime>(
    app: &AppHandle<R>,
    cancelled: &AtomicBool,
    keep_token: bool,
) -> Result<serde_json::Value, SerializedError> {
    if let Some(existing) = app.get_webview_window(AUTH_WINDOW) {
        let _ = existing.close();
    }
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let login_url = Url::parse(LOGIN_URL).map_err(|error| serialized_error(error.to_string(), "INVALID_URL"))?;
    let data_directory = webview_data_directory(app)?;
    fs::create_dir_all(&data_directory).map_err(|error| serialized_error(error.to_string(), "META_SESSION_STORAGE_FAILED"))?;

    let navigation_tx = event_tx.clone();
    let load_tx = event_tx.clone();
    let close_tx = event_tx;
    let window = WebviewWindowBuilder::new(app, AUTH_WINDOW, WebviewUrl::External(login_url))
        .title("Meta login")
        .inner_size(650.0, 800.0)
        .center()
        .decorations(true)
        .resizable(true)
        .focused(true)
        .visible(true)
        .data_directory(data_directory)
        .on_navigation(move |url| {
            let _ = navigation_tx.send(AuthEvent::Page(url.clone()));
            url.scheme() == "https"
        })
        .on_page_load(move |_, payload| {
            if payload.event() == PageLoadEvent::Finished {
                let _ = load_tx.send(AuthEvent::Page(payload.url().clone()));
            }
        })
        .build()
        .map_err(|error| serialized_error(error.to_string(), "META_LOGIN_WINDOW_FAILED"))?;
    window.on_window_event(move |event| {
        if matches!(event, WindowEvent::Destroyed) {
            let _ = close_tx.send(AuthEvent::Closed);
        }
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10 * 60);
    let token_result = loop {
        tokio::select! {
            event = event_rx.recv() => match event {
                Some(AuthEvent::Page(url)) if is_profile_url(&url) => {
                    match token_from_cookies(window.clone()).await {
                        Ok(Some(token)) => break Ok(token),
                        Ok(None) => {}
                        Err(error) => log::warn!("Could not read Meta login cookies: {}", error.message),
                    }
                }
                Some(AuthEvent::Closed) | None => break Err(serialized_error("Meta login window closed by user", "META_LOGIN_WINDOW_CLOSED_BY_USER")),
                _ => {}
            },
            _ = sleep(Duration::from_millis(250)) => {
                if cancelled.load(Ordering::Relaxed) {
                    break Err(serialized_error("Meta login was cancelled", "REQUEST_CANCELLED"));
                }
                if tokio::time::Instant::now() >= deadline {
                    break Err(serialized_error("Trying to get Meta user token timed out", "META_LOGIN_TIMED_OUT"));
                }
            }
        }
    };

    let cleanup_result = if keep_token {
        match &token_result {
            Ok(token) => save_token(app, token).await,
            Err(_) => Ok(()),
        }
    } else {
        match remove_saved_token(app).await {
            Ok(()) => clear_all_meta_cookies(app).await,
            Err(error) => Err(error),
        }
    };
    let _ = window.close();
    cleanup_result?;
    token_result.map(serde_json::Value::String)
}

pub async fn delete_session<R: Runtime>(app: &AppHandle<R>) -> Result<serde_json::Value, SerializedError> {
    remove_saved_token(app).await?;
    clear_all_meta_cookies(app).await?;
    if let Some(existing) = app.get_webview_window(AUTH_WINDOW) { let _ = existing.close(); }
    Ok(serde_json::Value::Null)
}

pub async fn session_exists<R: Runtime>(app: &AppHandle<R>) -> serde_json::Value {
    let Some(token) = load_saved_token(app).await else { return serde_json::Value::Bool(false) };
    serde_json::Value::Bool(valid_token(&token))
}

fn is_profile_url(url: &Url) -> bool {
    url.scheme() == "https" && url.host_str() == Some(REDIRECT_HOST) && url.path().starts_with(REDIRECT_PATH)
}

async fn token_from_cookies<R: Runtime>(window: WebviewWindow<R>) -> Result<Option<String>, SerializedError> {
    let url = Url::parse("https://secure.oculus.com/my/profile")
        .map_err(|error| serialized_error(error.to_string(), "INVALID_URL"))?;
    let cookies = tokio::task::spawn_blocking(move || window.cookies_for_url(url))
        .await
        .map_err(|error| serialized_error(error.to_string(), "META_COOKIE_READ_FAILED"))?
        .map_err(|error| serialized_error(error.to_string(), "META_COOKIE_READ_FAILED"))?;
    Ok(cookies.into_iter().find(|cookie| cookie.name() == COOKIE_NAME && valid_token(cookie.value()))
        .map(|cookie| cookie.value().to_owned()))
}

async fn clear_cookies<R: Runtime>(window: WebviewWindow<R>) -> Result<(), SerializedError> {
    tokio::task::spawn_blocking(move || {
        let cookies = window.cookies().map_err(|error| error.to_string())?;
        for cookie in cookies {
            if cookie.domain().is_some_and(is_meta_domain) {
                window.delete_cookie(cookie).map_err(|error| error.to_string())?;
            }
        }
        Ok::<(), String>(())
    }).await
        .map_err(|error| serialized_error(error.to_string(), "META_SESSION_CLEAR_FAILED"))?
        .map_err(|error| serialized_error(error, "META_SESSION_CLEAR_FAILED"))
}

async fn clear_all_meta_cookies<R: Runtime>(app: &AppHandle<R>) -> Result<(), SerializedError> {
    if let Some(window) = app.get_webview_window(AUTH_WINDOW) {
        return clear_cookies(window).await;
    }
    if let Some(existing) = app.get_webview_window(CLEANER_WINDOW) {
        let _ = existing.close();
    }
    let data_directory = webview_data_directory(app)?;
    fs::create_dir_all(&data_directory).map_err(|error| serialized_error(error.to_string(), "META_SESSION_STORAGE_FAILED"))?;
    let url = Url::parse(LOGIN_URL).map_err(|error| serialized_error(error.to_string(), "INVALID_URL"))?;
    let window = WebviewWindowBuilder::new(app, CLEANER_WINDOW, WebviewUrl::External(url))
        .title("Meta session cleanup")
        .inner_size(1.0, 1.0)
        .decorations(false)
        .resizable(false)
        .focused(false)
        .skip_taskbar(true)
        .visible(false)
        .data_directory(data_directory)
        .on_navigation(|url| url.scheme() == "https")
        .build()
        .map_err(|error| serialized_error(error.to_string(), "META_SESSION_CLEAR_FAILED"))?;
    let result = clear_cookies(window.clone()).await;
    let _ = window.close();
    result
}

fn is_meta_domain(domain: &str) -> bool {
    let domain = domain.trim_start_matches('.');
    domain.eq_ignore_ascii_case("oculus.com") || domain.to_ascii_lowercase().ends_with(".oculus.com")
}

async fn save_token<R: Runtime>(app: &AppHandle<R>, token: &str) -> Result<(), SerializedError> {
    let path = token_file(app)?;
    let parent = path.parent().ok_or_else(|| serialized_error("Meta session storage path is invalid", "META_SESSION_STORAGE_FAILED"))?;
    fs::create_dir_all(parent).map_err(|error| serialized_error(error.to_string(), "META_SESSION_STORAGE_FAILED"))?;
    let encrypted = steam::protect_secret(token.as_bytes())
        .map_err(|error| serialized_error(error.to_string(), "META_SESSION_STORAGE_FAILED"))?;
    fs::write(path, encrypted).map_err(|error| serialized_error(error.to_string(), "META_SESSION_STORAGE_FAILED"))
}

async fn load_saved_token<R: Runtime>(app: &AppHandle<R>) -> Option<String> {
    let encrypted = fs::read(token_file(app).ok()?).ok()?;
    let decrypted = steam::unprotect_secret(&encrypted).ok()?;
    String::from_utf8(decrypted).ok().filter(|token| valid_token(token))
}

async fn remove_saved_token<R: Runtime>(app: &AppHandle<R>) -> Result<(), SerializedError> {
    match fs::remove_file(token_file(app)?) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(serialized_error(error.to_string(), "META_SESSION_STORAGE_FAILED")),
    }
}

fn token_file<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, SerializedError> {
    app.path().app_config_dir()
        .map(|path| path.join(TOKEN_FILE))
        .map_err(|error| serialized_error(error.to_string(), "META_SESSION_STORAGE_FAILED"))
}

fn webview_data_directory<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, SerializedError> {
    app.path().app_data_dir()
        .map(|path| path.join("meta-auth-webview"))
        .map_err(|error| serialized_error(error.to_string(), "META_SESSION_STORAGE_FAILED"))
}

fn valid_token(token: &str) -> bool {
    if token.is_empty() || token.chars().any(|character| matches!(character, '%' | '|' | ':'))
        || (!token.starts_with("FRL") && !token.starts_with("OC"))
    {
        return false;
    }
    !token.as_bytes().windows(17).any(|part| {
        part.starts_with(b"OC") && part[2..].iter().all(u8::is_ascii_digit)
    })
}
