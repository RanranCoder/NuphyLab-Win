// Codex slot hotkeys for the shell, porting the Carbon/event-tap logic of
// AppHost.swift (two-phase held slots, function-key capture for recording,
// editing/focus gating and codexKeyStatus reporting). One WH_KEYBOARD_LL hook
// covers everything; the enable gate is evaluated per event from shared state.
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tao::event_loop::EventLoopProxy;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_MENU, VK_SHIFT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::worker::WorkerLog;
use crate::{https, UserEvent};

const LLKHF_INJECTED: u32 = 0x10;
const VK_LWIN: i32 = 0x5B;
const VK_RWIN: i32 = 0x5C;

#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    pub code: String,
    pub mac: u8,
    pub vk: u16,
}

#[derive(Default)]
pub struct HotkeyState {
    /// Validated bindings from the codexKeys config message (empty = none).
    pub bindings: Vec<Binding>,
    pub keys_enabled: bool,
    pub key_revision: i64,
    pub key_setup_revision: i64,
    pub capturing: bool,
    pub capture_session: Option<String>,
    pub editing: bool,
    pub window_focused: bool,
    /// Monitoring alive: set at ready, cleared on terminate/worker death.
    pub hook_active: bool,
    pub terminating: bool,
    pub held: HashSet<u8>,
    pub captured: HashSet<u8>,
    // codexKeyStatus throttle (reportHotkeys parity).
    status_signature: String,
    status_last: Option<Instant>,
}

pub type SharedHotkeys = Arc<Mutex<HotkeyState>>;

impl HotkeyState {
    /// refreshHotkeys gate: configured + enabled, not recording, not typing in
    /// a focused editor, monitoring alive, not exiting.
    pub fn effective_enabled(&self) -> bool {
        !self.bindings.is_empty()
            && self.keys_enabled
            && !self.capturing
            && !(self.editing && self.window_focused)
            && self.hook_active
            && !self.terminating
    }
}

// ---------------------------------------------------------------------------
// Serial codexKey queue (sendNextKey parity: one request at a time, 5s timeout)

static SLOT_SENDER: Mutex<Option<mpsc::Sender<(String, String)>>> = Mutex::new(None);
static SLOT_RUNNING: AtomicBool = AtomicBool::new(false);

pub fn start_slot_queue(base: &str, token: &str, log: Arc<WorkerLog>) {
    let (sender, receiver) = mpsc::channel::<(String, String)>();
    *SLOT_SENDER.lock().unwrap() = Some(sender);
    SLOT_RUNNING.store(true, Ordering::SeqCst);
    let base = base.to_owned();
    let token = token.to_owned();
    std::thread::spawn(move || {
        while let Ok((code, phase)) = receiver.recv() {
            https::post_command(&base, &token, &json!({"op": "codexKey", "code": code, "phase": phase}), Duration::from_secs(5));
            if phase == "up" {
                log.append(format!("[hotkey] slot {code} released\n").as_bytes());
            }
        }
    });
}

pub fn stop_slot_queue() {
    SLOT_RUNNING.store(false, Ordering::SeqCst);
    *SLOT_SENDER.lock().unwrap() = None;
}

pub fn enqueue_slot(code: &str, phase: &str) {
    if !SLOT_RUNNING.load(Ordering::SeqCst) {
        return;
    }
    if let Some(sender) = SLOT_SENDER.lock().unwrap().as_ref() {
        let _ = sender.send((code.to_owned(), phase.to_owned()));
    }
}

/// Periodic refresh driven by the /api/state poll (0.5s): flush slots left
/// held when the gate flipped off, then post codexKeyStatus on change.
pub fn refresh_tick(state: &SharedHotkeys, base: &str, token: &str) {
    let (registered, suspended, flush, signature, elapsed) = {
        let mut guard = state.lock().unwrap();
        let enabled = guard.effective_enabled();
        if !enabled && !guard.held.is_empty() {
            let held: Vec<u8> = guard.held.drain().collect();
            for mac in held {
                if let Some(binding) = guard.bindings.iter().find(|b| b.mac == mac) {
                    enqueue_slot(&binding.code, "up");
                }
            }
        }
        let registered: Vec<String> = if enabled { guard.bindings.iter().map(|b| b.code.clone()).collect() } else { Vec::new() };
        let suspended = guard.capturing || (guard.editing && guard.window_focused);
        let signature = format!("{}|{suspended}", registered.join(","));
        let elapsed = guard.status_last.map(|at| at.elapsed().as_secs() >= 5).unwrap_or(true);
        (registered, suspended, enabled, signature, elapsed)
    };
    let _ = flush;
    if base.is_empty() {
        return;
    }
    let changed = {
        let mut guard = state.lock().unwrap();
        if guard.status_signature == signature && !elapsed {
            false
        } else {
            guard.status_signature = signature.clone();
            guard.status_last = Some(Instant::now());
            true
        }
    };
    if changed {
        let base = base.to_owned();
        let token = token.to_owned();
        std::thread::spawn(move || {
            https::post_command(&base, &token, &json!({"op": "codexKeyStatus", "registered": registered, "failed": [], "suspended": suspended}), Duration::from_secs(5));
        });
    }
}

// ---------------------------------------------------------------------------
// The hook

thread_local! {
    static CONTEXT: std::cell::RefCell<Option<HookContext>> = const { std::cell::RefCell::new(None) };
}

struct HookContext {
    state: SharedHotkeys,
    proxy: EventLoopProxy<UserEvent>,
    /// Marks whether a key is physically down; doubles as the repeat filter
    /// (Windows' low-level hook has no autorepeat flag).
    down: HashSet<u16>,
}

fn any_modifier() -> bool {
    let pressed = |vk: i32| unsafe { GetAsyncKeyState(vk) as u16 & 0x8000 != 0 };
    pressed(VK_SHIFT as i32) || pressed(VK_CONTROL as i32) || pressed(VK_MENU as i32) || pressed(VK_LWIN) || pressed(VK_RWIN)
}

fn send_slot(state: &HotkeyState, mac: u8, phase: &str) {
    if let Some(binding) = state.bindings.iter().find(|b| b.mac == mac) {
        enqueue_slot(&binding.code, phase);
    }
}

/// AppHost.handleFunctionKey + the slot handler, unified for every binding.
fn handle_event(
    context: &mut HookContext,
    mac: Option<u8>,
    vk: u16,
    is_function: bool,
    down: bool,
    fresh: bool,
    suppress: &mut bool,
) {
    let mut state = context.state.lock().unwrap();
    if state.terminating {
        return;
    }
    let capture_mode = state.capturing && state.window_focused;
    if capture_mode && is_function {
        let Some(mac) = mac else { return };
        // Recording intercepts function keys globally (the browser sees the
        // rest while the page has focus). Only recorded keyUps are swallowed;
        // a keyUp for a key that was never recorded down passes through.
        if !down {
            if state.captured.remove(&mac) {
                let _ = context.proxy.send_event(UserEvent::FunctionKey { mac, down: false });
            } else {
                return;
            }
            *suppress = true;
            return;
        }
        if !any_modifier() && fresh && state.captured.insert(mac) {
            let _ = context.proxy.send_event(UserEvent::FunctionKey { mac, down: true });
        }
        *suppress = true;
        return;
    }
    if !state.effective_enabled() {
        // While disabled, still release slots that were held when the gate
        // flipped so the integration never sees a stuck key.
        if !down {
            if let Some(mac) = mac {
                if state.held.remove(&mac) {
                    send_slot(&state, mac, "up");
                }
            }
        }
        return;
    }
    let Some(binding) = state.bindings.iter().find(|b| b.vk == vk).map(|b| (b.mac, b.code.clone())) else { return };
    let (mac, code) = binding;
    if !down {
        if state.held.remove(&mac) {
            enqueue_slot(&code, "up");
        }
        *suppress = true;
        return;
    }
    if !any_modifier() && fresh && state.held.insert(mac) {
        enqueue_slot(&code, "down");
    }
    *suppress = true;
}

unsafe extern "system" fn low_level_callback(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
    }
    let info = &*(lparam as *const KBDLLHOOKSTRUCT);
    // The bridge's shortcut injection must never trigger slot actions.
    if info.flags & LLKHF_INJECTED != 0 {
        return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
    }
    let down = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
    let up = matches!(wparam as u32, WM_KEYUP | WM_SYSKEYUP);
    if !down && !up {
        return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
    }
    let vk = info.vkCode as u16;
    let mut suppress = false;
    CONTEXT.with(|cell| {
        let Ok(mut context) = cell.try_borrow_mut() else { return };
        let Some(context) = context.as_mut() else { return };
        let mac = keymap::mac_from_vk(vk);
        let is_function = mac.is_some_and(|code| keymap::function_key_name(code).is_some());
        if down {
            let fresh = context.down.insert(vk);
            handle_event(context, mac, vk, is_function, down, fresh, &mut suppress);
        } else {
            context.down.remove(&vk);
            handle_event(context, mac, vk, is_function, down, true, &mut suppress);
        }
    });
    if suppress {
        1
    } else {
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    }
}

pub struct HotkeyEngine {
    state: SharedHotkeys,
    thread: Option<HookHandle>,
}

struct HookHandle {
    thread_id: u32,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl HotkeyEngine {
    pub fn new(state: SharedHotkeys) -> HotkeyEngine {
        HotkeyEngine { state, thread: None }
    }

    /// Installs the hook and starts the codexKey queue (startHotkeyMonitoring
    /// parity: called on ready; the hook stays for the whole session).
    pub fn start(&mut self, proxy: EventLoopProxy<UserEvent>) {
        {
            let mut state = self.state.lock().unwrap();
            state.hook_active = true;
            state.held.clear();
        }
        if self.thread.is_none() {
            let state = self.state.clone();
            let (sender, receiver) = mpsc::channel::<Option<u32>>();
            let handle = std::thread::spawn(move || unsafe {
                CONTEXT.with(|cell| {
                    *cell.borrow_mut() = Some(HookContext { state: state.clone(), proxy, down: HashSet::new() });
                });
                let thread_id = GetCurrentThreadId();
                let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_callback), std::ptr::null_mut(), 0);
                if hook.is_null() {
                    let _ = sender.send(None);
                    return;
                }
                let _ = sender.send(Some(thread_id));
                let mut message: MSG = std::mem::zeroed();
                while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {}
                UnhookWindowsHookEx(hook);
            });
            match receiver.recv() {
                Ok(Some(thread_id)) => self.thread = Some(HookHandle { thread_id, handle: Some(handle) }),
                _ => {
                    let _ = handle.join();
                }
            }
        }
    }

    /// stopHotkeyMonitoring parity: flush held slots, drop the hook, stop the
    /// queue. Used on terminate and on unexpected worker death.
    pub fn stop(&mut self) {
        let held: Vec<u8> = {
            let mut state = self.state.lock().unwrap();
            state.hook_active = false;
            state.capturing = false;
            std::mem::take(&mut state.held).into_iter().collect()
        };
        for mac in held {
            send_slot(&self.state.lock().unwrap(), mac, "up");
        }
        stop_slot_queue();
        if let Some(mut thread) = self.thread.take() {
            unsafe {
                PostThreadMessageW(thread.thread_id, WM_QUIT, 0, 0);
            }
            if let Some(handle) = thread.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

/// Applies the codexKeys `config` message (AppHost.applyKeyConfiguration).
/// The page sends this on every /api/state refresh; stale revisions are
/// ignored, invalid payloads disable the bindings.
pub fn apply_config(state: &SharedHotkeys, value: Option<&Value>) {
    let mut guard = state.lock().unwrap();
    if let Some(revision) = value.and_then(|v| v.get("revision")).and_then(Value::as_i64) {
        if revision < guard.key_revision {
            return;
        }
        guard.key_revision = revision;
    }
    if let Some(setup) = value.and_then(|v| v.get("setupRevision")).and_then(Value::as_i64) {
        if setup != guard.key_setup_revision {
            guard.key_setup_revision = setup;
            guard.keys_enabled = false;
        }
    }
    let bindings = value
        .and_then(|v| v.get("bindings"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let code = item.get("code").and_then(Value::as_str)?.to_owned();
                    let mac = item.get("keyCode").and_then(Value::as_i64)?;
                    if !(0..=127).contains(&mac) {
                        return None;
                    }
                    let vk = keymap::vk_from_mac(mac as u8)?;
                    Some(Binding { code, mac: mac as u8, vk })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let codes_unique = bindings.iter().map(|b| b.code.as_str()).collect::<HashSet<_>>().len() == bindings.len();
    let keys_unique = bindings.iter().map(|b| b.mac).collect::<HashSet<_>>().len() == bindings.len();
    let valid = bindings.len() == 12 && codes_unique && keys_unique;
    guard.keys_enabled = valid && value.and_then(|v| v.get("enabled")) == Some(&Value::Bool(true));
    guard.bindings = valid.then_some(bindings).unwrap_or_default();
    if !guard.effective_enabled() {
        // A replaced configuration must not leave old slots held.
        let held: Vec<u8> = guard.held.drain().collect();
        for mac in held {
            if let Some(binding) = guard.bindings.iter().find(|b| b.mac == mac) {
                enqueue_slot(&binding.code, "up");
            }
        }
    }
}

/// Shared hook state handed to webview/builders.
pub fn shared() -> SharedHotkeys {
    SHARED.get_or_init(|| Arc::new(Mutex::new(HotkeyState::default()))).clone()
}

static SHARED: OnceLock<SharedHotkeys> = OnceLock::new();
