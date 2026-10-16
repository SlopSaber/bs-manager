use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{Manager, Runtime, WebviewWindow, WindowEvent};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
    ICoreWebView2_19,
};
use windows_core::Interface;

pub fn register<R: Runtime>(window: &WebviewWindow<R>) {
    let app = window.app_handle().clone();
    let label = window.label().to_owned();
    let minimized = window.is_minimized().unwrap_or(false);
    let low_memory = AtomicBool::new(minimized);
    if minimized { set_target(window, true); }

    window.on_window_event(move |event| {
        if !matches!(event, WindowEvent::Resized(_) | WindowEvent::Focused(_)) { return; }
        let Some(window) = app.get_webview_window(&label) else { return; };
        let Ok(minimized) = window.is_minimized() else { return; };
        if low_memory.swap(minimized, Ordering::Relaxed) != minimized {
            set_target(&window, minimized);
        }
    });
}

fn set_target<R: Runtime>(window: &WebviewWindow<R>, minimized: bool) {
    let level = if minimized {
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
    } else {
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
    };
    if let Err(error) = window.with_webview(move |platform| {
        // Tauri runs this closure on the webview thread with a live controller.
        let result = unsafe {
            platform.controller().CoreWebView2()
                .and_then(|webview| webview.cast::<ICoreWebView2_19>())
                .and_then(|webview| webview.SetMemoryUsageTargetLevel(level))
        };
        if let Err(error) = result {
            log::debug!("WebView2 memory target is unavailable: {error}");
        }
    }) {
        log::debug!("Could not schedule WebView2 memory target: {error}");
    }
}
