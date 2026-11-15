// Smart Selection controller (Windows port of SelectionCoordinator, with the
// Mac AIKit logic moved server-side): owns the shell config (shortcut +
// panel position), the cached server preferences, the bridge ops answered on
// a dedicated worker thread, and the capture → request → streaming flow. All
// bridge ops answer with the full Status object (or a promise rejection
// carrying the issue string), like the Mac bridge.
//
// Windows degradations versus the Mac feature (all surfaced through Status):
//   • speech recognition is not implemented — microphone/speechRecognition
//     report false with "denied" states, so an enabled voice preference keeps
//     the feature out of "ready" until the user disables it;
//   • no local text recognition — a selection carries the region image only
//     when the user confirmed image input for the model;
//   • mouse side buttons are not recorded (keyboard single keys / combos only).

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tao::event_loop::EventLoopProxy;

use crate::pocket::hotkeys::RecordedOutcome;
use crate::pocket::keys::Recorder;
use crate::win;
use crate::worker::WorkerLog;
use crate::UserEvent;

use self::hook::{SelectionRuntime, SharedSelection};
use self::keys::{action_label, enabled_actions, is_valid_action, is_valid_language, Phase, Session, ShellConfig, Shortcut};
pub mod hook;
pub mod keys;
pub mod overlay;
pub mod panel;

pub type SharedCore = Arc<Mutex<SelectionCore>>;

const RECORD_TIMEOUT: Duration = Duration::from_secs(15);
const STREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct RecordingSession {
    pub session: String,
    pub reply_id: i64,
}

pub struct SelectionCore {
    pub data_dir: PathBuf,
    pub config: ShellConfig,
    /// Cached server preferences (the server is authoritative; the shell only
    /// overlays its own shortcut on top when building Status).
    pub preferences: Value,
    pub has_key: bool,
    pub server_issue: String,
    pub server: Option<(String, String)>,
    pub recording: Option<RecordingSession>,
    pub session: Option<Session>,
    pub generation: u64,
    /// Base64 of the last captured region (kept for action switching).
    pub last_png_base64: Option<String>,
    pub status: String,
    pub issue: String,
}

impl SelectionCore {
    fn open(data_dir: &Path) -> SelectionCore {
        let root = data_dir.join("selection");
        let _ = std::fs::create_dir_all(&root);
        let mut config = ShellConfig::default();
        if let Ok(text) = std::fs::read_to_string(root.join("config.json")) {
            if let Ok(saved) = serde_json::from_str::<ShellConfig>(&text) {
                config = saved;
            }
        }
        SelectionCore {
            data_dir: data_dir.to_path_buf(),
            config,
            preferences: default_preferences(),
            has_key: false,
            server_issue: String::new(),
            server: None,
            recording: None,
            session: None,
            generation: 0,
            last_png_base64: None,
            status: String::new(),
            issue: String::new(),
        }
    }

    fn save_config(&self) -> Result<(), String> {
        let root = self.data_dir.join("selection");
        let _ = std::fs::create_dir_all(&root);
        let body = serde_json::to_vec_pretty(&self.config).map_err(|e| e.to_string())?;
        let target = root.join("config.json");
        let temp = root.join(".config.tmp");
        std::fs::write(&temp, &body).map_err(|e| format!("保存失败，原有内容未改变：{e}"))?;
        if target.exists() {
            let _ = std::fs::remove_file(&target);
        }
        std::fs::rename(&temp, &target).map_err(|e| format!("保存失败，原有内容未改变：{e}"))
    }

    fn preference_bool(&self, key: &str) -> bool {
        self.preferences.get(key).and_then(Value::as_bool).unwrap_or(false)
    }

    fn preference_str(&self, key: &str) -> String {
        self.preferences.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
    }

    fn enabled(&self) -> bool {
        self.preference_bool("enabled")
    }

    fn enabled_actions(&self) -> Vec<String> {
        let saved = self
            .preferences
            .get("enabledActions")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<String>>());
        enabled_actions(saved.as_ref(), &self.preference_str("defaultAction"))
    }

    fn default_action(&self) -> String {
        let action = self.preference_str("defaultAction");
        if is_valid_action(&action) {
            action
        } else {
            "translate".into()
        }
    }

    fn language(&self) -> String {
        let language = self.preference_str("translationLanguage");
        if is_valid_language(&language) {
            language
        } else {
            "zh-CN".into()
        }
    }

    fn model(&self) -> String {
        self.preferences
            .get("configuration")
            .and_then(|c| c.get("model"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    fn image_input_confirmed(&self) -> bool {
        self.preferences
            .get("configuration")
            .and_then(|c| c.get("imageInputConfirmed"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    fn configured(&self) -> bool {
        self.has_key && !self.model().is_empty() && self.server_issue.is_empty()
    }

    /// Re-pushes the current config into the hook runtime and resets transient
    /// recording state (pocket reconfigure parity).
    fn reconfigure(&self, shared: &SharedSelection) {
        let mut runtime = shared.lock().unwrap();
        runtime.enabled = self.enabled() && self.configured();
        runtime.shortcut_vks = self.config.shortcut.vks();
        runtime.recording = None;
        runtime.recording_session = None;
        runtime.recording_started = false;
        if !runtime.enabled {
            runtime.claimed.clear();
        }
    }
}

fn default_preferences() -> Value {
    json!({
        "enabled": false,
        "voiceEnabled": false,
        "defaultAction": "translate",
        "enabledActions": ["translate", "summarize", "explain", "reply"],
        "translationLanguage": "zh-CN",
        "shortcut": { "keyCode": 12, "modifiers": 2560, "label": "⌥ ⇧ Q" },
        "configuration": {
            "provider": "openAI",
            "endpoint": "https://api.openai.com/v1/responses",
            "model": "",
            "apiProtocol": "responses",
            "imageInputConfirmed": false,
            "maximumOutputTokens": 4096,
        },
    })
}

/// Messages the selection worker understands.
pub enum Op {
    // Bridge ops (selectionLab page).
    Ipc { id: i64, op: String, values: Value },
    // The node server announced its URL/token (UserEvent::Ready).
    ServerReady { base: String, token: String },
    // Hook decisions.
    ActivateSelection,
    ShortcutReleased,
    SelectionCancelled,
    Recorded { session: String, outcome: RecordedOutcome },
    RecordTimeout { session: String },
    // Overlay gestures.
    OverlayCaptured { rect: (i32, i32, i32, i32) },
    // Panel interactions.
    PanelCopy,
    PanelSwitch,
    PanelFollowUp { text: String },
    PanelPosition { x: i32, y: i32 },
    // Stream thread feedback (mirrored to the panel via UserEvent).
    StreamDelta { generation: u64, text: String },
    StreamDone { generation: u64, ok: bool, text: String, user_turn: String },
}

#[derive(Clone)]
pub struct SelectionHandle {
    sender: Sender<Op>,
}

impl SelectionHandle {
    pub fn send(&self, op: Op) {
        let _ = self.sender.send(op);
    }

    pub fn sender(&self) -> Sender<Op> {
        self.sender.clone()
    }
}

/// Spawns the selection worker.
pub fn spawn(data_dir: &Path, log: Arc<WorkerLog>, proxy: EventLoopProxy<UserEvent>, shared: SharedSelection) -> SelectionHandle {
    let core: SharedCore = Arc::new(Mutex::new(SelectionCore::open(data_dir)));
    {
        // Initial hook configuration from the persisted shell config.
        let mut runtime = shared.lock().unwrap();
        runtime.shortcut_vks = core.lock().unwrap().config.shortcut.vks();
    }
    let (sender, receiver) = channel::<Op>();
    let handle = SelectionHandle { sender };
    let worker_core = core.clone();
    let worker_handle = handle.clone();
    std::thread::Builder::new()
        .name("selection-worker".into())
        .spawn(move || worker_loop(worker_core, worker_handle, shared, receiver, log, proxy))
        .expect("selection worker thread");
    handle
}

fn worker_loop(
    core: SharedCore,
    handle: SelectionHandle,
    shared: SharedSelection,
    receiver: Receiver<Op>,
    log: Arc<WorkerLog>,
    proxy: EventLoopProxy<UserEvent>,
) {
    while let Ok(op) = receiver.recv() {
        match op {
            Op::Ipc { id, op, values } => {
                handle_ipc(&core, &handle, &shared, id, &op, &values, &log, &proxy);
            }
            Op::ServerReady { base, token } => {
                core.lock().unwrap().server = Some((base, token));
                refresh_server_state(&core);
            }
            Op::ActivateSelection => {
                let mut guard = core.lock().unwrap();
                if !guard.enabled() || !guard.configured() || guard.recording.is_some() {
                    continue;
                }
                shared.lock().unwrap().selecting = true;
                guard.session = Some(Session::begin(&guard.default_action()));
                guard.last_png_base64 = None;
                let _ = proxy.send_event(UserEvent::SelectionOverlay { show: true });
            }
            Op::ShortcutReleased => {
                let _ = proxy.send_event(UserEvent::SelectionShortcutReleased);
            }
            Op::SelectionCancelled => {
                shared.lock().unwrap().selecting = false;
                let mut guard = core.lock().unwrap();
                if let Some(session) = guard.session.as_mut() {
                    session.phase = Phase::Cancelled;
                }
                let _ = proxy.send_event(UserEvent::SelectionOverlay { show: false });
            }
            Op::Recorded { session, outcome } => {
                let mut guard = core.lock().unwrap();
                finish_recording(&mut guard, &shared, session, outcome, &proxy);
            }
            Op::RecordTimeout { session } => {
                let mut guard = core.lock().unwrap();
                let active = guard.recording.as_ref().is_some_and(|r| r.session == session);
                if active {
                    {
                        let mut runtime = shared.lock().unwrap();
                        runtime.recording = None;
                        runtime.recording_session = None;
                        runtime.recording_started = false;
                    }
                    let reply_id = guard.recording.as_ref().map(|r| r.reply_id).unwrap_or(0);
                    guard.recording = None;
                    reply(&proxy, reply_id, false, None, Some("录制超时，请点击快捷键后重试。".into()));
                }
            }
            Op::OverlayCaptured { rect } => {
                let mut guard = core.lock().unwrap();
                shared.lock().unwrap().selecting = false;
                let _ = proxy.send_event(UserEvent::SelectionOverlay { show: false });
                start_request(&handle, &mut guard, Some(rect), &proxy, &log);
            }
            Op::PanelCopy => {
                let mut guard = core.lock().unwrap();
                let result = guard.session.as_ref().map(|session| session.result.clone()).unwrap_or_default();
                if result.is_empty() {
                    guard.issue = "还没有可复制的回答。".into();
                } else {
                    let payload = crate::pocket::clipboard::WritePayload::Text { text: result };
                    match crate::pocket::clipboard::write_clipboard(&payload, false) {
                        Ok(()) => guard.status = "已复制结果".into(),
                        Err(message) => guard.issue = message,
                    }
                }
            }
            Op::PanelSwitch => {
                let mut guard = core.lock().unwrap();
                let enabled_actions = guard.enabled_actions();
                if let Some(session) = guard.session.as_mut() {
                    if session.switch_action(&enabled_actions).is_none() {
                        continue;
                    }
                }
                start_request(&handle, &mut guard, None, &proxy, &log);
            }
            Op::PanelFollowUp { text } => {
                let mut guard = core.lock().unwrap();
                {
                    let Some(session) = guard.session.as_mut() else { continue };
                    if session.begin_follow_up().is_err() {
                        continue;
                    }
                }
                guard.generation += 1;
                let generation = guard.generation;
                let history: Vec<Value> = guard
                    .session
                    .as_ref()
                    .map(|session| session.history.iter().map(|(role, turn)| json!({ "role": role, "text": turn })).collect())
                    .unwrap_or_default();
                let payload = json!({
                    "op": "stream",
                    "action": "ask",
                    "question": text,
                    "text": "",
                    "language": guard.language(),
                    "history": history,
                });
                begin_panel_request(&mut guard, &proxy, generation, (-1, -1));
                spawn_stream(&guard, &handle, generation, payload, &proxy);
            }
            Op::PanelPosition { x, y } => {
                let mut guard = core.lock().unwrap();
                guard.config.panel_x = x;
                guard.config.panel_y = y;
                let _ = guard.save_config();
            }
            Op::StreamDelta { generation, text } => {
                let mut guard = core.lock().unwrap();
                if generation == guard.generation {
                    if let Some(session) = guard.session.as_mut() {
                        session.append_delta(&text);
                    }
                }
            }
            Op::StreamDone { generation, ok, text, user_turn } => {
                let mut guard = core.lock().unwrap();
                if generation == guard.generation {
                    if let Some(session) = guard.session.as_mut() {
                        if ok {
                            session.complete(&text, &user_turn);
                            guard.status = "回答完成".into();
                        } else {
                            session.fail(&text);
                            guard.status = text.clone();
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge op dispatch

fn handle_ipc(
    core: &SharedCore,
    _handle: &SelectionHandle,
    shared: &SharedSelection,
    id: i64,
    op: &str,
    values: &Value,
    log: &Arc<WorkerLog>,
    proxy: &EventLoopProxy<UserEvent>,
) {
    let mut guard = core.lock().unwrap();
    match op {
        "status" | "refresh" | "ready" => {
            reply(proxy, id, true, Some(snapshot(&guard, shared).to_string()), None);
        }
        "permission" => {
            // Windows needs no accessibility grant; report ready state only.
            reply(proxy, id, true, Some(snapshot(&guard, shared).to_string()), None);
        }
        "relaunch" => {
            // Windows needs no relaunch after permission changes.
            reply(proxy, id, true, Some(snapshot(&guard, shared).to_string()), None);
        }
        "recordShortcut" => begin_recording(&mut guard, shared, id, proxy),
        "capture" => {
            if !guard.enabled() {
                guard.issue = "请先开启智能框选。".into();
            } else if !guard.configured() {
                guard.issue = "请先完成模型设置。".into();
            } else {
                shared.lock().unwrap().selecting = true;
                guard.session = Some(Session::begin(&guard.default_action()));
                guard.last_png_base64 = None;
                let _ = proxy.send_event(UserEvent::SelectionOverlay { show: true });
            }
            reply(proxy, id, true, Some(snapshot(&guard, shared).to_string()), None);
        }
        "save" => apply_save(core, &mut guard, shared, values, proxy, id, log),
        "disableVoice" => {
            // Voice is not implemented on Windows; turning it off unblocks
            // readiness (the UI offers the same action).
            let mut preferences = values.get("preferences").cloned().unwrap_or_else(|| guard.preferences.clone());
            if let Some(object) = preferences.as_object_mut() {
                object.insert("voiceEnabled".into(), Value::Bool(false));
            }
            apply_save(core, &mut guard, shared, &json!({ "preferences": preferences }), proxy, id, log);
        }
        "models" | "test" => {
            let Some((base, token)) = guard.server.clone() else {
                reply(proxy, id, false, None, Some("服务尚未就绪，请稍后重试。".into()));
                return;
            };
            let timeout = match op {
                "test" => Duration::from_secs(90),
                _ => Duration::from_secs(30),
            };
            let proxy = proxy.clone();
            let body = json!({ "op": op });
            std::thread::spawn(move || {
                match server_call(&base, &token, &body, timeout) {
                    Ok(result) => {
                        let _ = proxy.send_event(UserEvent::BridgeReply { id, ok: true, payload: Some(result.to_string()), error: None });
                    }
                    Err(message) => {
                        let _ = proxy.send_event(UserEvent::BridgeReply { id, ok: false, payload: None, error: Some(message) });
                    }
                }
            });
        }
        "deleteKey" => {
            let Some((base, token)) = guard.server.clone() else {
                reply(proxy, id, false, None, Some("服务尚未就绪，请稍后重试。".into()));
                return;
            };
            let core = core.clone();
            let shared = shared.clone();
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                match server_call(&base, &token, &json!({ "op": "deleteKey" }), Duration::from_secs(30)) {
                    Ok(result) => {
                        let payload = {
                            let mut guard = core.lock().unwrap();
                            if let Some(preferences) = result.get("preferences") {
                                guard.preferences = preferences.clone();
                            }
                            guard.has_key = result.get("hasKey").and_then(Value::as_bool).unwrap_or(false);
                            guard.server_issue = result.get("issue").and_then(Value::as_str).unwrap_or_default().to_owned();
                            snapshot(&guard, &shared).to_string()
                        };
                        let _ = proxy.send_event(UserEvent::BridgeReply { id, ok: true, payload: Some(payload), error: None });
                    }
                    Err(message) => {
                        let _ = proxy.send_event(UserEvent::BridgeReply { id, ok: false, payload: None, error: Some(message) });
                    }
                }
            });
        }
        _ => {
            reply(proxy, id, false, None, Some("未知的智能框选操作。".into()));
        }
    }
}

// ---------------------------------------------------------------------------
// Save (preferences patch → local shortcut config + server store)

fn apply_save(
    core: &SharedCore,
    guard: &mut SelectionCore,
    shared: &SharedSelection,
    values: &Value,
    proxy: &EventLoopProxy<UserEvent>,
    id: i64,
    _log: &Arc<WorkerLog>,
) {
    // The shell owns the shortcut: apply it locally before the server round
    // trip so the hook reflects the new chord immediately.
    if let Some(shortcut_value) = values.get("preferences").and_then(|p| p.get("shortcut")) {
        let key_code = shortcut_value.get("keyCode").and_then(Value::as_u64);
        let modifiers = shortcut_value.get("modifiers").and_then(Value::as_u64);
        let label = shortcut_value.get("label").and_then(Value::as_str).map(str::to_owned);
        if let (Some(key_code), Some(modifiers), Some(label)) = (key_code, modifiers, label) {
            let shortcut = Shortcut {
                key_code: key_code as u8,
                modifiers: modifiers as u32,
                label,
                modifier_only: shortcut_value.get("modifierOnly").and_then(Value::as_bool).unwrap_or(false),
            };
            if !shortcut.vks().is_empty() {
                guard.config.shortcut = shortcut;
                let _ = guard.save_config();
            }
        }
    }
    guard.reconfigure(shared);
    let Some((base, token)) = guard.server.clone() else {
        reply(proxy, id, false, None, Some("服务尚未就绪，请稍后重试。".into()));
        return;
    };
    let body = json!({
        "op": "save",
        "preferences": values.get("preferences").cloned().unwrap_or(Value::Null),
        "apiKey": values.get("apiKey").cloned().unwrap_or(Value::Null),
    });
    let core = core.clone();
    let shared = shared.clone();
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        match server_call(&base, &token, &body, Duration::from_secs(90)) {
            Ok(result) => {
                let payload = {
                    let mut guard = core.lock().unwrap();
                    if let Some(preferences) = result.get("preferences") {
                        guard.preferences = preferences.clone();
                    }
                    guard.has_key = result.get("hasKey").and_then(Value::as_bool).unwrap_or(false);
                    guard.server_issue = result.get("issue").and_then(Value::as_str).unwrap_or_default().to_owned();
                    guard.reconfigure(&shared);
                    snapshot(&guard, &shared).to_string()
                };
                let _ = proxy.send_event(UserEvent::BridgeReply { id, ok: true, payload: Some(payload), error: None });
            }
            Err(message) => {
                let _ = proxy.send_event(UserEvent::BridgeReply { id, ok: false, payload: None, error: Some(message) });
            }
        }
    });
}

/// Pulls server-side state (preferences + key presence) once the server is up.
fn refresh_server_state(core: &SharedCore) {
    let Some((base, token)) = core.lock().unwrap().server.clone() else { return };
    let core = core.clone();
    std::thread::spawn(move || {
        if let Ok(result) = server_call(&base, &token, &json!({ "op": "status" }), Duration::from_secs(30)) {
            let mut guard = core.lock().unwrap();
            if let Some(preferences) = result.get("preferences") {
                guard.preferences = preferences.clone();
            }
            guard.has_key = result.get("hasKey").and_then(Value::as_bool).unwrap_or(false);
            guard.server_issue = result.get("issue").and_then(Value::as_str).unwrap_or_default().to_owned();
        }
    });
}

// ---------------------------------------------------------------------------
// Shortcut recording (pocket recording flow parity)

fn begin_recording(guard: &mut SelectionCore, shared: &SharedSelection, id: i64, proxy: &EventLoopProxy<UserEvent>) {
    let session = crate::pocket::store::uuid_v4();
    {
        let mut runtime = shared.lock().unwrap();
        runtime.recording = Some(Recorder::new(crate::pocket::keys::RecordKind::Prefix));
        runtime.recording_session = Some(session.clone());
        runtime.recording_started = false;
    }
    guard.recording = Some(RecordingSession { session: session.clone(), reply_id: id });
    let proxy_watcher = proxy.clone();
    std::thread::spawn(move || {
        std::thread::sleep(RECORD_TIMEOUT);
        let _ = proxy_watcher.send_event(UserEvent::SelectionRecordTimeout { session });
    });
}

fn finish_recording(
    guard: &mut SelectionCore,
    shared: &SharedSelection,
    session: String,
    outcome: RecordedOutcome,
    proxy: &EventLoopProxy<UserEvent>,
) {
    let Some(recording) = guard.recording.take() else { return };
    if recording.session != session {
        // Stale completion from a superseded session: keep the live one.
        guard.recording = Some(recording);
        return;
    }
    {
        let mut runtime = shared.lock().unwrap();
        runtime.recording = None;
        runtime.recording_session = None;
        runtime.recording_started = false;
    }
    let failure = match outcome {
        RecordedOutcome::Cancelled => Some("已取消快捷键录制。".to_string()),
        RecordedOutcome::Failed(message) => Some(message),
        RecordedOutcome::Chord { modifiers, ordinary } => {
            match Shortcut::from_chord(&crate::pocket::keys::RecordChord { modifiers, ordinary }) {
                Some(shortcut) => {
                    let previous = guard.config.shortcut.clone();
                    guard.config.shortcut = shortcut;
                    match guard.save_config() {
                        Ok(()) => {
                            guard.reconfigure(shared);
                            guard.status = "快捷键已保存".into();
                            None
                        }
                        Err(message) => {
                            guard.config.shortcut = previous;
                            Some(message)
                        }
                    }
                }
                None => Some("这组按键无法用作快捷键，请录制其他单键或组合键。".to_string()),
            }
        }
    };
    match failure {
        None => reply(proxy, recording.reply_id, true, Some(snapshot(guard, shared).to_string()), None),
        Some(message) => reply(proxy, recording.reply_id, false, None, Some(message)),
    }
}

// ---------------------------------------------------------------------------
// Capture + request flow

/// Positions the answer panel next to the captured region (clamped later).
fn panel_position_for(rect: (i32, i32, i32, i32)) -> (i32, i32) {
    let (vx, vy, vw, vh) = win::virtual_screen();
    if vw > 0 && vh > 0 {
        let x = (rect.2 + 16).min(vx + vw - panel::PANEL_WIDTH);
        let y = rect.1.min(vy + vh - panel::PANEL_HEIGHT);
        (x, y)
    } else {
        (rect.2 + 16, rect.1)
    }
}

/// Panel reset at the start of every request generation. x/y of -1 keeps the
/// current window position (action switches and follow-ups).
fn begin_panel_request(guard: &mut SelectionCore, proxy: &EventLoopProxy<UserEvent>, generation: u64, position: (i32, i32)) {
    let action = guard.session.as_ref().map(|session| session.action.clone()).unwrap_or_default();
    let _ = proxy.send_event(UserEvent::SelectionPanel {
        show: true,
        x: position.0,
        y: position.1,
        generation,
        action: action_label(&action).to_string(),
    });
}

/// Common tail of capture and action switching: guards the image path and
/// spawns the streaming request.
fn start_request(
    handle: &SelectionHandle,
    guard: &mut SelectionCore,
    rect: Option<(i32, i32, i32, i32)>,
    proxy: &EventLoopProxy<UserEvent>,
    _log: &Arc<WorkerLog>,
) {
    if guard.session.is_none() {
        return;
    }
    {
        let Some(session) = guard.session.as_mut() else { return };
        session.start_selecting();
        session.start_requesting();
    }
    guard.generation += 1;
    let generation = guard.generation;
    let language = guard.language();
    let position = match rect {
        Some(rect) => panel_position_for(rect),
        None => (-1, -1),
    };
    begin_panel_request(&mut *guard, proxy, generation, position);

    let image_base64 = match rect {
        Some(rect) => match crate::capture::capture_region_png(rect) {
            Ok(png) => {
                let _ = proxy.send_event(UserEvent::SelectionThumbnail { png: png.clone() });
                let encoded = base64_encode(&png);
                guard.last_png_base64 = Some(encoded.clone());
                Some(encoded)
            }
            Err(message) => {
                let message = format!("屏幕捕获失败：{message}");
                if let Some(session) = guard.session.as_mut() {
                    session.fail(&message);
                }
                let _ = proxy.send_event(UserEvent::SelectionDone { generation, ok: false, text: message });
                return;
            }
        },
        None => guard.last_png_base64.clone(),
    };

    // Degradation: without local OCR a text-only request has no content.
    let Some(image_base64) = image_base64.filter(|_| guard.image_input_confirmed()) else {
        let message = "本机暂无文字识别：请在“模型与 API”开启“发送选区图片”，或选择支持图片的模型。".to_string();
        if let Some(session) = guard.session.as_mut() {
            session.fail(&message);
        }
        let _ = proxy.send_event(UserEvent::SelectionDone { generation, ok: false, text: message });
        return;
    };

    let payload = json!({
        "op": "stream",
        "action": guard.session.as_ref().map(|session| session.action.clone()).unwrap_or_default(),
        "text": "",
        "image": { "base64": image_base64 },
        "language": language,
        "history": [],
    });
    spawn_stream(guard, handle, generation, payload, proxy);
}

/// One streaming request: POST /api/selection and forward the normalized SSE
/// events to the panel (UserEvent) and the worker (Op) as they arrive.
fn spawn_stream(
    guard: &SelectionCore,
    handle: &SelectionHandle,
    generation: u64,
    payload: Value,
    proxy: &EventLoopProxy<UserEvent>,
) {
    let fail = move |proxy: &EventLoopProxy<UserEvent>, handle: &SelectionHandle, message: String| {
        let _ = proxy.send_event(UserEvent::SelectionDone { generation, ok: false, text: message.clone() });
        let _ = handle.send(Op::StreamDone { generation, ok: false, text: message, user_turn: String::new() });
    };
    let Some((base, token)) = guard.server.clone() else {
        fail(proxy, handle, "服务尚未就绪，请稍后重试。".to_string());
        return;
    };
    let proxy = proxy.clone();
    let handle = handle.clone();
    std::thread::Builder::new()
        .name("selection-stream".into())
        .spawn(move || {
            let agent = ureq::AgentBuilder::new().timeout_connect(STREAM_CONNECT_TIMEOUT).build();
            let outcome = agent
                .post(&format!("{base}/api/selection"))
                .set("X-Air-Token", &token)
                .set("Content-Type", "application/json")
                .send_string(&payload.to_string());
            let response = match outcome {
                Ok(response) => response,
                Err(ureq::Error::Status(status, response)) => {
                    let text = response.into_string().unwrap_or_default();
                    fail(&proxy, &handle, format!("请求失败（HTTP {status}）：{}", first_error_message(&text)));
                    return;
                }
                Err(error) => {
                    fail(&proxy, &handle, format!("无法连接本机服务：{error}"));
                    return;
                }
            };
            let reader = std::io::BufReader::new(response.into_reader());
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Some(data) = line.strip_prefix("data: ") else { continue };
                let Ok(event) = serde_json::from_str::<Value>(data) else { continue };
                match event.get("type").and_then(Value::as_str) {
                    Some("delta") => {
                        let text = event.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
                        if !text.is_empty() {
                            let _ = proxy.send_event(UserEvent::SelectionDelta { generation, text: text.clone() });
                            let _ = handle.send(Op::StreamDelta { generation, text });
                        }
                    }
                    Some("done") => {
                        let text = event.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
                        let user_turn = event.get("turn").and_then(Value::as_str).unwrap_or_default().to_string();
                        let _ = proxy.send_event(UserEvent::SelectionDone { generation, ok: true, text: text.clone() });
                        let _ = handle.send(Op::StreamDone { generation, ok: true, text, user_turn });
                        return;
                    }
                    Some("error") => {
                        let text = event.get("message").and_then(Value::as_str).unwrap_or("模型服务返回错误").to_string();
                        fail(&proxy, &handle, text);
                        return;
                    }
                    _ => continue,
                }
            }
            fail(&proxy, &handle, "回答流在完成前中断了。".to_string());
        })
        .ok();
}

fn first_error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.get("error").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(120).collect())
}

// ---------------------------------------------------------------------------
// One blocking /api/selection round trip.

fn server_call(base: &str, token: &str, body: &Value, timeout: Duration) -> Result<Value, String> {
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let response = agent
        .post(&format!("{base}/api/selection"))
        .set("X-Air-Token", token)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(|error| match error {
            ureq::Error::Status(status, response) => {
                let text = response.into_string().unwrap_or_default();
                format!("请求失败（HTTP {status}）：{}", first_error_message(&text))
            }
            other => other.to_string(),
        })?;
    let text = response.into_string().map_err(|error| error.to_string())?;
    let value: Value = serde_json::from_str(&text).map_err(|_| "服务返回了无法解析的数据".to_string())?;
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    } else {
        Err(value.get("error").and_then(Value::as_str).unwrap_or("未知错误").to_string())
    }
}

// ---------------------------------------------------------------------------
// Status snapshot (Status parity for SelectionLab.tsx)

pub fn snapshot(core: &SelectionCore, shared: &SharedSelection) -> Value {
    let runtime = shared.lock().unwrap();
    let enabled = core.enabled();
    let voice_enabled = core.preference_bool("voiceEnabled");
    let recording = core.recording.is_some();
    let ready = enabled && core.configured() && runtime.hook_active && !recording && !voice_enabled;
    let phase = if recording {
        "recording".to_string()
    } else {
        core.session.as_ref().map(|session| session.phase.label().to_string()).unwrap_or_else(|| "idle".to_string())
    };
    let mut preferences = core.preferences.clone();
    if let Some(object) = preferences.as_object_mut() {
        let mut shortcut_json = json!({
            "keyCode": core.config.shortcut.key_code,
            "modifiers": core.config.shortcut.modifiers,
            "label": core.config.shortcut.label,
        });
        if core.config.shortcut.modifier_only {
            shortcut_json["modifierOnly"] = json!(true);
        }
        object.insert("shortcut".into(), shortcut_json);
    }
    json!({
        "supported": true,
        "ready": ready,
        "setupState": if recording { "recording" } else { "" },
        "shortcutConflict": false,
        "configurationIssue": core.server_issue,
        "needsRelaunch": false,
        "microphoneState": "denied",
        "speechRecognitionState": "denied",
        "preferences": preferences,
        "hasKey": core.has_key,
        "screenRecording": true,
        "accessibility": true,
        "inputMonitoring": true,
        "microphone": false,
        "speechRecognition": false,
        "shortcutNotice": "",
        "shortcutRegistered": runtime.hook_active && enabled && !recording,
        "issue": core.issue,
        "phase": phase,
        "voice": { "available": false },
    })
}

fn reply(proxy: &EventLoopProxy<UserEvent>, id: i64, ok: bool, payload: Option<String>, error: Option<String>) {
    let _ = proxy.send_event(UserEvent::BridgeReply { id, ok, payload, error });
}

fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(triple >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[triple as usize & 63] as char } else { '=' });
    }
    out
}

// ---------------------------------------------------------------------------
// QA entry (AIR_STUDIO_SELECTION_QA=1): exercises the pure logic paths
// (rect normalization, chord → shortcut conversion, tables, state machine,
// config persistence, status snapshot) in an isolated directory and writes a
// report next to the logs. No screen, no server, no API key required.

pub fn run_selection_qa(data_dir: &Path) -> i32 {
    let mut lines: Vec<String> = Vec::new();
    let mut code = 0;
    let mut check = |label: &str, ok: bool| {
        if !ok {
            code = 1;
        }
        lines.push(format!("{label}: {}", if ok { "ok" } else { "FAILED" }));
    };

    // Rect normalization across drag directions and thresholds.
    check("region forward", overlay::normalize_region((10, 20), (110, 80)) == Some((10, 20, 110, 80)));
    check("region reversed", overlay::normalize_region((110, 80), (10, 20)) == Some((10, 20, 110, 80)));
    check("region click rejected", overlay::normalize_region((5, 5), (5, 5)).is_none());
    check("region floor enforced", overlay::normalize_region((5, 5), (7, 90)).is_none());

    // Shortcut round trip: Alt+Shift+Q default and a recorded chord.
    let default_shortcut = Shortcut::default();
    check("default shortcut Alt+Shift+Q", default_shortcut.key_code == 12 && default_shortcut.modifiers == 2560);
    let recorded = Shortcut::from_chord(&crate::pocket::keys::RecordChord { modifiers: vec!["option".into(), "shift".into()], ordinary: Some(12) });
    check("chord → shortcut label", recorded.as_ref().is_some_and(|s| s.label == "⌥ ⇧ Q" && s.vks().contains(&0x51)));

    // Action/language tables (parity with the UI tables).
    check("actions table", keys::ACTIONS.len() == 4 && is_valid_action("reply") && !is_valid_action("extractText"));
    check("languages table", keys::TRANSLATION_LANGUAGES.len() == 16 && is_valid_language("zh-CN"));
    check("enabled actions fallback", enabled_actions(Some(&vec![]), "summarize") == vec!["summarize".to_string()]);

    // Session state machine: hold → select → request → delta → complete.
    let mut session = Session::begin("translate");
    session.start_selecting();
    session.start_requesting();
    session.append_delta("你");
    session.append_delta("好");
    session.complete("你好", "{\"user_task\":\"…\"}");
    check("session completes with history", session.phase == Phase::Completed && session.history.len() == 2);
    let switched = session.switch_action(&["translate".to_string(), "summarize".to_string()]);
    check("session action switch cycles", switched.as_deref() == Some("summarize"));

    // Config persistence round trip + snapshot shape.
    let root = data_dir.join("selection-qa");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::create_dir_all(&root);
    let shared: SharedSelection = SharedSelection::new(Mutex::new(SelectionRuntime::new()));
    {
        let mut core = SelectionCore::open(&root);
        core.config.shortcut = Shortcut { key_code: 0, modifiers: 2048, label: "⌥ A".into(), modifier_only: false };
        let saved = core.save_config();
        // Preferences are server-owned; a fresh core starts from defaults and
        // only the shortcut/panel config survives a restart locally.
        let reloaded = SelectionCore::open(&root);
        check(
            "config persistence",
            saved.is_ok() && reloaded.config.shortcut.label == "⌥ A" && reloaded.preference_str("defaultAction") == "translate",
        );
        check("snapshot shape", snapshot(&core, &shared).get("supported") == Some(&json!(true)));
    }
    let report = format!("{}\n", lines.join("\n"));
    let _ = std::fs::create_dir_all(data_dir);
    let _ = std::fs::write(data_dir.join("selection-qa.txt"), &report);
    print!("{report}");
    code
}
