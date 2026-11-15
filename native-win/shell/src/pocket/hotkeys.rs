// Pocket shortcut engine: one WH_KEYBOARD_LL hook covering the hold-prefix
// matcher (port of PocketSlotMatcher, Windows-simplified), the shortcut
// recorder routing, and the panel-preview session. Events not part of the
// recorded chord are never swallowed, and injected strokes (our own
// SendInput, HIDBridge) are filtered by LLKHF_INJECTED so the hook can never
// trigger on itself or on the other bridges' injections.

use std::collections::HashSet;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use super::keys::{PocketConfig, RecordKind, RecordOutcome, Recorder};
use super::Op;

const LLKHF_INJECTED: u32 = 0x10;
const VK_ESCAPE: u16 = 0x1B;

fn is_modifier_vk(vk: u16) -> bool {
    matches!(vk, 0x10 | 0xA0 | 0xA1 | 0x11 | 0xA2 | 0xA3 | 0x12 | 0xA4 | 0xA5 | 0x5B | 0x5C)
}

/// Hook-facing runtime state. The pocket worker mutates config/recording;
/// the hook thread owns the chord state.
pub struct PocketRuntime {
    pub config: PocketConfig,
    pub down: HashSet<u16>,
    pub claimed: HashSet<u16>,
    /// Index of the currently held prefix (0 = use, 1 = capture).
    pub active: Option<usize>,
    /// Live recording session (recorder id from the worker).
    pub recording: Option<Recorder>,
    pub recording_kind: Option<RecordKind>,
    pub recording_session: Option<String>,
    /// True from the first hook event of the recording session: everything
    /// after that is swallowed so the chord never types into an application.
    pub recording_started: bool,
    pub internal_drag: bool,
    pub dialog_open: bool,
    pub panel_visible: bool,
    pub hook_active: bool,
}

impl PocketRuntime {
    pub fn new() -> PocketRuntime {
        PocketRuntime {
            config: PocketConfig::default(),
            down: HashSet::new(),
            claimed: HashSet::new(),
            active: None,
            recording: None,
            recording_kind: None,
            recording_session: None,
            recording_started: false,
            internal_drag: false,
            dialog_open: false,
            panel_visible: false,
            hook_active: false,
        }
    }

    fn allowed(&self) -> bool {
        self.config.enabled && self.hook_active && !self.internal_drag && !self.dialog_open && self.recording.is_none()
    }

    /// Currently held observable modifiers (the hook callback reads the real
    /// keyboard state; tests pass synthetic sets instead).
    pub fn modifiers_held() -> HashSet<u16> {
        let pressed = |vk: i32| unsafe { GetAsyncKeyState(vk) as u16 & 0x8000 != 0 };
        let mut held = HashSet::new();
        if pressed(0x10) {
            held.insert(0x10);
        }
        if pressed(0x11) {
            held.insert(0x11);
        }
        if pressed(0x12) {
            held.insert(0x12);
        }
        if pressed(0x5B) || pressed(0x5C) {
            held.insert(0x5B);
        }
        held
    }
}

pub type SharedPocket = Arc<Mutex<PocketRuntime>>;

/// Decisions the hook hands to the pocket worker thread.
#[derive(Debug)]
pub enum HookDecision {
    Action { action: &'static str, index: usize },
    Preview { show: bool, capture_mode: bool },
    /// Recorder finished; the session id comes from the runtime.
    Recorded { session: String, kind: RecordKind, outcome: RecordedOutcome },
}

#[derive(Debug)]
pub enum RecordedOutcome {
    Chord { modifiers: Vec<String>, ordinary: Option<u8> },
    Cancelled,
    Failed(String),
}

fn recorded_outcome(outcome: &RecordOutcome) -> RecordedOutcome {
    match outcome {
        RecordOutcome::Chord(chord) => RecordedOutcome::Chord {
            modifiers: chord.modifiers.clone(),
            ordinary: chord.ordinary,
        },
        RecordOutcome::Cancelled => RecordedOutcome::Cancelled,
        RecordOutcome::Failed(message) => RecordedOutcome::Failed(message.clone()),
        RecordOutcome::Pending => RecordedOutcome::Failed("pending".into()),
    }
}

/// Pure matcher logic, factored out for unit tests: returns (swallow, decision).
/// `held` is the set of currently pressed observable modifier VKs. `fresh`
/// mirrors the low-level hook's lack of an autorepeat flag.
pub fn handle_key_event(
    runtime: &mut PocketRuntime,
    vk: u16,
    down: bool,
    fresh: bool,
    held: &HashSet<u16>,
) -> (bool, Option<HookDecision>) {
    // The recorder takes over completely while a recording session is live.
    if runtime.recording.is_some() {
        let claimed = runtime.claimed.contains(&vk);
        let outcome = if down || claimed || is_modifier_vk(vk) {
            runtime.recording.as_mut().unwrap().observe(vk, down)
        } else {
            RecordOutcome::Pending
        };
        if down {
            runtime.claimed.insert(vk);
        } else {
            runtime.claimed.remove(&vk);
        }
        runtime.recording_started = true;
        let swallow = claimed || down || is_modifier_vk(vk) || runtime.claimed.contains(&vk);
        return match outcome {
            RecordOutcome::Pending => (swallow, None),
            RecordOutcome::Chord(_) | RecordOutcome::Cancelled | RecordOutcome::Failed(_) => {
                runtime.recording = None;
                runtime.recording_started = false;
                let kind = runtime.recording_kind.take().unwrap_or(RecordKind::Prefix);
                let session = runtime.recording_session.take().unwrap_or_default();
                runtime.claimed.clear();
                (true, Some(HookDecision::Recorded { session, kind, outcome: recorded_outcome(&outcome) }))
            }
        };
    }
    if !runtime.allowed() {
        // While disallowed, still drain claimed keys so nothing sticks.
        if down {
            runtime.claimed.remove(&vk);
            return (false, None);
        }
        return (runtime.claimed.remove(&vk), None);
    }
    if !down {
        let mut swallow = runtime.claimed.remove(&vk);
        if let Some(index) = runtime.active {
            let prefix = runtime.config.prefixes[index].clone();
            let released_own = prefix.trigger == Some(vk)
                || (prefix.trigger.is_none() && prefix.modifiers.contains(&vk));
            if released_own || !prefix.trigger_held(&runtime.down) {
                runtime.active = None;
                swallow = true;
                return (swallow, Some(HookDecision::Preview { show: false, capture_mode: false }));
            }
        }
        return (swallow, None);
    }
    if runtime.claimed.contains(&vk) {
        return (true, None); // repeats of a claimed chord key
    }
    // A held prefix selects the slot action for its index.
    if let Some(slot) = runtime.config.slot_vks.iter().position(|slot| *slot == Some(vk)) {
        let candidate = runtime.active.or_else(|| {
            runtime
                .config
                .prefixes
                .iter()
                .position(|prefix| prefix.trigger.is_none() && prefix.modifiers_match(held))
        });
        if let Some(prefix_index) = candidate {
            let prefix = runtime.config.prefixes[prefix_index].clone();
            if prefix.trigger_held(&runtime.down) {
                runtime.claimed.insert(vk);
                runtime.active = None;
                let action = prefix.action;
                return (true, Some(HookDecision::Action { action, index: slot }));
            }
        }
        // Slot key without a prefix passes through untouched.
        return (false, None);
    }
    // Trigger-key prefixes claim their trigger key.
    for (index, prefix) in runtime.config.prefixes.iter().enumerate() {
        if prefix.trigger == Some(vk) && prefix.modifiers_match(held) {
            runtime.claimed.insert(vk);
            if fresh {
                runtime.active = Some(index);
                let capture_mode = prefix.action == "capture";
                return (true, Some(HookDecision::Preview { show: true, capture_mode }));
            }
            return (true, None);
        }
    }
    // Modifier-only prefixes activate without swallowing the modifier.
    if is_modifier_vk(vk) && held.contains(&vk) {
        for (index, prefix) in runtime.config.prefixes.iter().enumerate() {
            if prefix.trigger.is_none() && prefix.modifiers_match(held) {
                if fresh {
                    runtime.active = Some(index);
                    let capture_mode = prefix.action == "capture";
                    return (false, Some(HookDecision::Preview { show: true, capture_mode }));
                }
                return (false, None);
            }
        }
    }
    if runtime.active.is_some() {
        runtime.active = None;
        // Esc while a preview is open is consumed (Mac keyCode 53 handling).
        return (vk == VK_ESCAPE, Some(HookDecision::Preview { show: false, capture_mode: false }));
    }
    (false, None)
}

thread_local! {
    static CONTEXT: std::cell::RefCell<Option<HookContext>> = const { std::cell::RefCell::new(None) };
}

struct HookContext {
    state: SharedPocket,
    decisions: Sender<Op>,
}

unsafe extern "system" fn low_level_callback(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
    }
    let info = &*(lparam as *const KBDLLHOOKSTRUCT);
    // Our own SendInput strokes and HIDBridge injections never trigger actions.
    if info.flags & LLKHF_INJECTED != 0 {
        return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
    }
    let down = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
    let up = matches!(wparam as u32, WM_KEYUP | WM_SYSKEYUP);
    if !down && !up {
        return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
    }
    let vk = info.vkCode as u16;
    let mut swallow = false;
    CONTEXT.with(|cell| {
        let Ok(mut context) = cell.try_borrow_mut() else { return };
        let Some(context) = context.as_mut() else { return };
        let Ok(mut runtime) = context.state.try_lock() else { return };
        let held = PocketRuntime::modifiers_held();
        let fresh = if down { runtime.down.insert(vk) } else { runtime.down.remove(&vk) };
        let (swallowed, decision) = handle_key_event(&mut runtime, vk, down, fresh, &held);
        swallow = swallowed;
        if let Some(decision) = decision {
            let op = match decision {
                HookDecision::Action { action, index } => Op::HookAction { action, index },
                HookDecision::Preview { show, capture_mode } => Op::Preview { show, capture_mode },
                HookDecision::Recorded { session, kind, outcome } => Op::Recorded { session, kind, outcome },
            };
            let _ = context.decisions.send(op);
        }
    });
    if swallow {
        1
    } else {
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    }
}

pub struct PocketHook {
    state: SharedPocket,
    thread: Option<HookHandle>,
}

struct HookHandle {
    thread_id: u32,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl PocketHook {
    pub fn new(state: SharedPocket) -> PocketHook {
        PocketHook { state, thread: None }
    }

    pub fn start(&mut self, decisions: Sender<Op>) {
        {
            let mut runtime = self.state.lock().unwrap();
            runtime.hook_active = true;
            runtime.down.clear();
            runtime.claimed.clear();
        }
        if self.thread.is_none() {
            let state = self.state.clone();
            let (ready_sender, ready_receiver) = std::sync::mpsc::channel::<Option<u32>>();
            let handle = std::thread::spawn(move || unsafe {
                CONTEXT.with(|cell| {
                    *cell.borrow_mut() = Some(HookContext { state: state.clone(), decisions });
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
            });
            match ready_receiver.recv() {
                Ok(Some(thread_id)) => self.thread = Some(HookHandle { thread_id, handle: Some(handle) }),
                _ => {
                    let _ = handle.join();
                }
            }
        }
    }

    pub fn stop(&mut self) {
        {
            let mut runtime = self.state.lock().unwrap();
            runtime.hook_active = false;
            runtime.active = None;
            runtime.recording = None;
            runtime.recording_kind = None;
            runtime.recording_session = None;
            runtime.recording_started = false;
            runtime.claimed.clear();
            runtime.down.clear();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pocket::keys::PocketSettings;

    fn runtime_with(mut settings: PocketSettings) -> PocketRuntime {
        settings.enabled = true;
        let mut runtime = PocketRuntime::new();
        runtime.config = PocketConfig::from_settings(&settings);
        runtime.hook_active = true;
        runtime
    }

    fn control_prefix_settings() -> PocketSettings {
        let mut settings = PocketSettings::default();
        settings.use_modifiers = vec!["control".into()];
        settings.capture_modifiers = vec!["shift".into(), "control".into()];
        settings.slot_key_codes = vec![18, 19, 20, 21, 23, 22, 26, 28, 25];
        settings
    }

    fn press(runtime: &mut PocketRuntime, vk: u16, held: &[u16]) -> (bool, Option<HookDecision>) {
        let fresh = runtime.down.insert(vk);
        handle_key_event(runtime, vk, true, fresh, &held.iter().copied().collect())
    }

    fn release(runtime: &mut PocketRuntime, vk: u16, held: &[u16]) -> (bool, Option<HookDecision>) {
        runtime.down.remove(&vk);
        handle_key_event(runtime, vk, false, true, &held.iter().copied().collect())
    }

    #[test]
    fn trigger_prefix_activates_and_claims() {
        let mut settings = control_prefix_settings();
        settings.use_trigger = Some(0); // "A" (not a slot key: validation forbids overlap)
        let mut runtime = runtime_with(settings);
        let (ctrl_swallow, ctrl_decision) = press(&mut runtime, 0x11, &[0x11]);
        assert!(!ctrl_swallow, "modifiers are never swallowed");
        assert!(ctrl_decision.is_none());
        let (swallow, decision) = press(&mut runtime, 0x41, &[0x11]);
        assert!(swallow, "trigger key must be claimed");
        match decision {
            Some(HookDecision::Preview { show: true, capture_mode }) => assert!(!capture_mode),
            other => panic!("expected preview, got {other:?}"),
        }
        let (up_swallow, up_decision) = release(&mut runtime, 0x41, &[0x11]);
        assert!(up_swallow);
        assert!(matches!(up_decision, Some(HookDecision::Preview { show: false, .. })));
    }

    #[test]
    fn modifier_only_prefix_activates_on_modifier() {
        let mut runtime = runtime_with(control_prefix_settings());
        let (swallow, decision) = press(&mut runtime, 0x11, &[0x11]);
        assert!(!swallow);
        match decision {
            Some(HookDecision::Preview { show: true, capture_mode }) => assert!(!capture_mode),
            other => panic!("expected preview, got {other:?}"),
        }
    }

    #[test]
    fn prefix_then_slot_key_dispatches_action() {
        let mut runtime = runtime_with(control_prefix_settings());
        press(&mut runtime, 0x11, &[0x11]);
        press(&mut runtime, 0x10, &[0x11, 0x10]); // Shift completes capture prefix
        let (swallow, decision) = press(&mut runtime, 0x31, &[0x11, 0x10]);
        assert!(swallow, "slot key claimed");
        match decision {
            Some(HookDecision::Action { action, index }) => {
                assert_eq!(action, "capture");
                assert_eq!(index, 0);
            }
            other => panic!("expected action, got {other:?}"),
        }
    }

    #[test]
    fn use_prefix_pastes_matching_slot() {
        let mut runtime = runtime_with(control_prefix_settings());
        press(&mut runtime, 0x11, &[0x11]);
        let (swallow, decision) = press(&mut runtime, 0x32, &[0x11]);
        assert!(swallow);
        match decision {
            Some(HookDecision::Action { action, index }) => {
                assert_eq!(action, "use");
                assert_eq!(index, 1);
            }
            other => panic!("expected action, got {other:?}"),
        }
    }

    #[test]
    fn other_key_cancels_preview_and_esc_swallowed() {
        let mut runtime = runtime_with(control_prefix_settings());
        press(&mut runtime, 0x11, &[0x11]);
        let (swallow, decision) = press(&mut runtime, 0x41, &[0x11]);
        assert!(!swallow);
        assert!(matches!(decision, Some(HookDecision::Preview { show: false, .. })));
        // The Ctrl release resets the chord; a fresh press re-activates.
        release(&mut runtime, 0x11, &[]);
        press(&mut runtime, 0x11, &[0x11]);
        let (esc_swallow, esc_decision) = press(&mut runtime, VK_ESCAPE, &[0x11]);
        assert!(esc_swallow);
        assert!(matches!(esc_decision, Some(HookDecision::Preview { show: false, .. })));
    }

    #[test]
    fn disabled_runtime_never_claims() {
        let mut settings = control_prefix_settings();
        settings.enabled = false;
        let mut runtime = runtime_with(settings);
        let (swallow, decision) = press(&mut runtime, 0x31, &[]);
        assert!(!swallow);
        assert!(decision.is_none());
    }

    #[test]
    fn slot_key_without_prefix_passes_through() {
        let mut runtime = runtime_with(control_prefix_settings());
        let (swallow, decision) = press(&mut runtime, 0x31, &[]);
        assert!(!swallow);
        assert!(decision.is_none());
    }

    #[test]
    fn fn_only_prefix_never_activates() {
        let settings = PocketSettings::default(); // both prefixes contain fn
        let mut runtime = runtime_with(settings);
        let (swallow, decision) = press(&mut runtime, 0x31, &[]);
        assert!(!swallow);
        assert!(decision.is_none());
    }

    #[test]
    fn recorder_session_swallows_chord_and_reports() {
        let mut runtime = runtime_with(control_prefix_settings());
        runtime.config.enabled = false;
        runtime.recording = Some(Recorder::new(RecordKind::Prefix));
        runtime.recording_kind = Some(RecordKind::Prefix);
        runtime.recording_session = Some("session-1".into());
        let (down_swallow, _) = press(&mut runtime, 0x11, &[0x11]);
        assert!(down_swallow);
        let (_, _) = press(&mut runtime, 0x43, &[0x11]);
        let (_, _) = release(&mut runtime, 0x11, &[]);
        let (up_swallow, decision) = release(&mut runtime, 0x43, &[]);
        assert!(up_swallow);
        match decision {
            Some(HookDecision::Recorded { session, kind, outcome }) => {
                assert_eq!(session, "session-1");
                assert_eq!(kind, RecordKind::Prefix);
                match outcome {
                    RecordedOutcome::Chord { modifiers, ordinary } => {
                        assert_eq!(modifiers, vec!["control"]);
                        assert_eq!(ordinary, Some(8)); // Carbon C
                    }
                    other => panic!("expected chord, got {other:?}"),
                }
            }
            other => panic!("expected recorded, got {other:?}"),
        }
        assert!(runtime.recording.is_none());
    }

    #[test]
    fn slot_recording_fails_on_modifier() {
        let mut runtime = runtime_with(control_prefix_settings());
        runtime.recording = Some(Recorder::new(RecordKind::Slot));
        runtime.recording_kind = Some(RecordKind::Slot);
        runtime.recording_session = Some("session-2".into());
        // Any modifier immediately fails a slot recording (Mac slotNeedsOrdinary).
        let (swallow, decision) = press(&mut runtime, 0x11, &[0x11]);
        assert!(swallow);
        match decision {
            Some(HookDecision::Recorded { outcome, .. }) => {
                assert!(matches!(outcome, RecordedOutcome::Failed(_)));
            }
            other => panic!("expected recorded failure, got {other:?}"),
        }
        assert!(runtime.recording.is_none());
    }

    #[test]
    fn claimed_key_drains_while_disallowed() {
        let mut runtime = runtime_with(control_prefix_settings());
        press(&mut runtime, 0x11, &[0x11]);
        press(&mut runtime, 0x31, &[0x11]); // claimed
        runtime.config.enabled = false;
        let (up_swallow, _) = release(&mut runtime, 0x31, &[]);
        assert!(up_swallow, "claimed releases still drain when disabled");
        let (down_swallow, _) = press(&mut runtime, 0x32, &[]);
        assert!(!down_swallow, "fresh keys pass through when disabled");
    }

    #[test]
    fn internal_drag_suspends_actions() {
        let mut runtime = runtime_with(control_prefix_settings());
        runtime.internal_drag = true;
        let (swallow, decision) = press(&mut runtime, 0x11, &[0x11]);
        assert!(!swallow);
        assert!(decision.is_none());
    }

    #[test]
    fn preview_hides_when_prefix_released() {
        let mut runtime = runtime_with(control_prefix_settings());
        press(&mut runtime, 0x11, &[0x11]);
        let (_, decision) = release(&mut runtime, 0x11, &[]);
        assert!(matches!(decision, Some(HookDecision::Preview { show: false, .. })));
        assert!(runtime.active.is_none());
    }

    #[test]
    fn recorder_esc_cancels_and_swallows() {
        let mut runtime = runtime_with(control_prefix_settings());
        runtime.recording = Some(Recorder::new(RecordKind::Prefix));
        runtime.recording_kind = Some(RecordKind::Prefix);
        let (swallow, decision) = press(&mut runtime, VK_ESCAPE, &[]);
        assert!(swallow);
        match decision {
            Some(HookDecision::Recorded { outcome, .. }) => {
                assert!(matches!(outcome, RecordedOutcome::Cancelled));
            }
            other => panic!("expected recorded cancel, got {other:?}"),
        }
    }
}
