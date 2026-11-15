// Smart Selection shortcut engine: one independent WH_KEYBOARD_LL hook that
// coexists with the pocket / dragconvert / typing / codex hooks (each module
// owns its own hook; injected strokes — our own SendInput and HIDBridge — are
// filtered by LLKHF_INJECTED so no bridge can trigger another). Keys pass
// through untouched except inside selection mode (where everything is
// swallowed so a stray chord never types into an application) and while the
// shortcut recorder session is live.

use std::collections::HashSet;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::pocket::hotkeys::RecordedOutcome;
use crate::pocket::keys::Recorder;

use super::Op;

const LLKHF_INJECTED: u32 = 0x10;
const VK_ESCAPE: u16 = 0x1B;

pub fn is_modifier_vk(vk: u16) -> bool {
    matches!(vk, 0x10 | 0xA0 | 0xA1 | 0x11 | 0xA2 | 0xA3 | 0x12 | 0xA4 | 0xA5 | 0x5B | 0x5C)
}

/// Hook-facing runtime state. The worker mutates config/recording; the hook
/// thread owns the chord state.
pub struct SelectionRuntime {
    pub enabled: bool,
    pub shortcut_vks: Vec<u16>,
    pub recording: Option<Recorder>,
    pub recording_session: Option<String>,
    /// True from the first hook event of a recording session: everything after
    /// that is swallowed so the chord never types into an application.
    pub recording_started: bool,
    pub down: HashSet<u16>,
    pub claimed: HashSet<u16>,
    /// True while the selection overlay owns the screen (selection mode).
    pub selecting: bool,
    pub hook_active: bool,
}

impl SelectionRuntime {
    pub fn new() -> SelectionRuntime {
        SelectionRuntime {
            enabled: false,
            shortcut_vks: Vec::new(),
            recording: None,
            recording_session: None,
            recording_started: false,
            down: HashSet::new(),
            claimed: HashSet::new(),
            selecting: false,
            hook_active: false,
        }
    }
}

pub type SharedSelection = Arc<Mutex<SelectionRuntime>>;

/// Decisions the hook hands to the selection worker thread.
#[derive(Debug)]
pub enum Decision {
    /// The full chord was freshly held: open the selection overlay.
    Activate,
    /// A chord key was released while the overlay is open (the overlay cancels
    /// when no drag is in progress, Mac hold-to-select parity).
    ShortcutReleased,
    /// Esc during selection mode.
    Cancelled,
    Recorded { session: String, outcome: RecordedOutcome },
}

/// Pure matcher logic, factored out for unit tests. The caller updates
/// `runtime.down` and passes whether this press was fresh (the low-level hook
/// has no autorepeat flag). Returns (swallow, decision).
pub fn handle_key_event(runtime: &mut SelectionRuntime, vk: u16, down: bool, fresh: bool) -> (bool, Option<Decision>) {
    // The recorder takes over completely while a recording session is live.
    if runtime.recording.is_some() {
        let claimed = runtime.claimed.contains(&vk);
        let outcome = if down || claimed || is_modifier_vk(vk) {
            runtime.recording.as_mut().unwrap().observe(vk, down)
        } else {
            crate::pocket::keys::RecordOutcome::Pending
        };
        if down {
            runtime.claimed.insert(vk);
        } else {
            runtime.claimed.remove(&vk);
        }
        runtime.recording_started = true;
        let swallow = claimed || down || is_modifier_vk(vk) || runtime.claimed.contains(&vk);
        return match outcome {
            crate::pocket::keys::RecordOutcome::Pending => (swallow, None),
            crate::pocket::keys::RecordOutcome::Chord(_) | crate::pocket::keys::RecordOutcome::Cancelled
            | crate::pocket::keys::RecordOutcome::Failed(_) => {
                runtime.recording = None;
                runtime.recording_started = false;
                let session = runtime.recording_session.take().unwrap_or_default();
                runtime.claimed.clear();
                let recorded = match outcome {
                    crate::pocket::keys::RecordOutcome::Chord(chord) => RecordedOutcome::Chord { modifiers: chord.modifiers, ordinary: chord.ordinary },
                    crate::pocket::keys::RecordOutcome::Cancelled => RecordedOutcome::Cancelled,
                    crate::pocket::keys::RecordOutcome::Failed(message) => RecordedOutcome::Failed(message),
                    crate::pocket::keys::RecordOutcome::Pending => unreachable!(),
                };
                (true, Some(Decision::Recorded { session, outcome: recorded }))
            }
        };
    }
    if runtime.selecting {
        // Selection mode owns the keyboard: Esc cancels, everything else is
        // swallowed so the chord never leaks into applications.
        if down {
            runtime.down.insert(vk);
            if vk == VK_ESCAPE {
                return (true, Some(Decision::Cancelled));
            }
            return (true, None);
        }
        runtime.down.remove(&vk);
        if runtime.shortcut_vks.contains(&vk) {
            // A chord key released while selecting: the overlay cancels when
            // no drag is in progress (hold-to-select parity).
            runtime.claimed.remove(&vk);
            return (true, Some(Decision::ShortcutReleased));
        }
        runtime.claimed.remove(&vk);
        return (true, None);
    }
    if !runtime.enabled || !runtime.hook_active {
        // While disallowed, still drain claimed keys so nothing sticks.
        if down {
            runtime.claimed.remove(&vk);
            return (false, None);
        }
        return (runtime.claimed.remove(&vk), None);
    }
    if down {
        if runtime.claimed.contains(&vk) {
            return (true, None); // repeats of a claimed chord key
        }
        if runtime.shortcut_vks.contains(&vk) {
            let all_held = runtime.shortcut_vks.iter().all(|required| runtime.down.contains(required));
            if all_held && fresh {
                runtime.claimed.insert(vk);
                return (true, Some(Decision::Activate));
            }
            if !is_modifier_vk(vk) {
                // A chord trigger pressed before the modifiers: claim it so it
                // never types, without activating.
                runtime.claimed.insert(vk);
                return (true, None);
            }
            // A modifier of an incomplete chord passes through untouched.
            return (false, None);
        }
        return (false, None);
    }
    // Release path: claimed keys drain silently.
    (runtime.claimed.remove(&vk), None)
}

thread_local! {
    static CONTEXT: std::cell::RefCell<Option<HookContext>> = const { std::cell::RefCell::new(None) };
}

struct HookContext {
    state: SharedSelection,
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
        let fresh = if down { runtime.down.insert(vk) } else { runtime.down.remove(&vk) };
        let (swallowed, decision) = handle_key_event(&mut runtime, vk, down, fresh);
        swallow = swallowed;
        if let Some(decision) = decision {
            let op = match decision {
                Decision::Activate => Op::ActivateSelection,
                Decision::ShortcutReleased => Op::ShortcutReleased,
                Decision::Cancelled => Op::SelectionCancelled,
                Decision::Recorded { session, outcome } => Op::Recorded { session, outcome },
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

pub struct SelectionHook {
    state: SharedSelection,
    thread: Option<HookHandle>,
}

struct HookHandle {
    thread_id: u32,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl SelectionHook {
    pub fn new(state: SharedSelection) -> SelectionHook {
        SelectionHook { state, thread: None }
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
            runtime.selecting = false;
            runtime.recording = None;
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
    use crate::pocket::keys::RecordKind;

    fn runtime(shortcut: &[u16], enabled: bool) -> SelectionRuntime {
        let mut runtime = SelectionRuntime::new();
        runtime.enabled = enabled;
        runtime.shortcut_vks = shortcut.to_vec();
        runtime.hook_active = true;
        runtime
    }

    fn press(runtime: &mut SelectionRuntime, vk: u16) -> (bool, Option<Decision>) {
        let fresh = runtime.down.insert(vk);
        handle_key_event(runtime, vk, true, fresh)
    }

    fn release(runtime: &mut SelectionRuntime, vk: u16) -> (bool, Option<Decision>) {
        runtime.down.remove(&vk);
        handle_key_event(runtime, vk, false, true)
    }

    const ALT: u16 = 0x12;
    const SHIFT: u16 = 0x10;
    const Q: u16 = 0x51;

    #[test]
    fn full_chord_activates_and_claims() {
        let mut runtime = runtime(&[SHIFT, ALT, Q], true);
        let (swallow, decision) = press(&mut runtime, SHIFT);
        assert!(!swallow, "modifiers pass through until the chord completes");
        assert!(decision.is_none());
        let (swallow, decision) = press(&mut runtime, ALT);
        assert!(!swallow);
        assert!(decision.is_none());
        let (swallow, decision) = press(&mut runtime, Q);
        assert!(swallow, "the completing key is claimed");
        assert!(matches!(decision, Some(Decision::Activate)));
        // Repeats stay swallowed without re-activating.
        let (swallow, decision) = press(&mut runtime, Q);
        assert!(swallow);
        assert!(decision.is_none());
    }

    #[test]
    fn single_key_shortcut_activates_on_fresh_press() {
        let mut runtime = runtime(&[Q], true);
        let (swallow, decision) = press(&mut runtime, Q);
        assert!(swallow);
        assert!(matches!(decision, Some(Decision::Activate)));
    }

    #[test]
    fn partial_chord_claims_the_trigger_but_not_modifiers() {
        let mut runtime = runtime(&[SHIFT, ALT, Q], true);
        press(&mut runtime, SHIFT);
        // Q alone (no Alt yet): claimed so it never types, but no activation.
        let (swallow, decision) = press(&mut runtime, Q);
        assert!(swallow);
        assert!(decision.is_none());
        assert!(runtime.claimed.contains(&Q));
    }

    #[test]
    fn disabled_runtime_never_claims_or_activates() {
        let mut runtime = runtime(&[Q], false);
        let (swallow, decision) = press(&mut runtime, Q);
        assert!(!swallow);
        assert!(decision.is_none());
    }

    #[test]
    fn selection_mode_swallows_keys_and_esc_cancels() {
        let mut runtime = runtime(&[Q], true);
        press(&mut runtime, Q);
        runtime.selecting = true;
        runtime.claimed.insert(Q);
        let (swallow, decision) = press(&mut runtime, 0x41); // A
        assert!(swallow, "selection mode owns the keyboard");
        assert!(decision.is_none());
        let (esc_swallow, esc_decision) = press(&mut runtime, VK_ESCAPE);
        assert!(esc_swallow);
        assert!(matches!(esc_decision, Some(Decision::Cancelled)));
    }

    #[test]
    fn chord_release_during_selection_signals_the_overlay() {
        let mut runtime = runtime(&[SHIFT, Q], true);
        press(&mut runtime, SHIFT);
        press(&mut runtime, Q);
        runtime.selecting = true;
        runtime.claimed.extend([SHIFT, Q]);
        let (swallow, decision) = release(&mut runtime, Q);
        assert!(swallow);
        assert!(matches!(decision, Some(Decision::ShortcutReleased)));
        // A second chord release signals again; the overlay treats it as a
        // no-op once it has already cancelled or captured.
        let (swallow, decision) = release(&mut runtime, SHIFT);
        assert!(swallow);
        assert!(matches!(decision, Some(Decision::ShortcutReleased)));
    }

    #[test]
    fn claimed_releases_drain_after_activation_without_typing() {
        let mut runtime = runtime(&[Q], true);
        press(&mut runtime, Q);
        // The overlay closes (capture finished) while Q is still held.
        runtime.selecting = false;
        runtime.claimed.insert(Q);
        let (swallow, decision) = release(&mut runtime, Q);
        assert!(swallow, "the claimed release must not type Q");
        assert!(decision.is_none());
    }

    #[test]
    fn recorder_session_swallows_and_reports_a_chord() {
        let mut runtime = runtime(&[Q], true);
        runtime.recording = Some(Recorder::new(RecordKind::Prefix));
        runtime.recording_session = Some("session-1".into());
        let (swallow, _) = press(&mut runtime, ALT);
        assert!(swallow);
        let (_, _) = press(&mut runtime, Q);
        let (_, _) = release(&mut runtime, ALT);
        let (up_swallow, decision) = release(&mut runtime, Q);
        assert!(up_swallow);
        match decision {
            Some(Decision::Recorded { session, outcome }) => {
                assert_eq!(session, "session-1");
                match outcome {
                    RecordedOutcome::Chord { modifiers, ordinary } => {
                        assert_eq!(modifiers, vec!["option"]);
                        assert_eq!(ordinary, Some(12)); // Carbon Q
                    }
                    other => panic!("expected chord, got {other:?}"),
                }
            }
            other => panic!("expected recorded, got {other:?}"),
        }
        assert!(runtime.recording.is_none());
    }

    #[test]
    fn recorder_esc_cancels_and_swallows() {
        let mut runtime = runtime(&[Q], true);
        runtime.recording = Some(Recorder::new(RecordKind::Prefix));
        runtime.recording_session = Some("session-2".into());
        let (swallow, decision) = press(&mut runtime, VK_ESCAPE);
        assert!(swallow);
        match decision {
            Some(Decision::Recorded { outcome, .. }) => assert!(matches!(outcome, RecordedOutcome::Cancelled)),
            other => panic!("expected recorded cancel, got {other:?}"),
        }
    }
}
