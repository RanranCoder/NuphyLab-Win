// NuPhyLab Windows shell (port of native/AppHost.swift): tao window + wry
// WebView2, tray icon, node worker orchestration and the graceful quit chain
// (stdin EOF -> server restore chain -> 45s watchdog dialog).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod autostart;
mod capture;
mod dragconvert;
mod hotkey;
mod https;
mod pocket;
mod power;
mod selection;
mod theme;
mod traymenu;
mod webview;
mod win;
mod worker;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopProxy};
use tao::window::{Icon, Window, WindowBuilder};
use traymenu::{Tray, TrayAction};
use wry::WebView;

use worker::{WorkerLog, WorkerSlot};

#[derive(Debug)]
pub enum UserEvent {
    Ready { url: String },
    WorkerExited { code: Option<i32> },
    /// typingSounds bridge reply after the /api/typing round trip on the
    /// worker thread; payload/error are pre-serialized JSON fragments.
    BridgeReply { id: i64, ok: bool, payload: Option<String>, error: Option<String> },
    LanguageChanged { english: bool },
    PowerChanged { sleeping: bool },
    OsThemeChanged { dark: bool, accent: String },
    Tray(traymenu::TrayAction),
    QuitStillRestoring,
    FunctionKey { mac: u8, down: bool },
    QaProbe,
    QaAutoQuit,
    /// Keyboard Pocket panel visibility flips (worker thread → UI thread).
    PocketPanel { show: bool },
    /// Pocket recording watchdog: forwarded to the pocket worker for matching.
    PocketRecordTimeout { session: String },
    /// Smart Selection windows and streaming updates (worker thread → UI).
    SelectionOverlay { show: bool },
    SelectionShortcutReleased,
    SelectionPanel { show: bool, x: i32, y: i32, generation: u64, action: String },
    SelectionThumbnail { png: Vec<u8> },
    SelectionDelta { generation: u64, text: String },
    SelectionDone { generation: u64, ok: bool, text: String },
    SelectionRecordTimeout { session: String },
}

#[derive(Default)]
pub struct ShellState {
    pub terminating: bool,
    pub english: bool,
    pub base_url: Option<String>,
    pub token: String,
    pub poll_generation: u64,
}


#[derive(Clone, Serialize, Deserialize)]
struct WindowBounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    maximized: bool,
}

fn resolve_root() -> PathBuf {
    // Development override; the packaged layout resolves next to the exe.
    if let Some(root) = std::env::var_os("AIR_STUDIO_SHELL_ROOT") {
        return PathBuf::from(root);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn resolve_data_dir() -> PathBuf {
    // The packaged shell always owns this env var; honoring a preset value is
    // a development/QA affordance to run against an isolated data directory.
    if let Some(dir) = std::env::var_os("AIR_STUDIO_DATA") {
        return PathBuf::from(dir);
    }
    std::env::var_os("APPDATA")
        .map(|base| PathBuf::from(base).join("Air Studio"))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read_language(data_dir: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(data_dir.join("library.json")) else { return false };
    let Ok(value) = serde_json::from_str::<Value>(&text) else { return false };
    value.get("settings").and_then(|s| s.get("language")).and_then(Value::as_str) == Some("en")
}

fn read_token(data_dir: &std::path::Path) -> String {
    let Ok(text) = std::fs::read_to_string(data_dir.join("runtime.json")) else { return String::new() };
    let Ok(value) = serde_json::from_str::<Value>(&text) else { return String::new() };
    value.get("token").and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn load_icon_rgba() -> Option<(Vec<u8>, u32, u32)> {
    let bytes = include_bytes!("../../../ui/public/nuphy-studio.png");
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buffer).ok()?;
    match frame.color_type {
        png::ColorType::Rgba => Some((buffer, frame.width, frame.height)),
        png::ColorType::Rgb => {
            let mut rgba = Vec::with_capacity(frame.width as usize * frame.height as usize * 4);
            for pixel in buffer.chunks_exact(3) {
                rgba.extend_from_slice(pixel);
                rgba.push(255);
            }
            Some((rgba, frame.width, frame.height))
        }
        _ => None,
    }
}

fn load_bounds(data_dir: &std::path::Path) -> Option<WindowBounds> {
    let text = std::fs::read_to_string(data_dir.join("window-state.json")).ok()?;
    let bounds: WindowBounds = serde_json::from_str(&text).ok()?;
    if bounds.width < 720 || bounds.height < 520 {
        return None;
    }
    // Drop off-screen state (monitor unplugged or layout changed).
    let (vx, vy, vw, vh) = win::virtual_screen();
    if vw <= 0 || vh <= 0 {
        return None;
    }
    let visible = bounds.x >= vx - 64
        && bounds.y >= vy - 64
        && bounds.x < vx + vw
        && bounds.y < vy + vh
        && bounds.x + bounds.width as i32 > vx
        && bounds.y + bounds.height as i32 > vy;
    visible.then_some(bounds)
}

fn save_bounds(window: &Window, last_normal: Option<WindowBounds>, data_dir: &std::path::Path) {
    let Some(mut bounds) = last_normal else { return };
    bounds.maximized = window.is_maximized();
    let _ = std::fs::write(data_dir.join("window-state.json"), serde_json::to_vec(&bounds).unwrap_or_default());
}

fn default_window_placement(scale: f64) -> (LogicalSize<f64>, PhysicalPosition<i32>) {
    let area = win::primary_work_area();
    let Some(area) = area else {
        return (LogicalSize::new(1000.0, 660.0), PhysicalPosition::new(120, 90));
    };
    let visible_w = (area.right - area.left) as f64 / scale;
    let visible_h = (area.bottom - area.top) as f64 / scale;
    let width = (visible_w * 0.85).clamp(720.0, 1100.0);
    let height = (visible_h * 0.82 - 28.0).clamp(520.0, 700.0);
    let x = area.left as f64 + ((area.right - area.left) as f64 - width * scale) / 2.0;
    let y = area.top as f64 + ((area.bottom - area.top) as f64 - height * scale) / 2.0;
    (LogicalSize::new(width, height), PhysicalPosition::new(x as i32, y as i32))
}

fn snapshot_target(state: &Arc<Mutex<ShellState>>) -> Option<(String, String)> {
    let guard = state.lock().unwrap();
    guard.base_url.clone().map(|base| (base, guard.token.clone()))
}

fn start_poll(state: Arc<Mutex<ShellState>>, hotkeys: hotkey::SharedHotkeys, log: Arc<WorkerLog>, proxy: EventLoopProxy<UserEvent>) {
    let generation = state.lock().unwrap().poll_generation;
    std::thread::spawn(move || loop {
        {
            let guard = state.lock().unwrap();
            if guard.terminating || guard.poll_generation != generation || guard.base_url.is_none() {
                break;
            }
        }
        // One request at a time, mirroring hotkeyStateTask: the 3s timeout
        // can exceed the 500ms cadence without piling up queries.
        let Some((base, token)) = snapshot_target(&state) else { break };
        if let Some(value) = https::get_state(&base, &token, Duration::from_secs(3)) {
            let english = value.get("settings").and_then(|s| s.get("language")).and_then(Value::as_str) == Some("en");
            let changed = {
                let mut guard = state.lock().unwrap();
                if guard.terminating || guard.poll_generation != generation {
                    break;
                }
                if guard.english != english {
                    guard.english = english;
                    true
                } else {
                    false
                }
            };
            if changed {
                let _ = proxy.send_event(UserEvent::LanguageChanged { english });
            }
            // AudioCapture.sync parity: desired capture state comes from the
            // same poll; a failed / missed answer must NOT be read as
            // "disabled" (BUG_AUDIT P2-2) — sync only runs on real answers.
            // The screen-ambient source routes to the DXGI capture module.
            let capture_desired = value.get("audio").and_then(|a| a.get("capture"));
            if capture_desired.and_then(|c| c.get("source")).and_then(Value::as_str) == Some("screen") {
                audio::sync(None, &base, &token, &log);
                capture::sync(capture_desired, &base, &token, &log);
            } else {
                capture::stop_active();
                audio::sync(capture_desired, &base, &token, &log);
            }
        }
        // codexKeyStatus refresh (throttled internally) plus held-slot flush
        // after a gate flip.
        hotkey::refresh_tick(&hotkeys, &base, &token);
        std::thread::sleep(Duration::from_millis(500));
    });
    // Server gone or polling superseded: stop capture (watchdog parity).
    audio::stop_active();
}

fn begin_terminate(
    state: &Arc<Mutex<ShellState>>,
    hotkeys: &hotkey::SharedHotkeys,
    hotkey_engine: &mut hotkey::HotkeyEngine,
    pocket_hook: &mut pocket::PocketHook,
    drag_hook: &mut dragconvert::trigger::DragHook,
    selection_hook: &mut selection::hook::SelectionHook,
    worker_slot: &WorkerSlot,
    worker_exited: &Arc<AtomicBool>,
    webview_slot: &Rc<RefCell<Option<WebView>>>,
    log: &Arc<WorkerLog>,
    proxy: &EventLoopProxy<UserEvent>,
) {
    {
        let mut guard = state.lock().unwrap();
        if guard.terminating {
            return;
        }
        guard.terminating = true;
        guard.poll_generation += 1;
    }
    hotkeys.lock().unwrap().terminating = true;
    // stopHotkeyMonitoring parity: audio and screen capture stop and held
    // slots release before the server starts its restore chain; the queue
    // drains buffered sends before closing.
    audio::stop_active();
    capture::stop_active();
    hotkey_engine.stop();
    pocket_hook.stop();
    drag_hook.stop();
    selection_hook.stop();
    if let Some(webview) = webview_slot.borrow().as_ref() {
        let _ = webview.evaluate_script(
            "window.__AIR_EXITING__ = true; window.dispatchEvent(new Event('nuphylab:shutdown'));",
        );
    }
    let mut slot = worker_slot.lock().unwrap();
    if let Some(worker) = slot.as_mut() {
        if !worker.request_graceful_exit() {
            log.append(b"Worker shutdown signal failed: stdin already closed; force terminating\n");
            worker.kill();
        }
        // stdin closed: node runs its restore chain and exits on its own.
    }
    drop(slot);
    let proxy = proxy.clone();
    let exited = worker_exited.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(45));
        if !exited.load(Ordering::SeqCst) {
            let _ = proxy.send_event(UserEvent::QuitStillRestoring);
        }
    });
}

/// Port of workerDidExit: backup-aware message boxes. Blocking (runs its own
/// message pump), like the macOS modal alerts.
fn show_worker_exit_dialog(data_dir: &std::path::Path, log_path: &std::path::Path, english: bool, failed: bool) {
    let t = |zh: &'static str, en: &'static str| -> &'static str { if english { en } else { zh } };
    let has_backup = [
        "original-lighting.json",
        "original-lighting.profile-0.json",
        "original-lighting.profile-1.json",
    ]
    .iter()
    .any(|name| data_dir.join(name).exists());
    let has_key_backup = ["key-mapping-backup.json", "function-key-backup.json"]
        .iter()
        .any(|name| data_dir.join(name).exists());
    let title = if failed {
        if has_backup {
            t("未能确认键盘恢复", "Keyboard Restore Unconfirmed")
        } else if has_key_backup {
            t("未能确认功能键恢复", "Function Key Restore Unconfirmed")
        } else {
            t("退出检查未完成", "Quit Checks Incomplete")
        }
    } else {
        t("灯光服务已停止", "Lighting Service Stopped")
    };
    let backup_message = if has_backup {
        t(
            "原始灯效备份仍保留在本机。连接键盘后重新打开 NuPhyLab，可再次尝试恢复。",
            "The original lighting backup is preserved on this computer. Connect the keyboard and reopen NuPhyLab to retry restoration.",
        )
    } else if has_key_backup {
        t(
            "部分功能键尚未确认恢复，按键适配备份已保留。重新打开 NuPhyLab 后会再次尝试恢复。",
            "Some function keys could not be verified as restored. The key mapping backup is preserved. Reopen NuPhyLab to retry.",
        )
    } else {
        t(
            "服务已经停止，但退出检查未全部通过。详细原因已记录在日志中。",
            "The service stopped, but some quit checks did not pass. Details have been saved in the log.",
        )
    };
    let log_exists = log_path.exists();
    let mut text = backup_message.to_string();
    if log_exists {
        text.push_str(&format!("\n{}\n{}", t("诊断日志已保存在：", "Diagnostic logs saved at:"), log_path.display()));
    } else {
        text.push_str(t("\n诊断日志未能写入本机存储。", "\nUnable to save diagnostic logs on this computer."));
    }
    win::alert(&text, title, failed);
    if log_exists && win::ask_yes_no(t("打开日志所在文件夹？", "Open the log folder?"), "NuPhyLab") {
        win::show_in_explorer(&log_path.to_string_lossy());
    }
}

fn main() {
    let root = resolve_root();
    let data_dir = resolve_data_dir();
    // Keyboard Pocket storage QA: exercises the exact store paths in an
    // isolated directory and exits (QA / field-support affordance).
    if std::env::var("AIR_STUDIO_POCKET_QA").ok().as_deref() == Some("1") {
        let code = pocket::run_storage_qa(&data_dir);
        std::process::exit(code);
    }
    // Drag Convert conversion QA: real image/zip/tar/mergePDF round trips in
    // an isolated directory (QA / field-support affordance).
    if std::env::var("AIR_STUDIO_DRAG_QA").ok().as_deref() == Some("1") {
        let code = dragconvert::run_conversion_qa(&data_dir);
        std::process::exit(code);
    }
    // Smart Selection QA: pure logic paths (rect normalization, shortcut
    // conversion, tables, state machine, persistence) in an isolated
    // directory — no screen, no server, no API key required.
    if std::env::var("AIR_STUDIO_SELECTION_QA").ok().as_deref() == Some("1") {
        let code = selection::run_selection_qa(&data_dir);
        std::process::exit(code);
    }
    // Diagnostics entry point (QA / field support): duplicate the primary
    // display for a few seconds, write the zone grid next to the logs and exit.
    // Verifies the exact code path the screen-ambient source uses.
    if let Ok(seconds) = std::env::var("AIR_STUDIO_SCREEN_PROBE") {
        let seconds = seconds.parse::<f64>().unwrap_or(5.0);
        let report = match capture::probe_duplication(seconds) {
            Ok(summary) => capture::probe_report(&summary),
            Err(error) => format!("screen probe failed: {error}\n"),
        };
        let target = data_dir.join("screen-probe.txt");
        let _ = std::fs::create_dir_all(&data_dir);
        let _ = std::fs::write(&target, &report);
        print!("{report}");
        std::process::exit(if report.contains("screen probe failed") { 1 } else { 0 });
    }
    // Autostart self-check: enable, read back, then restore the previous state.
    // Exercises the exact registry path the tray toggle uses.
    if std::env::var("AIR_STUDIO_AUTOSTART_QA").ok().as_deref() == Some("1") {
        let exe = autostart::current_exe_path().unwrap_or_default();
        let before = autostart::enabled_for(&exe);
        let mut lines = format!("exe: {exe}\nbefore: {before}\n");
        let mut code = 0;
        match autostart::set(true, &exe) {
            Ok(()) => {
                let stored = autostart::read_run_value().unwrap_or_default();
                let enabled = autostart::enabled_for(&exe);
                lines.push_str(&format!("after enable: stored={stored} enabled={enabled}\n"));
                if !enabled || !stored.contains("NuPhyLab.exe") {
                    code = 1;
                }
            }
            Err(error) => {
                lines.push_str(&format!("enable failed: {error}\n"));
                code = 1;
            }
        }
        if let Err(error) = autostart::set(before, &exe) {
            lines.push_str(&format!("restore failed: {error}\n"));
            code = 1;
        }
        lines.push_str(&format!("restored: {}\n", autostart::enabled_for(&exe)));
        let target = data_dir.join("autostart-qa.txt");
        let _ = std::fs::create_dir_all(&data_dir);
        let _ = std::fs::write(&target, &lines);
        print!("{lines}");
        std::process::exit(code);
    }
    let log_path = data_dir.join("Logs").join("worker.log");
    let log = Arc::new(WorkerLog::new(log_path.clone()));
    let english = read_language(&data_dir);
    let state = Arc::new(Mutex::new(ShellState { english, ..Default::default() }));
    let hotkeys = hotkey::shared();
    let mut hotkey_engine = hotkey::HotkeyEngine::new(hotkeys.clone());

    let event_loop = tao::event_loop::EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let scale = event_loop.primary_monitor().map(|m| m.scale_factor()).unwrap_or(1.0);

    let (window_icon, tray_icon) = match load_icon_rgba() {
        Some((rgba, width, height)) => (
            Icon::from_rgba(rgba.clone(), width, height).ok(),
            tray_icon::Icon::from_rgba(rgba, width, height).ok(),
        ),
        None => (None, None),
    };
    let saved_bounds = load_bounds(&data_dir);
    let mut builder = WindowBuilder::new()
        .with_title("NuPhyLab")
        .with_resizable(true)
        .with_min_inner_size(LogicalSize::new(720.0, 520.0));
    builder = match window_icon {
        Some(icon) => builder.with_window_icon(Some(icon)),
        None => builder,
    };
    builder = match &saved_bounds {
        Some(bounds) => builder
            .with_position(PhysicalPosition::new(bounds.x, bounds.y))
            .with_inner_size(tao::dpi::PhysicalSize::new(bounds.width, bounds.height))
            .with_maximized(bounds.maximized),
        None => {
            let (size, position) = default_window_placement(scale);
            builder.with_inner_size(size).with_position(position)
        }
    };
    let window = builder.build(&event_loop).expect("window");

    // Keyboard Pocket: shared hook runtime + worker thread + native panel
    // window (hidden until a prefix hold or the openPanel op shows it).
    let pocket_shared = pocket::SharedPocket::new(std::sync::Mutex::new(pocket::PocketRuntime::new()));
    let (pocket_handle, pocket_core) = pocket::spawn(&data_dir, log.clone(), proxy.clone(), pocket_shared.clone());
    let mut pocket_hook = pocket::PocketHook::new(pocket_shared.clone());
    let pocket_panel = pocket::panel::Panel::create(&event_loop);

    // Drag Convert: wheel-menu window thread + worker + independent trigger
    // hook (never swallows keystrokes; the menu only opens during file drags).
    let (drag_handle, drag_core, drag_runtime) = dragconvert::spawn(&data_dir, log.clone(), proxy.clone(), english);
    let mut drag_hook = dragconvert::trigger::DragHook::new(drag_runtime.clone());

    // Smart Selection: shared hook runtime + worker thread + the overlay
    // (fullscreen region selector) and answer panel windows.
    let selection_shared = selection::hook::SharedSelection::new(std::sync::Mutex::new(selection::hook::SelectionRuntime::new()));
    let selection_handle = selection::spawn(&data_dir, log.clone(), proxy.clone(), selection_shared.clone());
    let mut selection_hook = selection::hook::SelectionHook::new(selection_shared.clone());
    let mut selection_overlay = selection::overlay::Overlay::create(&event_loop);
    let mut selection_panel = selection::panel::Panel::create(&event_loop, selection_handle.sender());

    let mut tray = match tray_icon {
        Some(icon) => {
            let exe = autostart::current_exe_path().unwrap_or_default();
            let enabled = !exe.is_empty() && autostart::enabled_for(&exe);
            if !exe.is_empty() {
                // A moved install (portable zip) leaves a stale Run entry;
                // self-heal so the toggle always reflects this location.
                if enabled {
                    let _ = autostart::set(true, &exe);
                }
            }
            Tray::build(icon, english, enabled, proxy.clone())
        }
        None => {
            log.append(b"Tray icon asset missing; tray unavailable\n");
            // A shell without a tray cannot be quit gracefully; refuse to run half-alive.
            win::alert("图标资源缺失，无法启动托盘。", "NuPhyLab", true);
            std::process::exit(1);
        }
    };

    let worker_slot: WorkerSlot = Arc::new(Mutex::new(None));
    let worker_exited = Arc::new(AtomicBool::new(false));
    match worker::spawn(&root, &data_dir, log.clone(), proxy.clone()) {
        Ok(worker) => *worker_slot.lock().unwrap() = Some(worker),
        Err(error) => {
            let t = |zh: &'static str, en: &'static str| -> &'static str { if english { en } else { zh } };
            win::alert(&error, t("无法启动 NuPhyLab", "Unable to Start NuPhyLab"), true);
            std::process::exit(1);
        }
    }

    // Sleep/resume notifications and OS theme watching outlive the worker.
    power::watch(proxy.clone());
    theme::watch(proxy.clone());

    if std::env::var("AIR_STUDIO_SHELL_QA").ok().as_deref() == Some("1") {
        let probe_proxy = proxy.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(3000));
            let _ = probe_proxy.send_event(UserEvent::QaProbe);
        });
        let quit_proxy = proxy.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(8000));
            let _ = quit_proxy.send_event(UserEvent::QaAutoQuit);
        });
    }

    let webview_slot: Rc<RefCell<Option<WebView>>> = Rc::new(RefCell::new(None));
    let devtools = std::env::var("AIR_STUDIO_DEVTOOLS").ok().as_deref() == Some("1");

    let mut last_normal = saved_bounds.filter(|b| !b.maximized);
    let mut last_bounds_write = Instant::now();
    let mut pending_save = false;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::Ready { url }) => {
                {
                    let mut guard = state.lock().unwrap();
                    guard.token = read_token(&data_dir);
                    guard.base_url = Some(url.clone());
                }
                hotkey::start_slot_queue(&url, &state.lock().unwrap().token, log.clone());
                hotkey_engine.start(proxy.clone());
                pocket_hook.start(pocket_handle.sender());
                drag_hook.start(&drag_core);
                selection_handle.send(selection::Op::ServerReady { base: url.clone(), token: state.lock().unwrap().token.clone() });
                selection_hook.start(selection_handle.sender());
                match webview::build(&window, &url, hotkeys.clone(), state.clone(), proxy.clone(), log.clone(), webview_slot.clone(), pocket_handle.clone(), drag_handle.clone(), selection_handle.clone()) {
                    Ok(webview) => {
                        if devtools {
                            webview.open_devtools();
                        }
                        // The OS theme watcher sends on change only; apply the
                        // current theme once so the first paint is already right.
                        let current = theme::read();
                        let _ = webview.evaluate_script(&theme::apply_script(&current));
                        *webview_slot.borrow_mut() = Some(webview);
                        start_poll(state.clone(), hotkeys.clone(), log.clone(), proxy.clone());
                    }
                    Err(error) => {
                        log.append(format!("WebView build failed: {}\n", error).as_bytes());                        let t = |zh: &'static str, en: &'static str| -> &'static str { if english { en } else { zh } };
                        // Win10 machines without the Evergreen runtime fail here;
                        // Win11 ships it. Point the user at the installer.
                        let message = error.to_string();
                        let runtime_missing = ["webview2", "0x80070002", "cannot find the file"]
                            .iter()
                            .any(|needle| message.to_lowercase().contains(*needle));
                        if runtime_missing {
                            let prompt = t(
                                "未检测到 WebView2 运行时（Windows 10 需要单独安装，Windows 11 已内置）。\
是否打开官方下载页面？安装完成后重新运行 NuPhyLab。",
                                "The WebView2 Runtime was not found (Windows 10 needs a separate install; \
Windows 11 ships it). Open the official download page? After installing, run NuPhyLab again.",
                            );
                            let caption = t("缺少 WebView2 运行时", "WebView2 Runtime Missing");
                            if win::ask_yes_no(prompt, caption) {
                                win::shell_open("https://developer.microsoft.com/microsoft-edge/webview2/");
                            }
                        } else {
                            win::alert(&message, t("无法启动 NuPhyLab", "Unable to Start NuPhyLab"), true);
                        }
                        std::process::exit(1);
                    }
                }
            }
            Event::UserEvent(UserEvent::LanguageChanged { english }) => {
                tray.set_language(english);
                dragconvert::set_english(english);
            }
            Event::UserEvent(UserEvent::PocketPanel { show }) => {
                if show {
                    pocket_panel.show();
                    if let Ok(guard) = pocket_core.lock() {
                        pocket_panel.repaint(&guard);
                    }
                } else {
                    pocket_panel.hide();
                }
            }
            Event::UserEvent(UserEvent::PocketRecordTimeout { session }) => {
                pocket_handle.send(pocket::Op::RecordTimeout { session });
            }
            Event::UserEvent(UserEvent::SelectionOverlay { show }) => {
                if show {
                    selection_overlay.show();
                } else {
                    selection_overlay.hide();
                }
            }
            Event::UserEvent(UserEvent::SelectionShortcutReleased) => {
                // Released before dragging: cancel quietly (keep the overlay
                // when a drag is in progress).
                if selection_overlay.cancel_if_undragged() {
                    selection_handle.send(selection::Op::SelectionCancelled);
                }
            }
            Event::UserEvent(UserEvent::SelectionPanel { show, x, y, generation, action }) => {
                if show {
                    if selection_panel.generation() != generation {
                        selection_panel.begin_request(generation);
                    }
                    selection_panel.set_action(&action);
                    if x >= 0 && y >= 0 {
                        selection_panel.show_at(x, y);
                    } else {
                        // Keep the current position (action switch / follow-up).
                        selection_panel.window.set_visible(true);
                        selection_panel.window.request_redraw();
                    }
                } else {
                    selection_panel.hide();
                }
            }
            Event::UserEvent(UserEvent::SelectionThumbnail { png }) => {
                selection_panel.set_thumbnail_png(&png);
            }
            Event::UserEvent(UserEvent::SelectionDelta { generation, text }) => {
                selection_panel.append_delta(generation, &text);
            }
            Event::UserEvent(UserEvent::SelectionDone { generation, ok, text }) => {
                selection_panel.finish(generation, ok, &text);
            }
            Event::UserEvent(UserEvent::SelectionRecordTimeout { session }) => {
                selection_handle.send(selection::Op::RecordTimeout { session });
            }
            Event::UserEvent(UserEvent::BridgeReply { id, ok, payload, error }) => {
                // The WebView only runs on the event-loop thread; the async
                // /api/typing reply lands here from the worker thread.
                if let Some(webview) = webview_slot.borrow().as_ref() {
                    let script = webview::bridge_reply_script(id, ok, payload.as_deref(), error.as_deref());
                    let _ = webview.evaluate_script(&script);
                }
            }
            Event::UserEvent(UserEvent::PowerChanged { sleeping }) => {
                // AppHost.willSleep parity: stop capture before the machine
                // suspends; after resume the /api/state poll restarts it.
                if sleeping {
                    log.append(b"[power] suspending: stopping audio and screen capture\n");
                    audio::stop_active();
                    capture::stop_active();
                } else {
                    log.append(b"[power] resumed\n");
                }
            }
            Event::UserEvent(UserEvent::OsThemeChanged { dark, accent }) => {
                if let Some(webview) = webview_slot.borrow().as_ref() {
                    let script = theme::apply_script(&theme::OsTheme { dark, accent });
                    let _ = webview.evaluate_script(&script);
                }
            }
            Event::UserEvent(UserEvent::Tray(action)) => match action {
                TrayAction::Show => {
                    window.set_visible(true);
                    window.set_focus();
                }
                TrayAction::TogglePause | TrayAction::Restore => {
                    if let Some((base, token)) = snapshot_target(&state) {
                        let op = if matches!(action, TrayAction::TogglePause) { "pause" } else { "restore" };
                        std::thread::spawn(move || {
                            https::post_command(&base, &token, &serde_json::json!({ "op": op }), Duration::from_secs(5));
                        });
                    }
                }
                TrayAction::ToggleAutostart => {
                    let exe = autostart::current_exe_path().unwrap_or_default();
                    let enabled = !exe.is_empty() && autostart::enabled_for(&exe);
                    let target = !enabled;
                    match autostart::set(target, &exe) {
                        Ok(()) => {
                            tray.set_autostart(target);
                            log.append(format!("[autostart] {}\n", if target { "enabled" } else { "disabled" }).as_bytes());
                        }
                        Err(error) => {
                            log.append(format!("[autostart] toggle failed: {error}\n").as_bytes());
                            let english_now = state.lock().unwrap().english;
                            let t = |zh: &'static str, en: &'static str| -> &'static str { if english_now { en } else { zh } };
                            win::alert(&error, t("无法更改开机自启", "Cannot Change Start at Login"), true);
                        }
                    }
                }
                TrayAction::Quit => begin_terminate(&state, &hotkeys, &mut hotkey_engine, &mut pocket_hook, &mut drag_hook, &mut selection_hook, &worker_slot, &worker_exited, &webview_slot, &log, &proxy),
            },
            Event::UserEvent(UserEvent::WorkerExited { code }) => {
                worker_exited.store(true, Ordering::SeqCst);
                worker_slot.lock().unwrap().take();
                let terminating = state.lock().unwrap().terminating;
                if terminating {
                    let english_now = state.lock().unwrap().english;
                    if code != Some(0) {
                        show_worker_exit_dialog(&data_dir, &log_path, english_now, true);
                    }
                    save_bounds(&window, last_normal.clone(), &data_dir);
                    *control_flow = ControlFlow::Exit;
                } else {
                    // Unexpected worker death: stop polling and hotkeys,
                    // report, keep the window alive (the page shows the
                    // service-error state).
                    state.lock().unwrap().poll_generation += 1;
                    hotkeys.lock().unwrap().terminating = false;
                    hotkey_engine.stop();
                    let english_now = state.lock().unwrap().english;
                    show_worker_exit_dialog(&data_dir, &log_path, english_now, code != Some(0));
                }
            }
            Event::UserEvent(UserEvent::QuitStillRestoring) => {
                let english_now = state.lock().unwrap().english;
                let t = |zh: &'static str, en: &'static str| -> &'static str { if english_now { en } else { zh } };
                let title = t("仍在恢复键盘灯效", "Restoring Keyboard Lighting");
                let body = t(
                    "原始备份已保存。请保持键盘连接，应用将在恢复流程结束后退出。",
                    "The original backup is saved. Keep the keyboard connected. The app will quit when restoration finishes.",
                );
                // Own thread: the watchdog dialog must not stall the loop
                // servicing the WorkerExited event.
                std::thread::spawn(move || win::alert(body, title, false));
            }
            Event::UserEvent(UserEvent::QaProbe) => {
                if let Some(webview) = webview_slot.borrow().as_ref() {
                    let log = log.clone();
                    let _ = webview.evaluate_script_with_callback(
                        "JSON.stringify({title: document.title, ipc: typeof window.ipc, audioBridge: !!(window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.audioControl), codexKeysExposed: !!window.webkit.messageHandlers.codexKeys, exiting: !!window.__AIR_EXITING__, osTheme: document.documentElement.dataset.osTheme || null, osThemeSystem: document.documentElement.dataset.osThemeSystem || null, accent: getComputedStyle(document.documentElement).getPropertyValue('--accent').trim(), darkPanel: getComputedStyle(document.documentElement).getPropertyValue('--panel').trim(), appearanceSelect: document.querySelector('select[aria-label=\"外观\"], select[aria-label=\"Appearance\"]')?.value ?? null})",
                        move |result| {
                            let line = format!("[QA] {}\n", result);
                            eprint!("{}", line);
                            log.append(line.as_bytes());
                        },
                    );
                }
            }
            Event::UserEvent(UserEvent::FunctionKey { mac, down }) => {
                if let Some(name) = keymap::function_key_name(mac) {
                    if let Some(webview) = webview_slot.borrow().as_ref() {
                        let phase = if down { "down" } else { "up" };
                        let script = format!(
                            "window.dispatchEvent(new CustomEvent('nuphylab:function-key',{{detail:{{code:'{}',phase:'{}'}}}}));",
                            name, phase
                        );
                        let _ = webview.evaluate_script(&script);
                    }
                }
            }
            Event::UserEvent(UserEvent::QaAutoQuit) => {
                begin_terminate(&state, &hotkeys, &mut hotkey_engine, &mut pocket_hook, &mut drag_hook, &mut selection_hook, &worker_slot, &worker_exited, &webview_slot, &log, &proxy);
            }
            Event::WindowEvent { window_id, event: WindowEvent::Focused(focused), .. } if window_id == window.id() => {
                // applicationDidBecomeActive/ResignActive parity: the editor
                // gate only suppresses hotkeys while our own window is active.
                hotkeys.lock().unwrap().window_focused = focused;
            }
            Event::WindowEvent { window_id, event: WindowEvent::CloseRequested, .. } if window_id == window.id() => {
                // Closing the window hides to tray; quitting runs the restore
                // chain from the tray menu (applicationShouldTerminateAfterLastWindowClosed = false).
                save_bounds(&window, last_normal.clone(), &data_dir);
                window.set_visible(false);
            }
            Event::WindowEvent { window_id, event: WindowEvent::Moved(position), .. } if window_id == window.id() => {
                if !window.is_maximized() {
                    let size = window.inner_size();
                    last_normal = Some(WindowBounds { x: position.x, y: position.y, width: size.width, height: size.height, maximized: false });
                    pending_save = true;
                }
            }
            Event::WindowEvent { window_id, event: WindowEvent::Resized(size), .. } if window_id == window.id() => {
                if !window.is_maximized() {
                    if let Ok(position) = window.inner_position() {
                        last_normal = Some(WindowBounds { x: position.x, y: position.y, width: size.width, height: size.height, maximized: false });
                        pending_save = true;
                    }
                }
            }
            // Pocket panel interaction: clicks land on slots (the window never
            // activates, so the foreground app keeps focus for paste targets).
            Event::WindowEvent { window_id, event: WindowEvent::MouseInput { button: tao::event::MouseButton::Left, state: tao::event::ElementState::Pressed, .. }, .. }
                if window_id == pocket_panel.window.id() =>
            {
                if let Some(position) = pocket_panel.cursor_cell() {
                    pocket_handle.send(pocket::Op::PanelClick { position });
                }
            }
            Event::RedrawRequested(window_id) if window_id == pocket_panel.window.id() => {
                if let Ok(guard) = pocket_core.lock() {
                    pocket_panel.repaint(&guard);
                }
            }
            // Smart Selection overlay: press → start the rubber band, move →
            // repaint, release → normalized region to the worker (which runs
            // the DXGI capture and the request).
            Event::WindowEvent { window_id, event: WindowEvent::MouseInput { button: tao::event::MouseButton::Left, state: tao::event::ElementState::Pressed, .. }, .. }
                if window_id == selection_overlay.window.id() =>
            {
                let (x, y) = selection_overlay.client_cursor();
                selection_overlay.on_mouse_down(x, y);
            }
            Event::WindowEvent { window_id, event: WindowEvent::MouseInput { button: tao::event::MouseButton::Left, state: tao::event::ElementState::Released, .. }, .. }
                if window_id == selection_overlay.window.id() =>
            {
                if let Some(rect) = selection_overlay.on_mouse_up() {
                    selection_handle.send(selection::Op::OverlayCaptured { rect });
                }
            }
            Event::WindowEvent { window_id, event: WindowEvent::CursorMoved { .. }, .. }
                if window_id == selection_overlay.window.id() =>
            {
                let (x, y) = selection_overlay.client_cursor();
                selection_overlay.on_mouse_move(x, y);
            }
            Event::RedrawRequested(window_id) if window_id == selection_overlay.window.id() => {
                selection_overlay.repaint();
            }
            Event::WindowEvent { window_id, event: WindowEvent::CloseRequested, .. }
                if window_id == selection_overlay.window.id() || window_id == selection_panel.window.id() =>
            {
                if window_id == selection_overlay.window.id() {
                    selection_overlay.hide();
                    selection_handle.send(selection::Op::SelectionCancelled);
                } else {
                    selection_panel.hide();
                }
            }
            // Smart Selection answer panel: button clicks, wheel action
            // switching, Esc dismiss, drag-to-move (position persists).
            Event::WindowEvent { window_id, event: WindowEvent::MouseInput { button: tao::event::MouseButton::Left, state: tao::event::ElementState::Pressed, .. }, .. }
                if window_id == selection_panel.window.id() =>
            {
                let (x, y) = selection_panel.client_cursor();
                let (copy, send, header) = selection_panel.click(x, y);
                if copy {
                    selection_handle.send(selection::Op::PanelCopy);
                } else if send {
                    let text = selection_panel.read_follow_up();
                    if !text.is_empty() {
                        selection_handle.send(selection::Op::PanelFollowUp { text });
                        selection_panel.clear_follow_up();
                        selection_panel.focus_edit();
                    }
                } else if header {
                    let _ = selection_panel.window.drag_window();
                }
            }
            Event::WindowEvent { window_id, event: WindowEvent::MouseWheel { .. }, .. }
                if window_id == selection_panel.window.id() =>
            {
                selection_handle.send(selection::Op::PanelSwitch);
            }
            Event::WindowEvent { window_id, event: WindowEvent::KeyboardInput { event: key_event, is_synthetic: _, .. }, .. }
                if window_id == selection_panel.window.id() =>
            {
                if key_event.state == tao::event::ElementState::Pressed && key_event.physical_key == tao::keyboard::KeyCode::Escape {
                    selection_panel.hide();
                }
            }
            Event::WindowEvent { window_id, event: WindowEvent::Moved(position), .. }
                if window_id == selection_panel.window.id() && selection_panel.window.is_visible() =>
            {
                selection_handle.send(selection::Op::PanelPosition { x: position.x, y: position.y });
            }
            Event::RedrawRequested(window_id) if window_id == selection_panel.window.id() => {
                selection_panel.repaint();
            }
            Event::LoopDestroyed => {
                if pending_save {
                    save_bounds(&window, last_normal.clone(), &data_dir);
                }
            }
            _ => {}
        }
        if pending_save && last_bounds_write.elapsed() >= Duration::from_secs(1) {
            last_bounds_write = Instant::now();
            pending_save = false;
            save_bounds(&window, last_normal.clone(), &data_dir);
        }
    });
}
