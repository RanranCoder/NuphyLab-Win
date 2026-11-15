// WebView2 surface: page-loading rules, the JS↔Rust bridge and downloads.
// Ported from the WKWebView delegate methods in AppHost.swift.
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
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
///
/// Reply protocol: `__airReply(id, ok)` keeps the legacy boolean resolution for
/// audioControl/codexKeys; `__airReply(id, ok, value)` resolves the Promise
/// with the decoded JSON object and `__airReply(id, false, undefined, code)`
/// rejects it, which is what the typingSounds UI expects from macOS
/// (`reply(nil, "settings")` arrives as a rejected Promise with the code).
const BRIDGE_SHIM: &str = r#"
(() => {
  if (window.__airBridgeInstalled) return;
  window.__airBridgeInstalled = true;
  let seq = 0;
  const pending = new Map();
  window.__airReply = (id, ok, value, errorText) => {
    const entry = pending.get(id);
    if (!entry) return;
    pending.delete(id);
    if (value === undefined && errorText === undefined) { entry.resolve(ok); return; }
    if (ok) entry.resolve(value === undefined ? null : value);
    else entry.reject(new Error(errorText === undefined ? 'unavailable' : errorText));
  };
  const make = (name) => ({
    postMessage: (value) => new Promise((resolve, reject) => {
      const id = ++seq;
      pending.set(id, { resolve, reject });
      window.ipc.postMessage(JSON.stringify({ bridge: name, id, value }));
    })
  });
  const handlers = { audioControl: make('audioControl'), codexKeys: make('codexKeys'), typingSounds: make('typingSounds'), keyboardPocket: make('keyboardPocket') };
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

/// Builds the `__airReply` script for the typingSounds bridge: a resolved
/// Promise carrying the TypingStatus object, or a rejected Promise carrying the
/// issue code (both embedded as JSON literals — a JSON object/string is a valid
/// JS expression apart from U+2028/2029, which are escaped).
pub fn bridge_reply_script(id: i64, ok: bool, payload: Option<&str>, error: Option<&str>) -> String {
    let escape = |text: &str| text.replace('\u{2028}', "\\u2028").replace('\u{2029}', "\\u2029");
    let value_part = payload.map(|json| format!(", {}", escape(json))).unwrap_or_default();
    let error_part = error
        .map(|code| format!(", undefined, {}", escape(&serde_json::to_string(code).unwrap_or_else(|_| "\"unavailable\"".into()))))
        .unwrap_or_default();
    if value_part.is_empty() && error_part.is_empty() {
        format!("window.__airReply && window.__airReply({}, {})", id, if ok { "true" } else { "false" })
    } else {
        format!("window.__airReply && window.__airReply({}, {}{}{})", id, if ok { "true" } else { "false" }, value_part, error_part)
    }
}

/// typingSounds IPC → POST /api/typing on the local server. The reply is
/// marshalled back through the event loop (`UserEvent::BridgeReply`) because
/// the HTTP call must not block the UI thread and the WebView only runs on it.
pub fn handle_typing_sounds(
    state: &Arc<Mutex<crate::ShellState>>,
    proxy: &tao::event_loop::EventLoopProxy<crate::UserEvent>,
    log: &Arc<WorkerLog>,
    id: i64,
    payload: &Value,
) {
    let target = {
        let guard = state.lock().unwrap();
        guard.base_url.clone().map(|base| (base, guard.token.clone()))
    };
    let Some((base, token)) = target else {
        // Server not ready (or gone): fail the call like a transport error.
        let _ = proxy.send_event(crate::UserEvent::BridgeReply { id, ok: false, payload: None, error: None });
        return;
    };
    let proxy = proxy.clone();
    let log = log.clone();
    let body = payload.to_string();
    std::thread::spawn(move || {
        let outcome = post_typing(&base, &token, &body);
        match outcome {
            Ok(result) => {
                let _ = proxy.send_event(crate::UserEvent::BridgeReply { id, ok: true, payload: Some(result), error: None });
            }
            Err(code) => {
                log.append(format!("[typingSounds] /api/typing failed: {code}\n").as_bytes());
                let _ = proxy.send_event(crate::UserEvent::BridgeReply { id, ok: false, payload: None, error: Some(code) });
            }
        }
    });
}

/// One blocking /api/typing round trip. Returns the server's `result` JSON on
/// success or the Mac-style issue code ("settings"/"audio"/…) on failure.
fn post_typing(base: &str, token: &str, body: &str) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).build();
    let request = agent.post(&format!("{}/api/typing", base));
    let request = if token.is_empty() { request } else { request.set("X-Air-Token", token) };
    let response = request
        .set("Content-Type", "application/json")
        .send_string(body)
        .map_err(|error| match error {
            ureq::Error::Status(_status, response) => response.into_string().unwrap_or_default(),
            other => other.to_string(),
        })?;
    let text = response.into_string().map_err(|error| error.to_string())?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| "unavailable".to_string())?;
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(value
            .get("result")
            .map(|result| result.to_string())
            .unwrap_or_else(|| "null".to_string()))
    } else {
        // Map the JSONL error codes onto the UI's issue labels (AppHost parity).
        let code = value.get("code").and_then(Value::as_str).unwrap_or_default();
        let mapped = match code {
            "INVALID_SETTINGS" | "INVALID_CATALOG" => "settings",
            "AUDIO" => "audio",
            "UNAVAILABLE" => "unavailable",
            "UNKNOWN_OPERATION" => "unsupported",
            "" => "",
            _ => code,
        };
        let code = if mapped.is_empty() {
            value.get("error").and_then(Value::as_str).unwrap_or("unavailable")
        } else {
            mapped
        };
        Err(code.to_string())
    }
}

pub fn build(
    window: &Window,
    url: &str,
    hotkeys: SharedHotkeys,
    state: Arc<Mutex<crate::ShellState>>,
    proxy: tao::event_loop::EventLoopProxy<crate::UserEvent>,
    log: Arc<WorkerLog>,
    reply_slot: Rc<RefCell<Option<WebView>>>,
    pocket: crate::pocket::PocketHandle,
) -> Result<WebView, wry::Error> {
    let navigation_log = log.clone();
    let ipc_hotkeys = hotkeys.clone();
    let ipc_state = state.clone();
    let ipc_proxy = proxy.clone();
    let ipc_log = log.clone();
    let ipc_slot = reply_slot.clone();
    let ipc_pocket = pocket;
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
            match bridge {
                // typingSounds replies asynchronously via UserEvent::BridgeReply
                // after the /api/typing round trip completes; keyboardPocket ops
                // run on the pocket worker thread and answer the same way with
                // the full PocketStatus payload.
                "typingSounds" => handle_typing_sounds(&ipc_state, &ipc_proxy, &ipc_log, id, &payload),
                "keyboardPocket" => {
                    let op = payload.get("op").and_then(Value::as_str).unwrap_or("").to_string();
                    ipc_pocket.send(crate::pocket::Op::Ipc { id, op, values: payload });
                }
                _ => {
                    let ok = match bridge {
                        "audioControl" => handle_audio_control(&payload),
                        "codexKeys" => handle_codex_keys(&ipc_hotkeys, &payload),
                        _ => false,
                    };
                    reply(&ipc_slot, id, ok);
                }
            }
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
