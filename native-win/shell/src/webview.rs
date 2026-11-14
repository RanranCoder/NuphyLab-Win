// WebView2 surface: page-loading rules, the JS↔Rust bridge and downloads.
// Ported from the WKWebView delegate methods in AppHost.swift.
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tao::window::Window;
use wry::{WebView, WebViewBuilder};
use url::Url;

use crate::hotkey::SharedHotkeys;
use crate::win;
use crate::worker::WorkerLog;

/// Injected before page scripts on every navigation. It polyfills the exact
/// surface the UI probes for (`window.webkit.messageHandlers.<name>.postMessage`
/// returning a Promise) on top of wry's `window.ipc.postMessage`. The shell's
/// WH_KEYBOARD_LL hook (M3) owns the keys globally; the page's own keyboard
/// fallback stays disabled because these handlers exist.
const BRIDGE_SHIM: &str = r#"
(() => {
  if (window.__airBridgeInstalled) return;
  window.__airBridgeInstalled = true;
  let seq = 0;
  const pending = new Map();
  window.__airReply = (id, ok) => {
    const resolve = pending.get(id);
    if (resolve) { pending.delete(id); resolve(ok); }
  };
  const make = (name) => ({
    postMessage: (value) => new Promise((resolve) => {
      const id = ++seq;
      pending.set(id, resolve);
      window.ipc.postMessage(JSON.stringify({ bridge: name, id, value }));
    })
  });
  const handlers = { audioControl: make('audioControl'), codexKeys: make('codexKeys') };
  window.webkit = { messageHandlers: handlers };
})();
"#;

fn chrome_or_edge() -> Option<String> {
    let program_dirs = ["ProgramFiles", "ProgramFiles(x86)"];
    let mut candidates = Vec::new();
    for key in program_dirs {
        if let Some(dir) = std::env::var_os(key) {
            let base = std::path::PathBuf::from(&dir);
            candidates.push(base.join("Google/Chrome/Application/chrome.exe"));
            candidates.push(base.join("Microsoft/Edge/Application/msedge.exe"));
        }
    }
    if let Some(local) = std::env::var_os("LocalAppData") {
        candidates.push(std::path::PathBuf::from(local).join("Google/Chrome/Application/chrome.exe"));
    }
    candidates.into_iter().map(|p| p.to_string_lossy().into_owned()).find(|p| std::path::Path::new(p).is_file())
}

/// AppHost decidePolicyFor navigationAction: allow only the local origin and
/// blob: URLs; everything else is cancelled and http(s) links are routed to
/// the system browser (drive.nuphy.io prefers Chrome, then Edge).
pub fn allow_navigation(url: &str, log: &Arc<WorkerLog>) -> bool {
    let parsed = Url::parse(url).ok();
    let host = parsed.as_ref().and_then(|u| u.host_str()).map(str::to_owned);
    let scheme = parsed.as_ref().map(|u| u.scheme().to_owned()).unwrap_or_default();
    if url.starts_with("blob:") || host.as_deref() == Some("127.0.0.1") {
        return true;
    }
    if scheme == "http" || scheme == "https" {
        if host.as_deref() == Some("drive.nuphy.io") {
            let browser = chrome_or_edge();
            let opened = match browser {
                Some(exe) => win::open_with_browser(&exe, url),
                None => false,
            };
            if !opened {
                win::shell_open(url);
            }
        } else {
            win::shell_open(url);
        }
    } else {
        log.append(format!("Blocked navigation to {}\n", url).as_bytes());
    }
    false
}

fn sanitize_download_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') { c } else { '_' })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "download".to_string()
    } else {
        trimmed.chars().take(80).collect()
    }
}

fn handle_audio_control(payload: &Value) -> bool {
    let Some(permission) = payload.get("permission").and_then(Value::as_str) else { return false };
    // Windows has no privacy pane for system-audio loopback (it needs no
    // permission); the microphone deep link is the one that matters.
    match permission {
        "microphone" => win::open_settings("ms-settings:privacy-microphone"),
        "system" => win::open_settings("ms-settings:sound"),
        _ => false,
    }
}

/// codexKeys IPC routing: editing focus, recorder capture mode and slot
/// configuration all land in the shared hotkey state the hook reads.
fn handle_codex_keys(hotkeys: &SharedHotkeys, payload: &Value) -> bool {
    let Some(kind) = payload.get("kind").and_then(Value::as_str) else { return false };
    match kind {
        "editing" => {
            let active = payload.get("active") == Some(&Value::Bool(true));
            hotkeys.lock().unwrap().editing = active;
            true
        }
        "capture" => {
            let session = payload.get("session").and_then(Value::as_str).map(str::to_owned);
            let mut guard = hotkeys.lock().unwrap();
            if payload.get("active") == Some(&Value::Bool(true)) {
                guard.capture_session = session;
                guard.capturing = true;
                guard.captured.clear();
            } else if guard.capture_session == session {
                guard.capture_session = None;
                guard.capturing = false;
                guard.captured.clear();
            }
            true
        }
        "config" => {
            let value = payload.get("value").filter(|v| !v.is_null());
            crate::hotkey::apply_config(hotkeys, value);
            true
        }
        _ => false,
    }
}

fn reply(slot: &Rc<RefCell<Option<WebView>>>, id: i64, ok: bool) {
    if let Some(webview) = slot.borrow().as_ref() {
        let script = format!("window.__airReply && window.__airReply({}, {})", id, if ok { "true" } else { "false" });
        let _ = webview.evaluate_script(&script);
    }
}

pub fn build(
    window: &Window,
    url: &str,
    hotkeys: SharedHotkeys,
    log: Arc<WorkerLog>,
    reply_slot: Rc<RefCell<Option<WebView>>>,
) -> Result<WebView, wry::Error> {
    let navigation_log = log.clone();
    let ipc_hotkeys = hotkeys.clone();
    let ipc_slot = reply_slot.clone();
    let download_log = log.clone();
    WebViewBuilder::new()
        .with_url(url.to_string())
        .with_initialization_script(BRIDGE_SHIM)
        .with_clipboard(true)
        .with_navigation_handler(move |url| allow_navigation(&url, &navigation_log))
        .with_ipc_handler(move |request| {
            let body = request.body();
            let Ok(message) = serde_json::from_str::<Value>(body) else { return };
            let Some(id) = message.get("id").and_then(Value::as_i64) else { return };
            let bridge = message.get("bridge").and_then(Value::as_str).unwrap_or("");
            let payload = message.get("value").cloned().unwrap_or(Value::Null);
            let ok = match bridge {
                "audioControl" => handle_audio_control(&payload),
                "codexKeys" => handle_codex_keys(&ipc_hotkeys, &payload),
                _ => false,
            };
            reply(&ipc_slot, id, ok);
        })
        .with_download_started_handler(move |url, path| {
            let suggested = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            *path = win::downloads_dir().join(format!("NuPhyLab-{}-{}", stamp, sanitize_download_name(&suggested)));
            download_log.append(format!("Download started: {} -> {}\n", url, path.display()).as_bytes());
            true
        })
        .with_download_completed_handler(|_url, path, ok| {
            if !ok {
                if let Some(path) = path {
                    let _ = std::fs::remove_file(&path);
                }
            }
        })
        .build(window)
}
