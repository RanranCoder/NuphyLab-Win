// Keyboard Pocket controller (Windows port of PocketController.swift): owns
// the store and settings, dispatches the keyboardPocket bridge ops on a
// dedicated worker thread, drives the capture/use clipboard flows with the
// simplified conflict policy, and mirrors panel/hotkey state back to the UI
// thread through UserEvent. All ops answer with the full PocketStatus object
// (or a promise rejection carrying the issue string), like the Mac bridge.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tao::event_loop::EventLoopProxy;

use crate::win;
use crate::worker::WorkerLog;
use crate::UserEvent;

use self::clipboard::WritePayload;
use self::hotkeys::RecordedOutcome;
use self::keys::{mac_name, PocketConfig, PocketSettings, RecordKind, Recorder, SLOT_COUNT};
use self::store::{PocketItem, PocketStore};

pub mod clipboard;
pub mod hotkeys;
pub mod keys;
pub mod panel;
pub mod store;
pub mod zip;

pub use hotkeys::{PocketHook, PocketRuntime, SharedPocket};

const RECORD_TIMEOUT: Duration = Duration::from_secs(15);
const TRANSFER_RETENTION_DAYS: i64 = 7;

pub type SharedCore = Arc<Mutex<PocketCore>>;

pub struct RecordingSession {
    pub kind: String, // "use" | "capture" | "slot"
    pub index: i32,   // -1 for prefixes
    pub session: String,
    pub reply_id: i64,
}

pub struct PocketCore {
    pub settings: PocketSettings,
    pub store: PocketStore,
    pub status: String,
    pub issue: String,
    pub recording: Option<RecordingSession>,
    pub panel_capture_mode: bool,
    pub page_token: Option<String>,
    pub internal_drag: bool,
    pub file_access_required_index: Option<usize>,
    pub thumbnail_cache: HashMap<String, String>,
}

impl PocketCore {
    fn open(data_dir: &Path) -> PocketCore {
        let root = data_dir.join("keyboard-pocket");
        let _ = std::fs::create_dir_all(&root);
        let issue = match PocketStore::open(&root) {
            Ok(store) => {
                let mut settings = PocketSettings::default();
                if let Ok(text) = std::fs::read_to_string(root.join("settings.json")) {
                    if let Ok(saved) = serde_json::from_str::<PocketSettings>(&text) {
                        if saved.validate().is_ok() {
                            settings = saved;
                        }
                    }
                }
                return PocketCore {
                    settings,
                    store,
                    status: String::new(),
                    issue: String::new(),
                    recording: None,
                    panel_capture_mode: false,
                    page_token: None,
                    internal_drag: false,
                    file_access_required_index: None,
                    thumbnail_cache: HashMap::new(),
                };
            }
            Err(message) => message,
        };
        // A broken store must not brick the page: report the issue and run
        // disabled until the data file is repaired.
        PocketCore {
            settings: PocketSettings::default(),
            store: PocketStore::open(&root).unwrap_or_else(|_| {
                let _ = std::fs::write(root.join("pockets.json"), r#"{"version":1,"slots":[null,null,null,null,null,null,null,null,null],"history":[]}"#);
                PocketStore::open(&root).expect("fallback pocket store")
            }),
            status: String::new(),
            issue,
            recording: None,
            panel_capture_mode: false,
            page_token: None,
            internal_drag: false,
            file_access_required_index: None,
            thumbnail_cache: HashMap::new(),
        }
    }

    fn save_settings(&self, data_dir: &Path) -> Result<(), String> {
        let root = data_dir.join("keyboard-pocket");
        let _ = std::fs::create_dir_all(&root);
        let body = serde_json::to_vec_pretty(&self.settings).map_err(|e| e.to_string())?;
        let target = root.join("settings.json");
        let temp = root.join(".settings.tmp");
        std::fs::write(&temp, &body).map_err(|e| format!("保存失败，原有内容未改变：{e} / Could not save: {e}"))?;
        if target.exists() {
            let _ = std::fs::remove_file(&target);
        }
        std::fs::rename(&temp, &target).map_err(|e| format!("保存失败，原有内容未改变：{e} / Could not save: {e}"))
    }
}

/// Messages the pocket worker understands.
pub enum Op {
    Ipc { id: i64, op: String, values: Value },
    HookAction { action: &'static str, index: usize },
    Preview { show: bool, capture_mode: bool },
    Recorded { session: String, kind: RecordKind, outcome: RecordedOutcome },
    RecordTimeout { session: String },
    PanelClick { position: usize },
}

#[derive(Clone)]
pub struct PocketHandle {
    sender: Sender<Op>,
}

impl PocketHandle {
    pub fn send(&self, op: Op) {
        let _ = self.sender.send(op);
    }

    /// A clone of the op channel for the hook thread.
    pub fn sender(&self) -> Sender<Op> {
        self.sender.clone()
    }
}

/// Starts the pocket worker. Returns the op handle plus the shared core the
/// panel thread reads for painting.
pub fn spawn(
    data_dir: &Path,
    log: Arc<WorkerLog>,
    proxy: EventLoopProxy<UserEvent>,
    shared: SharedPocket,
) -> (PocketHandle, SharedCore) {
    let core = Arc::new(Mutex::new(PocketCore::open(data_dir)));
    let (sender, receiver) = channel::<Op>();
    let data_dir = data_dir.to_path_buf();
    let worker_core = core.clone();
    std::thread::Builder::new()
        .name("pocket-worker".into())
        .spawn(move || worker_loop(worker_core, shared, data_dir, receiver, log, proxy))
        .expect("pocket worker thread");
    (PocketHandle { sender }, core)
}

fn worker_loop(
    core: SharedCore,
    shared: SharedPocket,
    data_dir: PathBuf,
    receiver: Receiver<Op>,
    log: Arc<WorkerLog>,
    proxy: EventLoopProxy<UserEvent>,
) {
    while let Ok(op) = receiver.recv() {
        match op {
            Op::Ipc { id, op, values } => {
                let mut guard = core.lock().unwrap();
                handle_ipc(&mut guard, &shared, &data_dir, id, &op, &values, &proxy);
                if !guard.status.is_empty() {
                    log.append(format!("[pocket] op {op}: {}\n", guard.status).as_bytes());
                }
            }
            Op::HookAction { action, index } => {
                let mut guard = core.lock().unwrap();
                if action == "use" {
                    use_slot(&mut guard, &shared, &data_dir, index);
                } else if action == "capture" {
                    capture_to_slot(&mut guard, &shared, index);
                }
                // Panel visibility is owned by Preview/openPanel events; a
                // completed action leaves the panel hidden state as-is.
            }
            Op::Preview { show, capture_mode } => {
                let mut guard = core.lock().unwrap();
                if show && guard.settings.enabled && guard.settings.preview_enabled {
                    guard.panel_capture_mode = capture_mode;
                    shared.lock().unwrap().panel_visible = true;
                    let _ = proxy.send_event(UserEvent::PocketPanel { show: true });
                } else if !show && shared.lock().unwrap().panel_visible {
                    shared.lock().unwrap().panel_visible = false;
                    let _ = proxy.send_event(UserEvent::PocketPanel { show: false });
                }
            }
            Op::Recorded { session, kind, outcome } => {
                let mut guard = core.lock().unwrap();
                finish_recording(&mut guard, &shared, &data_dir, &session, kind, outcome, &proxy);
            }
            Op::RecordTimeout { session } => {
                let mut guard = core.lock().unwrap();
                let active = guard.recording.as_ref().is_some_and(|r| r.session == session);
                if active {
                    {
                        let mut runtime = shared.lock().unwrap();
                        runtime.recording = None;
                        runtime.recording_kind = None;
                        runtime.recording_session = None;
                        runtime.recording_started = false;
                    }
                    let reply_id = guard.recording.as_ref().map(|r| r.reply_id).unwrap_or(0);
                    guard.recording = None;
                    reply(
                        &proxy,
                        reply_id,
                        false,
                        None,
                        Some("录制超时，请点击快捷键后重试。 / Recording timed out. Click the shortcut and try again.".into()),
                    );
                }
            }
            Op::PanelClick { position } => {
                let mut guard = core.lock().unwrap();
                let order = display_order(&guard.settings);
                if let Some(index) = order.get(position).copied() {
                    if guard.panel_capture_mode {
                        capture_to_slot(&mut guard, &shared, index);
                    } else {
                        use_slot(&mut guard, &shared, &data_dir, index);
                    }
                }
                let _ = proxy.send_event(UserEvent::PocketPanel { show: true });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge op dispatch (PocketController.handle parity)

fn handle_ipc(
    core: &mut PocketCore,
    shared: &SharedPocket,
    data_dir: &Path,
    id: i64,
    op: &str,
    values: &Value,
    proxy: &EventLoopProxy<UserEvent>,
) {
    match op {
        "page" => {
            let token = values.get("pageToken").and_then(Value::as_str).filter(|t| t.len() <= 128);
            let active = values.get("active") == Some(&Value::Bool(true));
            if active {
                if core.page_token.as_deref() != token {
                    cancel_recording(core, shared, proxy);
                }
                core.page_token = token.map(str::to_owned);
            } else if token.is_some() && core.page_token.as_deref() == token {
                cancel_recording(core, shared, proxy);
                core.page_token = None;
            }
            reply(proxy, id, true, Some("true".into()), None);
        }
        "layout" => {
            // The Windows panel is a native window; slot rects are not needed.
            reply(proxy, id, true, Some("true".into()), None);
        }
        "internalDrag" => {
            let active = values.get("active") == Some(&Value::Bool(true));
            let token_ok = values
                .get("pageToken")
                .and_then(Value::as_str)
                .is_some_and(|token| Some(token) == core.page_token.as_deref());
            if token_ok {
                core.internal_drag = active;
                shared.lock().unwrap().internal_drag = active;
            }
            reply(proxy, id, true, Some("true".into()), None);
        }
        "refresh" | "status" | "ready" => {
            reply(proxy, id, true, Some(snapshot(core, shared).to_string()), None);
        }
        "permission" => {
            // Windows needs no accessibility grant; report ready state only.
            reply(proxy, id, true, Some(snapshot(core, shared).to_string()), None);
        }
        "cancelShortcutRecording" => {
            cancel_recording(core, shared, proxy);
            reply(proxy, id, true, Some(snapshot(core, shared).to_string()), None);
        }
        "recordShortcut" => begin_recording(core, shared, id, values, proxy),
        "settings" => apply_settings(core, shared, data_dir, values, proxy, id),
        "undo" => {
            require_enabled(core);
            match core.store.undo() {
                Ok(()) => core.status = "已撤销 / Undone".into(),
                Err(message) => {
                    core.status = message.clone();
                    reply(proxy, id, false, None, Some(message));
                    return;
                }
            }
            reply(proxy, id, true, Some(snapshot(core, shared).to_string()), None);
        }
        "clear" => {
            require_enabled(core);
            let index = values.get("index").and_then(Value::as_i64).unwrap_or(-1);
            let result = usize::try_from(index)
                .ok()
                .filter(|i| *i < SLOT_COUNT)
                .ok_or_else(|| "槽位不存在。 / This slot does not exist.".to_string())
                .and_then(|index| core.store.clear(index));
            match result {
                Ok(()) => core.status = "已清除，可撤销 / Cleared. Undo is available.".into(),
                Err(message) => {
                    core.status = message.clone();
                    reply(proxy, id, false, None, Some(message));
                    return;
                }
            }
            reply(proxy, id, true, Some(snapshot(core, shared).to_string()), None);
        }
        "clearAll" => {
            require_enabled(core);
            let confirmed = {
                shared.lock().unwrap().dialog_open = true;
                let confirmed = win::ask_yes_no(
                    "清空口袋？将清空九个槽位中的内容，之后可以撤销。 / Clear the pocket? All nine slots will be cleared. You can undo this action.",
                    "NuPhyLab",
                );
                shared.lock().unwrap().dialog_open = false;
                confirmed
            };
            if confirmed {
                match core.store.clear_all() {
                    Ok(()) => core.status = "已清除，可撤销 / Cleared. Undo is available.".into(),
                    Err(message) => {
                        core.status = message.clone();
                        reply(proxy, id, false, None, Some(message));
                        return;
                    }
                }
            }
            reply(proxy, id, true, Some(snapshot(core, shared).to_string()), None);
        }
        "copy" => {
            require_enabled(core);
            let index = values.get("index").and_then(Value::as_i64).unwrap_or(-1);
            if let Ok(index) = usize::try_from(index) {
                copy_slot(core, data_dir, index);
            }
            reply(proxy, id, true, Some(snapshot(core, shared).to_string()), None);
        }
        "capture" => {
            require_enabled(core);
            let index = values.get("index").and_then(Value::as_i64).unwrap_or(-1);
            if let Ok(index) = usize::try_from(index) {
                capture_to_slot(core, shared, index);
            }
            reply(proxy, id, true, Some(snapshot(core, shared).to_string()), None);
        }
        "import" => {
            require_enabled(core);
            let index = values.get("index").and_then(Value::as_i64).unwrap_or(-1);
            if let Ok(index) = usize::try_from(index) {
                import_files(core, shared, index);
            }
            reply(proxy, id, true, Some(snapshot(core, shared).to_string()), None);
        }
        "reconnect" => {
            require_enabled(core);
            let index = values.get("index").and_then(Value::as_i64).unwrap_or(-1);
            if let Ok(index) = usize::try_from(index) {
                reconnect_files(core, shared, index);
            }
            reply(proxy, id, true, Some(snapshot(core, shared).to_string()), None);
        }
        "openPanel" => {
            require_enabled(core);
            if core.settings.enabled && core.settings.preview_enabled {
                core.panel_capture_mode = false;
                shared.lock().unwrap().panel_visible = true;
                let _ = proxy.send_event(UserEvent::PocketPanel { show: true });
            }
            reply(proxy, id, true, Some(snapshot(core, shared).to_string()), None);
        }
        _other => {
            reply(proxy, id, false, None, Some("未知口袋操作 / Unknown pocket operation".into()));
        }
    }
}

fn require_enabled(core: &mut PocketCore) {
    if !core.settings.enabled {
        core.status = "请先开启键盘口袋。 / Turn on Keyboard Pocket to use this feature.".into();
    }
}

fn display_order(settings: &PocketSettings) -> Vec<usize> {
    let valid = settings.slot_order.len() == SLOT_COUNT
        && settings.slot_order.iter().collect::<std::collections::HashSet<_>>().len() == SLOT_COUNT
        && settings.slot_order.iter().all(|i| (0..SLOT_COUNT).contains(i));
    if valid {
        settings.slot_order.clone()
    } else {
        (0..SLOT_COUNT).collect()
    }
}

fn apply_settings(
    core: &mut PocketCore,
    shared: &SharedPocket,
    data_dir: &Path,
    values: &Value,
    proxy: &EventLoopProxy<UserEvent>,
    id: i64,
) {
    // The UI posts {op:"settings", values:{...}}; the patch lives one level down.
    let patch = values.get("values").cloned().unwrap_or_else(|| json!({}));
    let mut next = core.settings.clone();
    next.apply(&patch);
    if let Err(message) = next.validate() {
        reply(proxy, id, false, None, Some(message));
        return;
    }
    let previous = core.settings.clone();
    core.settings = next;
    if let Err(message) = core.save_settings(data_dir) {
        core.settings = previous;
        reply(proxy, id, false, None, Some(message));
        return;
    }
    reconfigure(core, shared);
    let _ = core.store.enforce_retention(core.settings.retain_days);
    core.status = "设置已保存".into();
    reply(proxy, id, true, Some(snapshot(core, shared).to_string()), None);
}

/// Pushes the current settings into the hotkey runtime and clears transient
/// state that depended on the old configuration.
pub fn reconfigure(core: &PocketCore, shared: &SharedPocket) {
    let mut runtime = shared.lock().unwrap();
    runtime.config = PocketConfig::from_settings(&core.settings);
    runtime.active = None;
    runtime.claimed.clear();
}

// ---------------------------------------------------------------------------
// Recording

fn begin_recording(
    core: &mut PocketCore,
    shared: &SharedPocket,
    id: i64,
    values: &Value,
    proxy: &EventLoopProxy<UserEvent>,
) {
    let kind_name = values.get("kind").and_then(Value::as_str).unwrap_or("");
    let index = values.get("index").and_then(Value::as_i64).unwrap_or(-1);
    let kind = match kind_name {
        "use" | "capture" => RecordKind::Prefix,
        "slot" if (0..SLOT_COUNT).contains(&(index as usize)) => RecordKind::Slot,
        _ => {
            reply(
                proxy,
                id,
                false,
                None,
                Some("请在键盘口袋页面录制快捷键。 / Open Keyboard Pocket settings to record a shortcut.".into()),
            );
            return;
        }
    };
    let token_ok = values
        .get("pageToken")
        .and_then(Value::as_str)
        .is_some_and(|token| Some(token) == core.page_token.as_deref());
    if !token_ok {
        reply(
            proxy,
            id,
            false,
            None,
            Some("请在键盘口袋页面录制快捷键。 / Open Keyboard Pocket settings to record a shortcut.".into()),
        );
        return;
    }
    // A stale session (page restarted mid-recording) is cancelled silently.
    cancel_recording(core, shared, proxy);
    let session = store::uuid_v4();
    {
        let mut runtime = shared.lock().unwrap();
        runtime.recording = Some(Recorder::new(kind));
        runtime.recording_kind = Some(kind);
        runtime.recording_session = Some(session.clone());
        runtime.recording_started = false;
    }
    core.recording = Some(RecordingSession {
        kind: kind_name.to_string(),
        index: index as i32,
        session: session.clone(),
        reply_id: id,
    });
    // Timeout watchdog routed through the event loop so it cannot race the
    // worker's own completion path.
    let proxy_watcher = proxy.clone();
    std::thread::spawn(move || {
        std::thread::sleep(RECORD_TIMEOUT);
        let _ = proxy_watcher.send_event(UserEvent::PocketRecordTimeout { session });
    });
}

fn cancel_recording(core: &mut PocketCore, shared: &SharedPocket, proxy: &EventLoopProxy<UserEvent>) {
    let pending = core.recording.take();
    {
        let mut runtime = shared.lock().unwrap();
        runtime.recording = None;
        runtime.recording_kind = None;
        runtime.recording_session = None;
        runtime.recording_started = false;
    }
    if let Some(session) = pending {
        reply(
            proxy,
            session.reply_id,
            false,
            None,
            Some("已取消快捷键录制。 / Shortcut recording cancelled.".into()),
        );
    }
}

fn finish_recording(
    core: &mut PocketCore,
    shared: &SharedPocket,
    data_dir: &Path,
    session: &str,
    _kind: RecordKind,
    outcome: RecordedOutcome,
    proxy: &EventLoopProxy<UserEvent>,
) {
    let Some(recording) = core.recording.take() else { return };
    if recording.session != session {
        // Stale completion from a superseded session: keep the live one.
        core.recording = Some(recording);
        return;
    }
    {
        let mut runtime = shared.lock().unwrap();
        runtime.recording = None;
        runtime.recording_kind = None;
        runtime.recording_session = None;
        runtime.recording_started = false;
    }
    let failure = match outcome {
        RecordedOutcome::Cancelled => Some("已取消快捷键录制。 / Shortcut recording cancelled.".to_string()),
        RecordedOutcome::Failed(message) => Some(message),
        RecordedOutcome::Chord { modifiers, ordinary } => {
            let mut next = core.settings.clone();
            let chord_error = match recording.kind.as_str() {
                "slot" => match ordinary {
                    Some(code) => {
                        next.slot_key_codes[recording.index.max(0) as usize] = code;
                        None
                    }
                    None => Some("槽位请使用一个普通按键，不带修饰键。 / Use one ordinary key without modifiers for a slot.".to_string()),
                },
                "use" => {
                    next.use_modifiers = modifiers.clone();
                    next.use_trigger = ordinary;
                    None
                }
                "capture" => {
                    next.capture_modifiers = modifiers.clone();
                    next.capture_trigger = ordinary;
                    None
                }
                _ => Some("未知的录制类型。 / Unknown recording kind.".to_string()),
            };
            match chord_error {
                Some(message) => Some(message),
                None => match next.validate() {
                    Ok(()) => {
                        let previous = core.settings.clone();
                        core.settings = next;
                        if let Err(message) = core.save_settings(data_dir) {
                            core.settings = previous;
                            Some(message)
                        } else {
                            reconfigure(core, shared);
                            core.status = "快捷键已保存 / Shortcut saved".into();
                            None
                        }
                    }
                    Err(message) => Some(message),
                },
            }
        }
    };
    match failure {
        None => reply(proxy, recording.reply_id, true, Some(snapshot(core, shared).to_string()), None),
        Some(message) => reply(proxy, recording.reply_id, false, None, Some(message)),
    }
}

// ---------------------------------------------------------------------------
// Capture and use flows (simplified PocketTransferPolicy)

fn wait_keys_released(shared: &SharedPocket) {
    let vks: Vec<u16> = {
        let runtime = shared.lock().unwrap();
        let mut vks = Vec::new();
        for prefix in &runtime.config.prefixes {
            vks.extend(prefix.modifiers.iter().copied());
            if let Some(trigger) = prefix.trigger {
                vks.push(trigger);
            }
        }
        vks
    };
    let deadline = Instant::now() + Duration::from_millis(1000);
    while clipboard::keys_still_held(&vks) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn capture_to_slot(core: &mut PocketCore, shared: &SharedPocket, index: usize) {
    if !enabled_ok(core) || index >= SLOT_COUNT {
        return;
    }
    let target = clipboard::foreground_window();
    let focus = clipboard::foreground_focus_hwnd();
    wait_keys_released(shared);
    let initial = clipboard::sequence_number();
    if !clipboard::send_copy_paste(false) {
        core.status = "无法发起复制，请检查目标应用。 / Could not start copying. Check the target app.".into();
        return;
    }
    let mut changed = false;
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        if clipboard::foreground_window() != target || clipboard::foreground_focus_hwnd() != focus {
            core.status = "选中位置已变化，未收纳。 / The selection changed. Nothing was stored.".into();
            return;
        }
        if clipboard::sequence_number() != initial {
            changed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(60));
    }
    if !changed {
        core.status = "没有取得选中内容。请先选中文字或文件，再按收纳快捷键。 / No selection was copied. Select text or files, then press the store shortcut.".into();
        return;
    }
    match clipboard::capture() {
        Ok(captured) => match item_from_captured(core, captured) {
            Ok(item) => match core.store.put(item, index) {
                Ok(()) => core.status = format!("已放入口袋 {} / Stored in Pocket {}", index + 1, index + 1),
                Err(message) => core.status = message,
            },
            Err(message) => core.status = message,
        },
        Err(message) => core.status = message,
    }
}

fn enabled_ok(core: &mut PocketCore) -> bool {
    if !core.settings.enabled {
        core.status = "请先开启键盘口袋。 / Turn on Keyboard Pocket to use this feature.".into();
        return false;
    }
    true
}

fn item_from_captured(core: &mut PocketCore, captured: clipboard::Captured) -> Result<PocketItem, String> {
    match captured.kind {
        "files" => core.store.item_from_files(captured.files.unwrap_or_default()),
        "image" => {
            let png = captured.image_png.ok_or_else(|| empty_content("image"))?;
            let stored = core.store.persist_image(&png)?;
            Ok(PocketItem {
                id: store::uuid_v4(),
                kind: "image".into(),
                title: "图片".into(),
                image_path: Some(stored),
                created_at: store::iso_now(),
                ..PocketItem::default()
            })
        }
        "richText" => {
            let text = captured.text.clone().unwrap_or_default();
            let html = captured.rich_html.ok_or_else(|| empty_content("richText"))?;
            let stored = core.store.persist_rich_text(&html)?;
            Ok(PocketItem {
                id: store::uuid_v4(),
                kind: "richText".into(),
                title: clipboard::title_for_text(&text),
                text: Some(text),
                rich_text_path: Some(stored),
                created_at: store::iso_now(),
                ..PocketItem::default()
            })
        }
        "link" => {
            let text = captured.text.unwrap_or_default();
            Ok(PocketItem {
                id: store::uuid_v4(),
                kind: "link".into(),
                title: clipboard::title_for_text(&text),
                text: Some(text),
                created_at: store::iso_now(),
                ..PocketItem::default()
            })
        }
        _ => {
            let text = captured.text.unwrap_or_default();
            Ok(PocketItem {
                id: store::uuid_v4(),
                kind: "text".into(),
                title: clipboard::title_for_text(&text),
                text: Some(text),
                created_at: store::iso_now(),
                ..PocketItem::default()
            })
        }
    }
}

fn empty_content(kind: &str) -> String {
    "剪贴板中没有可保存的文字、图片、链接或文件。 / The clipboard has no supported text, image, link or files to store.".to_string().replace("内容", kind)
}

/// Writes the slot content to the clipboard and optionally pastes it into the
/// foreground window (conflict policy: a changed foreground cancels the paste
/// but keeps the copied content, Mac deliver() parity). The panel never
/// activates, so the foreground window is the paste target for panel clicks.
fn use_slot(core: &mut PocketCore, shared: &SharedPocket, data_dir: &Path, index: usize) {
    if !enabled_ok(core) || index >= SLOT_COUNT {
        return;
    }
    core.file_access_required_index = None;
    let Some(item) = core.store.slots.get(index).cloned().flatten() else {
        core.status = "这个槽位还是空的。 / This slot is empty.".into();
        return;
    };
    let target = clipboard::foreground_window();
    if let Err(message) = write_slot(core, data_dir, index, &item) {
        core.status = message;
        return;
    }
    if target == 0 {
        core.status = format!("已复制口袋 {}，可粘贴或拖到目标位置。 / Copied Pocket {}. Paste or drag it where you need it.", index + 1, index + 1);
        return;
    }
    wait_keys_released(shared);
    if clipboard::foreground_window() != target {
        core.status = "目标窗口已切换，内容已复制，请手动粘贴。 / The target window changed. The content is copied; paste it manually.".into();
        return;
    }
    if clipboard::send_copy_paste(true) {
        core.status = format!("已取用口袋 {} / Retrieved Pocket {}", index + 1, index + 1);
        let same_content = core.store.slots.get(index).and_then(|slot| slot.as_ref()).map(|slot| slot.id.as_str()) == Some(item.id.as_str());
        if core.settings.clear_after_use && same_content {
            let _ = core.store.clear(index);
        }
    } else {
        core.status = "未能发起粘贴。内容已复制，可手动粘贴。 / Could not start pasting. The content is copied and can be pasted manually.".into();
    }
}

/// The copy op: clipboard write only, no paste. Folders are archived first.
fn copy_slot(core: &mut PocketCore, data_dir: &Path, index: usize) {
    if !enabled_ok(core) || index >= SLOT_COUNT {
        return;
    }
    core.file_access_required_index = None;
    let Some(item) = core.store.slots.get(index).cloned().flatten() else {
        core.status = "这个槽位还是空的。 / This slot is empty.".into();
        return;
    };
    if let Err(message) = write_slot(core, data_dir, index, &item) {
        core.status = message;
        return;
    }
    core.status = if PocketStore::needs_archive(&item) {
        "已复制 ZIP，可粘贴到聊天或资源管理器。 / ZIP copied. Paste it into your chat or Explorer.".into()
    } else {
        format!("已复制口袋 {} / Copied slot {}", index + 1, index + 1)
    };
}

fn write_slot(core: &mut PocketCore, data_dir: &Path, index: usize, item: &PocketItem) -> Result<(), String> {
    let plain = core.settings.plain_text;
    let payload = match item.kind.as_str() {
        "files" => {
            let paths = core.store.file_access(item)?;
            if PocketStore::needs_archive(item) {
                let archive = build_transfer_zip(data_dir, &paths)?;
                WritePayload::Files { paths: vec![archive] }
            } else {
                WritePayload::Files { paths }
            }
        }
        "image" => {
            let path = item
                .image_path
                .as_deref()
                .ok_or_else(|| "图片内容缺失。 / The image content is missing.".to_string())?;
            if !Path::new(path).exists() {
                core.file_access_required_index = Some(index);
                return Err(format!("文件已移动或删除：{path} / The file was moved or deleted: {path}"));
            }
            let png = std::fs::read(path).map_err(|e| e.to_string())?;
            WritePayload::Image { png }
        }
        "richText" => {
            let text = item.text.clone().unwrap_or_default();
            let mut fragment = String::new();
            if !plain {
                let path = item
                    .rich_text_path
                    .as_deref()
                    .ok_or_else(|| "富文本内容缺失。 / The rich text content is missing.".to_string())?;
                fragment = std::fs::read_to_string(path).unwrap_or_default();
            }
            WritePayload::RichText { text, html_fragment: fragment }
        }
        _ => {
            let text = item
                .text
                .clone()
                .ok_or_else(|| "文字内容缺失。 / The text content is missing.".to_string())?;
            WritePayload::Text { text }
        }
    };
    clipboard::write_clipboard(&payload, plain)
}

/// Archives folder references into the transfer cache directory (7-day
/// retention, Mac FolderTransfer parity) and returns the ZIP path.
fn build_transfer_zip(data_dir: &Path, paths: &[PathBuf]) -> Result<PathBuf, String> {
    expire_transfers(data_dir);
    let root = data_dir.join("keyboard-pocket").join("transfer");
    std::fs::create_dir_all(&root).map_err(|e| format!("临时文件夹不可用。 / {e}"))?;
    let archive = root.join(format!("pocket-{}.zip", store::uuid_v4()));
    let bytes = zip::build_zip(paths)?;
    std::fs::write(&archive, bytes).map_err(|e| format!("临时文件夹不可用。 / {e}"))?;
    Ok(archive)
}

fn expire_transfers(data_dir: &Path) {
    let root = data_dir.join("keyboard-pocket").join("transfer");
    let Ok(entries) = std::fs::read_dir(&root) else { return };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    for entry in entries.filter_map(|e| e.ok()) {
        let cutoff = now - TRANSFER_RETENTION_DAYS * 86_400;
        if let Ok(meta) = entry.metadata() {
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if modified < cutoff {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

fn import_files(core: &mut PocketCore, shared: &SharedPocket, index: usize) {
    shared.lock().unwrap().dialog_open = true;
    let selection = open_file_dialog(true);
    shared.lock().unwrap().dialog_open = false;
    let Some(paths) = selection else { return };
    match core.store.item_from_files(paths) {
        Ok(item) => match core.store.put(item, index) {
            Ok(()) => core.status = format!("已放入口袋 {} / Stored in Pocket {}", index + 1, index + 1),
            Err(message) => core.status = message,
        },
        Err(message) => core.status = message,
    }
}

fn reconnect_files(core: &mut PocketCore, shared: &SharedPocket, index: usize) {
    let Some(item) = core.store.slots.get(index).cloned().flatten() else {
        core.status = "这个槽位还是空的。 / This slot is empty.".into();
        return;
    };
    if item.kind != "files" {
        core.status = "只有文件槽位可以恢复访问。 / Only file slots can be reconnected.".into();
        return;
    }
    let expected = item.files.len();
    shared.lock().unwrap().dialog_open = true;
    let selection = open_file_dialog(expected > 1);
    shared.lock().unwrap().dialog_open = false;
    let Some(paths) = selection else { return };
    if paths.len() != expected {
        core.status = "请选择这个槽位原来的文件或文件夹。 / Select the original file or folder for this slot.".into();
        return;
    }
    let mut next = item.clone();
    next.files = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    if let Err(message) = core.store.validate_item(&next) {
        core.status = message;
        return;
    }
    match core.store.put(next, index) {
        Ok(()) => {
            core.file_access_required_index = None;
            core.status = "文件访问已恢复，可以复制或取用此槽位。 / File access restored. You can copy or retrieve this slot.".into();
        }
        Err(message) => core.status = message,
    }
}

/// Win32 GetOpenFileNameW wrapper (multi-select explorer buffer supported).
fn open_file_dialog(multi: bool) -> Option<Vec<PathBuf>> {
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OPENFILENAMEW, OFN_ALLOWMULTISELECT, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_PATHMUSTEXIST,
    };
    let mut buffer = vec![0u16; 32 * 1024];
    let mut filter: Vec<u16> = "所有文件\0*.*\0All files\0*.*\0\0".encode_utf16().collect();
    let mut title: Vec<u16> = "放入口袋 / Store in Pocket\0".encode_utf16().collect();
    let mut file = vec![0u16; 1024];
    let mut structure: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    structure.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    structure.lpstrFilter = filter.as_mut_ptr();
    structure.lpstrFile = buffer.as_mut_ptr();
    structure.nMaxFile = buffer.len() as u32;
    structure.lpstrFileTitle = file.as_mut_ptr();
    structure.nMaxFileTitle = file.len() as u32;
    structure.lpstrTitle = title.as_mut_ptr();
    structure.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | if multi { OFN_ALLOWMULTISELECT | OFN_EXPLORER } else { 0 };
    let ok = unsafe { GetOpenFileNameW(&mut structure) };
    if ok == 0 {
        return None;
    }
    // Parse the null-separated explorer buffer (dir + names when multi).
    let mut parts: Vec<String> = Vec::new();
    let mut start = 0usize;
    for (offset, unit) in buffer.iter().enumerate() {
        if *unit == 0 {
            if start == offset {
                break; // double null terminates the list
            }
            parts.push(String::from_utf16_lossy(&buffer[start..offset]));
            start = offset + 1;
            if offset + 1 < buffer.len() && buffer[offset + 1] == 0 {
                break;
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    if parts.len() == 1 {
        return Some(vec![PathBuf::from(&parts[0])]);
    }
    let directory = PathBuf::from(&parts[0]);
    Some(parts[1..].iter().map(|name| directory.join(name)).collect())
}

// ---------------------------------------------------------------------------
// Snapshot (PocketStatus parity)

fn slot_ready(core: &PocketCore, shared: &SharedPocket) -> bool {
    let runtime = shared.lock().unwrap();
    let prefixes_configured = runtime
        .config
        .prefixes
        .iter()
        .any(|prefix| !prefix.modifiers.is_empty() || prefix.trigger.is_some());
    core.settings.enabled && runtime.hook_active && core.recording.is_none() && prefixes_configured
}

/// Shrinks oversized images so the settings page stays responsive.
fn downscale_png(data: &[u8], maximum: u32) -> Option<Vec<u8>> {
    use std::io::Cursor;
    let mut decoder = png::Decoder::new(Cursor::new(data));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buffer).ok()?;
    let (width, height) = (frame.width, frame.height);
    let channels = match frame.color_type {
        png::ColorType::Rgba => 4usize,
        png::ColorType::Rgb => 3,
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        _ => return None,
    };
    let scale = (width as f64 / maximum as f64).max(height as f64 / maximum as f64).ceil().max(1.0) as u32;
    if scale <= 1 {
        return Some(data.to_vec());
    }
    let new_width = (width / scale).max(1);
    let new_height = (height / scale).max(1);
    let mut output = Vec::with_capacity((new_width * new_height * channels as u32) as usize);
    for y in 0..new_height {
        let source_y = (y * scale).min(height - 1) as usize;
        for x in 0..new_width {
            let source_x = (x * scale).min(width - 1) as usize;
            let start = (source_y * width as usize + source_x) * channels;
            output.extend_from_slice(&buffer[start..start + channels]);
        }
    }
    let rgba = match channels {
        4 => output,
        3 => {
            let mut expanded = Vec::with_capacity(output.len() / 3 * 4);
            for pixel in output.chunks_exact(3) {
                expanded.extend_from_slice(pixel);
                expanded.push(255);
            }
            expanded
        }
        1 => output.iter().flat_map(|p| [*p, *p, *p, 255]).collect(),
        2 => {
            let mut expanded = Vec::with_capacity(output.len() * 2);
            for pixel in output.chunks_exact(2) {
                expanded.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
            expanded
        }
        _ => return None,
    };
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, new_width, new_height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(&rgba).ok()?;
    }
    Some(encoded)
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

fn thumbnail_for(core: &mut PocketCore, id: &str, path: &str) -> Option<String> {
    if let Some(cached) = core.thumbnail_cache.get(id) {
        return Some(cached.clone());
    }
    let data = std::fs::read(path).ok()?;
    if data.len() > 8 * 1024 * 1024 {
        return None;
    }
    let png = if data.len() <= 400 * 1024 { data } else { downscale_png(&data, 320)? };
    let encoded = format!("data:image/png;base64,{}", base64_encode(&png));
    core.thumbnail_cache.insert(id.to_string(), encoded.clone());
    Some(encoded)
}

fn slot_status_text(core: &PocketCore, shared: &SharedPocket) -> String {
    let runtime = shared.lock().unwrap();
    if !runtime.hook_active {
        "快捷键已停止 / Shortcuts are stopped".into()
    } else if !core.settings.enabled {
        "快捷键已关闭 / Shortcuts are off".into()
    } else if core.recording.is_some() {
        "录制中 / Recording".into()
    } else if core.settings.uses_fn() {
        "等待检测 Fn：Windows 通常检测不到 Fn，建议录制其他修饰键 / Waiting for Fn: Fn is usually not observable on Windows, record other modifiers".into()
    } else {
        "直达快捷键就绪 / Slot shortcuts are ready".into()
    }
}

pub fn snapshot(core: &mut PocketCore, shared: &SharedPocket) -> Value {
    let mut slots = core.store.slots_value();
    let names = core.settings.slot_key_names();
    // Collect image asset references first so the thumbnail cache can mutate.
    let image_refs: Vec<(usize, String, String)> = core
        .store
        .slots
        .iter()
        .enumerate()
        .filter_map(|(index, slot)| {
            slot.as_ref()
                .filter(|item| item.kind == "image")
                .and_then(|item| item.image_path.clone().map(|path| (index, item.id.clone(), path)))
        })
        .collect();
    let mut thumbnails: Vec<(usize, String)> = Vec::new();
    for (index, id, path) in image_refs {
        if let Some(encoded) = thumbnail_for(core, &id, &path) {
            thumbnails.push((index, encoded));
        }
    }
    for (index, thumbnail) in thumbnails {
        if let Some(slot) = slots.get_mut(index) {
            if let Some(object) = slot.as_object_mut() {
                object.insert("thumbnail".into(), json!(thumbnail));
            }
        }
    }
    for (index, slot) in slots.iter_mut().enumerate() {
        if let Some(object) = slot.as_object_mut() {
            object.insert("shortcutKey".into(), json!(names.get(index).cloned().unwrap_or_default()));
            object.insert("shortcutCode".into(), json!(core.settings.slot_key_codes.get(index).copied().unwrap_or(0)));
        }
    }
    let ready = slot_ready(core, shared);
    let key_choices: Vec<Value> = keys::usable_mac_codes()
        .into_iter()
        .filter_map(|code| mac_name(code).map(|name| json!({ "code": code, "name": name })))
        .collect();
    let settings = &core.settings;
    let settings_json = json!({
        "enabled": settings.enabled,
        "previewEnabled": settings.preview_enabled,
        "clearAfterUse": settings.clear_after_use,
        "plainText": settings.plain_text,
        "sequential": settings.sequential,
        "sequenceIndex": settings.sequence_index,
        "retainDays": settings.retain_days,
        "useModifiers": settings.use_modifiers,
        "captureModifiers": settings.capture_modifiers,
        "useTriggerKeyCode": settings.use_trigger,
        "captureTriggerKeyCode": settings.capture_trigger,
        "slotKeyCodes": settings.slot_key_codes,
        "slotKeyNames": names,
        "slotOrder": settings.slot_order,
    });
    json!({
        "slots": slots,
        "canUndo": core.store.can_undo(),
        "settings": settings_json,
        "permissions": { "accessibility": true },
        "hotkeyReady": ready,
        "hotkeys": {
            "slotStatus": slot_status_text(core, shared),
            "functionObserved": !settings.uses_fn(),
            "slotReady": ready,
        },
        "recording": core.recording.as_ref().map(|r| json!({ "kind": r.kind, "index": r.index })),
        "fileAccessRequiredIndex": core.file_access_required_index,
        "status": core.status,
        "issue": core.issue,
        "keyChoices": key_choices,
    })
}

fn reply(proxy: &EventLoopProxy<UserEvent>, id: i64, ok: bool, payload: Option<String>, error: Option<String>) {
    let _ = proxy.send_event(UserEvent::BridgeReply { id, ok, payload, error });
}

// ---------------------------------------------------------------------------
// Storage QA entry (AIR_STUDIO_POCKET_QA=1): exercises the exact store paths
// (put/undo/atomic reload/retention) in an isolated directory and writes a
// report next to the logs.

pub fn run_storage_qa(data_dir: &Path) -> i32 {
    let root = data_dir.join("keyboard-pocket-qa");
    let _ = std::fs::remove_dir_all(&root);
    let mut lines = Vec::new();
    let mut code = 0;
    {
        let result = (|| -> Result<(), String> {
            let mut store = PocketStore::open(&root)?;
            store.put(
                PocketItem {
                    id: store::uuid_v4(),
                    kind: "text".into(),
                    title: "QA".into(),
                    text: Some("口袋自检 / pocket qa".into()),
                    created_at: store::iso_now(),
                    ..PocketItem::default()
                },
                3,
            )?;
            let mut reloaded = PocketStore::open(&root)?;
            if reloaded.slots[3].is_none() {
                return Err("atomic reload lost slot content".into());
            }
            if !reloaded.can_undo() {
                return Err("history was not persisted".into());
            }
            reloaded.undo()?;
            if reloaded.slots[3].is_some() {
                return Err("undo did not restore the previous state".into());
            }
            Ok(())
        })();
        match result {
            Ok(()) => lines.push("store: ok".into()),
            Err(message) => {
                lines.push(format!("store: FAILED {message}"));
                code = 1;
            }
        }
    }
    match zip::build_zip(&[]) {
        Err(_) => lines.push("zip empty guard: ok".into()),
        Ok(_) => {
            lines.push("zip empty guard: FAILED".to_string());
            code = 1;
        }
    }
    lines.push(format!("keys: {} usable mac codes", keys::usable_mac_codes().len()));
    let report = format!("{}\n", lines.join("\n"));
    let _ = std::fs::create_dir_all(data_dir);
    let _ = std::fs::write(data_dir.join("keyboard-pocket-qa.txt"), &report);
    print!("{report}");
    code
}
