// Drag trigger detection (Windows port of DragTrigger + DragFileWatcher):
// one WH_KEYBOARD_LL hook watches for the configured trigger key pressed
// while a real file drag is in progress (either mouse button held) and posts
// menu show/hide messages to the wheel window's thread. The hook never
// swallows keystrokes — normal typing is untouched, and injected strokes
// (our own SendInput, HIDBridge) are filtered so the hook can never trigger
// itself.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE, VK_LBUTTON, VK_RBUTTON};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use keymap::vk_from_mac;

use super::menu::{WM_DRAG_HIDE, WM_DRAG_SHOW};

const LLKHF_INJECTED: u32 = 0x10;

pub const TRIGGER_KEYS_JSON: &str = include_str!("../../../../shared/drag-trigger-keys.json");

/// The shared trigger-key table (values are Mac key codes behind "key:N" or
/// modifier names), also served to the settings UI as triggerLabels.
pub fn trigger_key_table() -> &'static Vec<TriggerKey> {
    static TABLE: OnceLock<Vec<TriggerKey>> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str::<Vec<TriggerKey>>(TRIGGER_KEYS_JSON).expect("shared/drag-trigger-keys.json parses")
    })
}

#[derive(Clone, Debug, serde::Deserialize)]
#[allow(dead_code)] // zh/group mirror the shared table consumed by the UI
pub struct TriggerKey {
    pub value: String,
    pub zh: String,
    pub en: String,
    pub group: String,
}

pub fn trigger_labels() -> std::collections::BTreeMap<String, String> {
    trigger_key_table()
        .iter()
        .map(|key| (key.value.clone(), key.en.clone()))
        .collect()
}

/// Resolves a trigger preference to the Windows virtual key it maps to.
/// `shift`/`option`/`control` map to their modifiers (the Win key stands in
/// for Command); `key:N` goes through the keymap crate's Mac→VK table.
pub fn trigger_vk(value: &str) -> Option<u16> {
    match value {
        "shift" => Some(0x10),
        "option" => Some(0x12), // Alt
        "control" => Some(0x11),
        "command" => Some(0x5B), // Win
        _ => {
            let code = value.strip_prefix("key:")?;
            vk_from_mac(code.parse::<u8>().ok()?)
        }
    }
}

pub fn is_valid_trigger(value: &str) -> bool {
    trigger_key_table().iter().any(|key| key.value == value) && trigger_vk(value).is_some()
}

/// True while the physical key is held (timer poll for the wheel window).
pub fn vk_held(vk: u16) -> bool {
    unsafe { GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 }
}

fn mouse_drag_held() -> bool {
    vk_held(VK_LBUTTON) || vk_held(VK_RBUTTON)
}

/// Hook-facing runtime: the current trigger config plus whether the wheel is
/// visible (owned by the hook thread; the wheel timer only reads it).
pub struct DragHookRuntime {
    pub enabled: bool,
    pub vk: Option<u16>,
    pub menu_visible: bool,
    pub menu_thread_id: Arc<AtomicU32>,
}

impl DragHookRuntime {
    pub fn new() -> Self {
        DragHookRuntime { enabled: false, vk: None, menu_visible: false, menu_thread_id: Arc::new(AtomicU32::new(0)) }
    }
}

pub type SharedDragRuntime = Arc<Mutex<DragHookRuntime>>;

/// Pure hook matcher, factored out for unit tests: a trigger press while a
/// mouse button is held opens the wheel; releasing the trigger (or pressing
/// Esc while the wheel is open) closes it. Everything else passes through.
pub fn handle_key_event(runtime: &mut DragHookRuntime, vk: u16, down: bool, mouse_held: bool) -> Option<MenuMessage> {
    if down && vk == VK_ESCAPE && runtime.menu_visible {
        runtime.menu_visible = false;
        return Some(MenuMessage::Hide);
    }
    if runtime.vk != Some(vk) {
        return None;
    }
    if down {
        if mouse_held && runtime.enabled && !runtime.menu_visible {
            runtime.menu_visible = true;
            return Some(MenuMessage::Show);
        }
    } else if runtime.menu_visible {
        runtime.menu_visible = false;
        return Some(MenuMessage::Hide);
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuMessage {
    Show,
    Hide,
}

thread_local! {
    static CONTEXT: std::cell::RefCell<Option<HookContext>> = const { std::cell::RefCell::new(None) };
}

struct HookContext {
    runtime: SharedDragRuntime,
}

unsafe extern "system" fn low_level_callback(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
    }
    let info = &*(lparam as *const KBDLLHOOKSTRUCT);
    if info.flags & LLKHF_INJECTED != 0 {
        return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
    }
    let down = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
    let up = matches!(wparam as u32, WM_KEYUP | WM_SYSKEYUP);
    if !down && !up {
        return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
    }
    CONTEXT.with(|cell| {
        let Ok(context) = cell.try_borrow() else { return };
        let Some(context) = context.as_ref() else { return };
        let Ok(mut runtime) = context.runtime.try_lock() else { return };
        if let Some(message) = handle_key_event(&mut runtime, info.vkCode as u16, down, mouse_drag_held()) {
            let thread_id = runtime.menu_thread_id.load(Ordering::SeqCst);
            if thread_id != 0 {
                let message = match message {
                    MenuMessage::Show => WM_DRAG_SHOW,
                    MenuMessage::Hide => WM_DRAG_HIDE,
                };
                unsafe {
                    PostThreadMessageW(thread_id, message, 0, 0);
                }
            }
        }
    });
    // Never swallow: the trigger key must not disturb an active drag.
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

/// Independent low-level keyboard hook for Drag Convert. Lives next to the
/// pocket hook; both stay passive and never intercept typing.
pub struct DragHook {
    runtime: SharedDragRuntime,
    thread: Option<HookHandle>,
}

struct HookHandle {
    thread_id: u32,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl DragHook {
    pub fn new(runtime: SharedDragRuntime) -> DragHook {
        DragHook { runtime, thread: None }
    }

    pub fn start(&mut self, core: &super::SharedCore) {
        if self.thread.is_some() {
            return;
        }
        {
            let guard = core.lock().unwrap();
            let mut runtime = self.runtime.lock().unwrap();
            runtime.vk = trigger_vk(&guard.settings.trigger);
            runtime.enabled = guard.settings.enabled;
        }
        let runtime = self.runtime.clone();
        let (ready_sender, ready_receiver) = std::sync::mpsc::channel::<Option<u32>>();
        let handle = std::thread::Builder::new()
            .name("dragconvert-hook".into())
            .spawn(move || unsafe {
                CONTEXT.with(|cell| {
                    *cell.borrow_mut() = Some(HookContext { runtime: runtime.clone() });
                });
                let thread_id = GetCurrentThreadId();
                let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_callback), std::ptr::null_mut(), 0);
                if hook.is_null() {
                    let _ = ready_sender.send(None);
                    return;
                }
                let _ = ready_sender.send(Some(thread_id));
                let mut message: MSG = std::mem::zeroed();
                while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {}
                UnhookWindowsHookEx(hook);
            })
            .expect("dragconvert hook thread");
        match ready_receiver.recv() {
            Ok(Some(thread_id)) => self.thread = Some(HookHandle { thread_id, handle: Some(handle) }),
            _ => {
                let _ = handle.join();
            }
        }
    }

    pub fn stop(&mut self) {
        if let Ok(mut runtime) = self.runtime.try_lock() {
            runtime.menu_visible = false;
            runtime.enabled = false;
        }
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
