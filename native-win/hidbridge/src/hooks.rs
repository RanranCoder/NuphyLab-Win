// WH_KEYBOARD_LL listeners and SendInput shortcut injection, porting KeyHooks
// and TriggerHooks from native/HIDBridge.swift. Each monitor owns a dedicated
// thread with a message pump (a low-level hook delivers on its installer
// thread, which must pump messages). Validation is split into pure helpers so
// the self-test can cover every branch without installing hooks.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;

use serde_json::{json, Value};
use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Threading::{
    GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_CONTROL,
    VK_MENU, VK_SHIFT,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, EnumWindows, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId,
    IsWindowVisible, PostThreadMessageW, SetForegroundWindow, SetWindowsHookExW,
    UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT,
    WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::device::{diagnostic, emit};
use crate::error::{failure, Result};
use keymap;

pub const MOD_CONTROL: u8 = 1;
pub const MOD_SHIFT: u8 = 2;
pub const MOD_OPTION: u8 = 4;
pub const MOD_COMMAND: u8 = 8;
pub const MOD_FN: u8 = 16;
const CTRL_OPTION: u8 = MOD_CONTROL | MOD_OPTION;
const LLKHF_INJECTED: u32 = 0x10;
const VK_LWIN: i32 = 0x5B;
const VK_RWIN: i32 = 0x5C;

/// Only the agreed Ctrl+Option+1..6 and Ctrl+Option+N combinations; slot index
/// order matches the macOS implementation's [18,19,20,21,23,22].
pub const SLOT_CODES: [u8; 6] = [18, 19, 20, 21, 23, 22];
pub const NEW_TASK_CODE: u8 = 45;
pub const ALLOWED_MONITOR_CODES: [u8; 7] = [18, 19, 20, 21, 23, 22, 45];

/// Keep this allowlist aligned with shared/trigger-keys.json (the Carbon
/// keyCodes it lists). Modifier keys, Fn and the encoder are excluded.
pub const ALLOWED_TRIGGER_CODES: &[u8] = &[
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
    25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44,
    45, 46, 47, 48, 49, 50, 51, 53, 96, 97, 98, 99, 100, 101, 103, 109, 111,
    114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126,
];

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Parses a modifier name list the way KeyHooks.flags does. Unknown names or a
/// non-array value are rejected; `names: None` means an empty list.
pub fn parse_flags(names: Option<&Value>) -> Result<u8> {
    let Some(value) = names else { return Ok(0) };
    let invalid = || failure("INVALID_MODIFIERS", "modifiers must be an array of command, shift, option, control or fn.");
    let Some(list) = value.as_array() else { return Err(invalid()) };
    let mut flags = 0u8;
    for name in list {
        let Some(name) = name.as_str() else { return Err(invalid()) };
        let bit = match name.to_ascii_lowercase().as_str() {
            "command" | "cmd" => MOD_COMMAND,
            "shift" => MOD_SHIFT,
            "option" | "alt" => MOD_OPTION,
            "control" | "ctrl" => MOD_CONTROL,
            "fn" => MOD_FN,
            other => return Err(failure("INVALID_MODIFIERS", format!("Unknown modifier: {other}"))),
        };
        flags |= bit;
    }
    Ok(flags)
}

// ---------------------------------------------------------------------------
// keyMonitor (KeyHooks port)

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BindingAction {
    Slot(usize),
    NewTask,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyBinding {
    pub mac: u8,
    pub vk: u16,
    pub suppress: bool,
    pub action: BindingAction,
}

/// Pure validation. Ok(None) means "disabled". Mirrors KeyHooks.configure.
pub fn validate_key_monitor(command: &Value) -> Result<Option<Vec<KeyBinding>>> {
    if command.get("enabled").and_then(Value::as_bool) == Some(false) {
        return Ok(None);
    }
    let default_bindings: Vec<Value> = SLOT_CODES
        .iter()
        .enumerate()
        .map(|(index, code)| json!({"name": format!("slot-{index}"), "keyCode": code, "modifiers": ["control", "option"], "suppress": false}))
        .chain(std::iter::once(json!({"name": "newTask", "keyCode": NEW_TASK_CODE, "modifiers": ["control", "option"], "suppress": false})))
        .collect();
    let raw: &Vec<Value> = match command.get("bindings") {
        None => &default_bindings,
        Some(list) => match list.as_array() {
            Some(items) if !items.is_empty() && items.len() <= 7 => items,
            _ => return Err(failure("INVALID_BINDINGS", "At most seven agreed Control+Option bindings are supported.")),
        },
    };
    let mut configured: Vec<KeyBinding> = Vec::new();
    for binding in raw {
        let invalid = || failure("INVALID_BINDING", "A binding requires name and a macOS keyCode for 1–6 or N.");
        let Some(code) = binding.get("keyCode").and_then(Value::as_i64) else {
            return Err(invalid());
        };
        if !(0..=127).contains(&code) || !ALLOWED_MONITOR_CODES.contains(&(code as u8)) {
            return Err(invalid());
        }
        let Some(name) = binding.get("name").and_then(Value::as_str).filter(|n| !n.is_empty() && n.len() <= 80) else {
            return Err(invalid());
        };
        let _ = name;
        let flags = parse_flags(binding.get("modifiers"))?;
        if flags != CTRL_OPTION {
            return Err(failure("INVALID_BINDING", "Global monitoring is limited to Control+Option+1–6 / N; bare typing keys are forbidden."));
        }
        let mac = code as u8;
        let Some(vk) = keymap::vk_from_mac(mac) else {
            return Err(invalid());
        };
        if configured.iter().any(|existing| existing.mac == mac) {
            return Err(failure("DUPLICATE_BINDING", "Two bindings have the same key and modifiers."));
        }
        let action = if mac == NEW_TASK_CODE {
            BindingAction::NewTask
        } else {
            BindingAction::Slot(SLOT_CODES.iter().position(|c| *c == mac).expect("slot code"))
        };
        configured.push(KeyBinding { mac, vk, suppress: binding.get("suppress").and_then(Value::as_bool).unwrap_or(false), action });
    }
    Ok(Some(configured))
}

// ---------------------------------------------------------------------------
// triggerMonitor (TriggerHooks port)

/// Pure validation. Ok(None) means "disabled". Mirrors TriggerHooks.requestedCode.
pub fn validate_trigger(command: &Value) -> Result<Option<u8>> {
    let Some(enabled) = command.get("enabled") else {
        return Err(failure("INVALID_TRIGGER_MONITOR", "triggerMonitor requires enabled: true or false."));
    };
    if !enabled.is_boolean() {
        return Err(failure("INVALID_TRIGGER_MONITOR", "triggerMonitor requires enabled: true or false."));
    }
    if enabled == &Value::Bool(false) {
        return Ok(None);
    }
    if let Some(code) = command.get("keyCode").and_then(Value::as_i64) {
        if (0..=127).contains(&code) && ALLOWED_TRIGGER_CODES.contains(&(code as u8)) {
            if command.get("suppress").is_some() || command.get("modifiers").is_some() {
                return Err(failure("INVALID_TRIGGER_MONITOR", "The lighting trigger observes one unmodified key and cannot suppress input."));
            }
            return Ok(Some(code as u8));
        }
    }
    Err(failure("INVALID_TRIGGER_KEY", "Choose one supported physical keyboard key for the lighting trigger."))
}

/// Pure predicate mirroring TriggerHooks.shouldTrigger: a bare, non-repeating
/// keyDown of exactly the configured key.
pub fn trigger_should_fire(down: bool, autorepeat: bool, modifiers: u8, configured: u8, mac: u8) -> bool {
    down && !autorepeat && modifiers == 0 && configured == mac
}

// ---------------------------------------------------------------------------
// Hook thread plumbing

enum Monitor {
    Keys { bindings: Vec<KeyBinding>, held: std::collections::HashSet<u16> },
    Trigger { configured: u8, vk: u16, held: bool },
}

thread_local! {
    static MONITOR: std::cell::RefCell<Option<Monitor>> = const { std::cell::RefCell::new(None) };
}

fn physical_modifiers() -> u8 {
    let pressed = |vk: i32| unsafe { GetAsyncKeyState(vk) as u16 & 0x8000 != 0 };
    let mut flags = 0;
    if pressed(VK_CONTROL as i32) {
        flags |= MOD_CONTROL;
    }
    if pressed(VK_SHIFT as i32) {
        flags |= MOD_SHIFT;
    }
    if pressed(VK_MENU as i32) {
        flags |= MOD_OPTION;
    }
    if pressed(VK_LWIN) || pressed(VK_RWIN) {
        flags |= MOD_COMMAND;
    }
    flags
}

unsafe extern "system" fn low_level_callback(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
    }
    let info = &*(lparam as *const KBDLLHOOKSTRUCT);
    // Own SendInput injections must never re-enter the monitors.
    if info.flags & LLKHF_INJECTED != 0 {
        return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
    }
    let (down, up) = match wparam as u32 {
        WM_KEYDOWN | WM_SYSKEYDOWN => (true, false),
        WM_KEYUP | WM_SYSKEYUP => (false, true),
        _ => return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam),
    };
    debug_assert!(down || up);
    let vk = info.vkCode as u16;
    let mut suppress = false;
    MONITOR.with(|cell| {
        let Ok(mut monitor) = cell.try_borrow_mut() else { return };
        match monitor.as_mut() {
            Some(Monitor::Keys { bindings, held }) => {
                let Some(binding) = bindings.iter().find(|b| b.vk == vk) else { return };
                let chord = physical_modifiers() == CTRL_OPTION;
                if down {
                    // The set doubles as Windows' missing autorepeat flag:
                    // a second down while held is a repeat and is ignored.
                    if chord && held.insert(vk) {
                        match binding.action {
                            BindingAction::Slot(index) => emit(json!({"event": "shortcut", "action": "slot", "index": index})),
                            BindingAction::NewTask => emit(json!({"event": "shortcut", "action": "newTask"})),
                        }
                        if binding.suppress {
                            suppress = true;
                        }
                    }
                } else {
                    // Always clear the held marker, even when the chord was
                    // already broken, so the next press is not read as repeat.
                    held.remove(&vk);
                    if chord && binding.suppress {
                        suppress = true;
                    }
                }
            }
            Some(Monitor::Trigger { configured, vk: target, held }) => {
                let Some(mac) = keymap::mac_from_vk(vk) else { return };
                if trigger_should_fire(down, down && *held, physical_modifiers(), *configured, mac) {
                    emit(json!({"event": "triggerKey", "keyCode": *configured}));
                }
                if down {
                    *held = true;
                } else if vk == *target {
                    *held = false;
                }
            }
            None => {}
        }
    });
    if suppress {
        1
    } else {
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    }
}

struct HookThread {
    thread_id: u32,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl HookThread {
    fn spawn(monitor: Monitor) -> Option<HookThread> {
        let (sender, receiver) = mpsc::channel::<Option<u32>>();
        let handle = std::thread::spawn(move || unsafe {
            MONITOR.with(|cell| *cell.borrow_mut() = Some(monitor));
            let thread_id = GetCurrentThreadId();
            let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_callback), std::ptr::null_mut(), 0);
            if hook.is_null() {
                let _ = sender.send(None);
                diagnostic("[hooks] SetWindowsHookExW failed");
                return;
            }
            let _ = sender.send(Some(thread_id));
            let mut message: MSG = std::mem::zeroed();
            // Returns 0 on WM_QUIT; negative on failure — both end the pump.
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {}
            UnhookWindowsHookEx(hook);
        });
        match receiver.recv() {
            Ok(Some(thread_id)) => Some(HookThread { thread_id, handle: Some(handle) }),
            _ => {
                let _ = handle.join();
                None
            }
        }
    }

    fn stop(&mut self) {
        unsafe {
            PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Persistent monitor state (ops are serialized on the main thread)

static KEY_MONITOR: Mutex<Option<HookThread>> = Mutex::new(None);
static TRIGGER_MONITOR: Mutex<Option<HookThread>> = Mutex::new(None);

pub fn configure_key_monitor(command: &Value) -> Result<Value> {
    let bindings = validate_key_monitor(command)?;
    let mut slot = KEY_MONITOR.lock().unwrap();
    if let Some(thread) = slot.as_mut() {
        thread.stop();
        *slot = None;
    }
    let Some(bindings) = bindings else { return Ok(json!({"enabled": false})) };
    let count = bindings.len();
    match HookThread::spawn(Monitor::Keys { bindings, held: std::collections::HashSet::new() }) {
        Some(thread) => {
            *slot = Some(thread);
            Ok(json!({"enabled": true, "bindingCount": count, "ordinaryTypingRecorded": false}))
        }
        None => Err(failure(
            "INPUT_MONITORING_REQUIRED",
            "Windows denied the key monitor (SetWindowsHookExW).",
        )),
    }
}

pub fn configure_trigger_monitor(command: &Value) -> Result<Value> {
    let configured = validate_trigger(command)?;
    let mut slot = TRIGGER_MONITOR.lock().unwrap();
    if let Some(thread) = slot.as_mut() {
        thread.stop();
        *slot = None;
    }
    let Some(mac) = configured else { return Ok(json!({"enabled": false, "ordinaryTypingRecorded": false})) };
    let Some(vk) = keymap::vk_from_mac(mac) else {
        return Err(failure("INVALID_TRIGGER_KEY", "Choose one supported physical keyboard key for the lighting trigger."));
    };
    match HookThread::spawn(Monitor::Trigger { configured: mac, vk, held: false }) {
        Some(thread) => {
            *slot = Some(thread);
            Ok(json!({"enabled": true, "ordinaryTypingRecorded": false}))
        }
        None => Err(failure(
            "INPUT_MONITORING_REQUIRED",
            "Windows denied the lighting trigger observer (SetWindowsHookExW).",
        )),
    }
}

// ---------------------------------------------------------------------------
// shortcut injection (KeyHooks.shortcut port)

struct HeldRelease {
    generation: u64,
    ups: Vec<INPUT>,
}

static HELD_SHORTCUT: Mutex<Option<HeldRelease>> = Mutex::new(None);
static SHORTCUT_GENERATION: AtomicU64 = AtomicU64::new(0);

fn key_input(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: windows_sys::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: if up { KEYEVENTF_KEYUP } else { 0 }, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn send(inputs: &[INPUT]) -> bool {
    unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) == inputs.len() as u32 }
}

fn release_shortcut_locked(held: &mut Option<HeldRelease>) -> bool {
    match held.take() {
        Some(release) => {
            SHORTCUT_GENERATION.fetch_add(1, Ordering::SeqCst);
            send(&release.ups)
        }
        None => false,
    }
}

pub fn release_shortcut() -> bool {
    let mut guard = HELD_SHORTCUT.lock().unwrap();
    release_shortcut_locked(&mut guard)
}

fn schedule_held_release(ups: Vec<INPUT>) {
    let generation = SHORTCUT_GENERATION.load(Ordering::SeqCst);
    *HELD_SHORTCUT.lock().unwrap() = Some(HeldRelease { generation, ups });
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(120));
        let mut guard = HELD_SHORTCUT.lock().unwrap();
        if let Some(held) = guard.as_ref() {
            if held.generation == generation {
                release_shortcut_locked(&mut guard);
            }
        }
    });
}

fn modifier_vk_list(names: Option<&Value>) -> Result<Vec<u16>> {
    let Some(value) = names else { return Ok(Vec::new()) };
    let invalid = || failure("INVALID_MODIFIERS", "modifiers must be an array of command, shift, option, control or fn.");
    let Some(list) = value.as_array() else { return Err(invalid()) };
    let mut result = Vec::new();
    for name in list {
        let Some(name) = name.as_str() else { return Err(invalid()) };
        match keymap::modifier_vks(name) {
            Some(Some(vk)) => result.push(vk),
            Some(None) => return Err(failure("INVALID_SHORTCUT", "The fn modifier cannot be injected on Windows.")),
            None => return Err(failure("INVALID_MODIFIERS", format!("Unknown modifier: {name}"))),
        }
    }
    Ok(result)
}

fn valid_thread_id(value: &str) -> bool {
    (1..=160).contains(&value.len())
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

struct CodexTarget {
    window: HWND,
    process_id: u32,
}

unsafe extern "system" fn enum_windows_proc(window: HWND, lparam: LPARAM) -> i32 {
    let mut pid = 0u32;
    GetWindowThreadProcessId(window, &mut pid);
    if pid == 0 {
        return 1;
    }
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if process.is_null() {
        return 1;
    }
    let mut buffer = [0u16; 512];
    let mut length = buffer.len() as u32;
    let mut matched = false;
    if QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut length) != 0 {
        let name = String::from_utf16_lossy(&buffer[..length as usize]).to_ascii_lowercase();
        if name.ends_with("\\codex.exe") {
            matched = true;
        }
    }
    CloseHandle(process);
    if matched && IsWindowVisible(window) != 0 {
        let slot = lparam as *mut Option<CodexTarget>;
        *slot = Some(CodexTarget { window, process_id: pid });
        return 0;
    }
    1
}

fn find_codex_window() -> Option<CodexTarget> {
    let mut found: Option<CodexTarget> = None;
    unsafe {
        EnumWindows(Some(enum_windows_proc), &mut found as *mut Option<CodexTarget> as LPARAM);
    }
    found
}

fn foreground_process_id() -> u32 {
    unsafe {
        let window = GetForegroundWindow();
        if window.is_null() {
            return 0;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(window, &mut pid);
        pid
    }
}

fn activate_codex() -> Result<()> {
    let Some(target) = find_codex_window() else {
        return Err(failure("TARGET_NOT_RUNNING", "Open Codex before sending a shortcut."));
    };
    unsafe {
        if foreground_process_id() != target.process_id {
            SetForegroundWindow(target.window);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while foreground_process_id() != target.process_id && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    if foreground_process_id() != target.process_id {
        return Err(failure("TARGET_NOT_FRONTMOST", "Codex did not become the foreground application; no shortcut was sent."));
    }
    Ok(())
}

pub fn shortcut(command: &Value) -> Result<Value> {
    let phase = command.get("phase").and_then(Value::as_str).unwrap_or("tap");
    if !["tap", "down", "up"].contains(&phase) {
        return Err(failure("INVALID_SHORTCUT", "Invalid shortcut phase."));
    }
    if phase == "up" {
        return Ok(json!({"released": release_shortcut()}));
    }
    if command.get("action").and_then(Value::as_str) == Some("task") {
        release_shortcut();
        let invalid_task = || failure("INVALID_TASK", "threadId must contain only letters, digits, underscores and hyphens.");
        let Some(thread_id) = command.get("threadId").and_then(Value::as_str) else {
            return Err(invalid_task());
        };
        if !valid_thread_id(thread_id) {
            return Err(invalid_task());
        }
        let url = format!("codex://threads/{thread_id}");
        let opened = unsafe {
            let verb = wide("open");
            let target = wide(&url);
            ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), target.as_ptr(), std::ptr::null(), std::ptr::null(), 1) as isize > 32
        };
        if !opened {
            return Err(failure("TASK_OPEN_FAILED", "Windows could not open the Codex task URL."));
        }
        return Ok(json!({"opened": true, "threadId": thread_id}));
    }
    let invalid_key = || failure("INVALID_SHORTCUT", "shortcut requires a macOS virtual keyCode from 0 to 127.");
    let Some(code) = command.get("keyCode").and_then(Value::as_i64) else {
        return Err(invalid_key());
    };
    if !(0..=127).contains(&code) {
        return Err(invalid_key());
    }
    let _ = parse_flags(command.get("modifiers"))?;
    let modifier_vks = modifier_vk_list(command.get("modifiers"))?;
    let target = command.get("target").and_then(Value::as_str).unwrap_or("com.openai.codex");
    if target != "com.openai.codex" {
        return Err(failure("INVALID_TARGET", "Shortcut delivery is limited to the Codex application."));
    }
    if command.get("activateTarget").and_then(Value::as_bool) == Some(true) {
        activate_codex()?;
    }
    let Some(vk) = keymap::vk_from_mac(code as u8) else {
        return Err(failure("INVALID_SHORTCUT", "This key cannot be injected on Windows."));
    };
    release_shortcut();

    let mut ups: Vec<INPUT> = vec![key_input(vk, true)];
    for modifier in modifier_vks.iter().rev() {
        ups.push(key_input(*modifier, true));
    }
    match command.get("delivery").and_then(Value::as_str) {
        Some("globalHold") | Some("globalTap") => (),
        Some(_) => return Err(failure("INVALID_SHORTCUT", "Invalid shortcut delivery.")),
        None => {
            let mut sequence: Vec<INPUT> = modifier_vks.iter().map(|vk| key_input(*vk, false)).collect();
            sequence.push(key_input(vk, false));
            if !send(&sequence) {
                return Err(failure("SHORTCUT_FAILED", "Windows rejected the shortcut events."));
            }
            if phase == "down" {
                schedule_held_release(ups);
            } else {
                send(&ups);
            }
            return Ok(json!({"sent": true, "keyCode": code, "phase": phase}));
        }
    }
    let delivery = command.get("delivery").and_then(Value::as_str).unwrap_or_default();
    if phase != if delivery == "globalHold" { "down" } else { "tap" } {
        return Err(failure("INVALID_SHORTCUT", "Invalid global dictation gesture."));
    }
    let modifier_only = command.get("modifierOnly").and_then(Value::as_bool) == Some(true);
    if modifier_only && modifier_vks.is_empty() {
        return Err(failure("INVALID_SHORTCUT", "A modifier hold requires at least one modifier."));
    }
    // modifierOnly holds the final modifier itself; other modifiers lead in.
    let (lead, key) = if modifier_only {
        (&modifier_vks[..modifier_vks.len() - 1], modifier_vks[modifier_vks.len() - 1])
    } else {
        (&modifier_vks[..], vk)
    };
    let held_now = |vk: u16| unsafe { GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 };
    let mut downs: Vec<INPUT> = Vec::new();
    // Release any chord key already physically held (ShortcutChord parity)
    // before pressing, so the release sequence cannot strand a stale state.
    for vk in lead.iter().chain(std::iter::once(&key)) {
        if held_now(*vk) {
            downs.push(key_input(*vk, true));
        }
    }
    for modifier in lead {
        downs.push(key_input(*modifier, false));
    }
    downs.push(key_input(key, false));
    if !send(&downs) {
        return Err(failure("SHORTCUT_FAILED", "Windows rejected the dictation gesture."));
    }
    if delivery == "globalHold" {
        schedule_held_release(ups);
    } else {
        send(&ups);
    }
    Ok(json!({"sent": true, "keyCode": code, "phase": phase, "delivery": delivery}))
}

/// Stops both monitors and releases any held shortcut. Called from the
/// shutdown path so no hook outlives the process's last response.
pub fn stop_all() {
    release_shortcut();
    if let Some(mut thread) = KEY_MONITOR.lock().unwrap().take() {
        thread.stop();
    }
    if let Some(mut thread) = TRIGGER_MONITOR.lock().unwrap().take() {
        thread.stop();
    }
}
