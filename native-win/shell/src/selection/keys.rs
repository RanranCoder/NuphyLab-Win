// Smart Selection key model and tables: the protocol keeps macOS Carbon key
// codes and modifier masks (the UI's preferences.shortcut vocabulary), the
// action and translation-language tables mirror ui/selection-actions.ts and
// ui/selection-languages.ts, and the session state machine ports the relevant
// subset of native/selection/SelectionSession.swift (hold → select → request →
// answer, with action switching and follow-ups; voice phases are absent on
// Windows because speech recognition is not implemented in this port).

use serde::{Deserialize, Serialize};

use keymap::{mac_from_vk, vk_from_mac};

use crate::pocket::keys::{mac_name, RecordChord};

// Carbon modifier masks (the Mac protocol stores these in preferences).
pub const CARBON_CMD: u32 = 256;
pub const CARBON_SHIFT: u32 = 512;
pub const CARBON_OPTION: u32 = 2048;
pub const CARBON_CONTROL: u32 = 4096;

/// Canonical modifier order used when reporting recorded shortcuts (option
/// before shift matches the UI's default "⌥ ⇧ Q" label).
const CANONICAL_MODIFIERS: [&str; 4] = ["control", "option", "shift", "command"];

fn carbon_mask(name: &str) -> Option<u32> {
    match name {
        "control" => Some(CARBON_CONTROL),
        "shift" => Some(CARBON_SHIFT),
        "option" => Some(CARBON_OPTION),
        "command" => Some(CARBON_CMD),
        _ => None,
    }
}

fn modifier_vk(name: &str) -> Option<u16> {
    match name {
        "control" => Some(0x11),
        "shift" => Some(0x10),
        "option" => Some(0x12),
        "command" => Some(0x5B),
        _ => None,
    }
}

fn modifier_symbol(name: &str) -> &'static str {
    match name {
        "control" => "⌃",
        "shift" => "⇧",
        "option" => "⌥",
        "command" => "⌘",
        _ => "",
    }
}

/// The recorded shortcut, encoded exactly like the UI's preferences.shortcut.
/// A modifiers-only recording stores key_code 0 with `modifier_only` set, so
/// the trigger VK is excluded from matching (the UI's optional modifierOnly
/// flag carries the same meaning).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Shortcut {
    #[serde(rename = "keyCode")]
    pub key_code: u8, // Carbon code (12 = Q); 0 for modifiers-only chords
    pub modifiers: u32,
    pub label: String,
    #[serde(rename = "modifierOnly", default, skip_serializing_if = "std::ops::Not::not")]
    pub modifier_only: bool,
}

impl Default for Shortcut {
    fn default() -> Self {
        // Alt+Shift+Q (Option-Shift-Q parity): Carbon shift+option = 2560.
        Shortcut { key_code: 12, modifiers: CARBON_SHIFT | CARBON_OPTION, label: "⌥ ⇧ Q".into(), modifier_only: false }
    }
}

impl Shortcut {
    /// Builds a shortcut from a pocket recorder chord (modifier names in
    /// canonical order + optional ordinary Carbon code).
    pub fn from_chord(chord: &RecordChord) -> Option<Shortcut> {
        let mut mask = 0u32;
        let mut symbols = Vec::new();
        for name in CANONICAL_MODIFIERS.iter().filter(|name| chord.modifiers.contains(&(*name).to_string())) {
            mask |= carbon_mask(name)?;
            symbols.push(modifier_symbol(name));
        }
        match chord.ordinary {
            Some(key_code) => {
                let label = match symbols.is_empty() {
                    true => mac_name(key_code)?,
                    false => format!("{} {}", symbols.join(" "), mac_name(key_code)?),
                };
                Some(Shortcut { key_code, modifiers: mask, label, modifier_only: false })
            }
            None => {
                if symbols.is_empty() {
                    return None; // An empty chord is never a shortcut.
                }
                Some(Shortcut { key_code: 0, modifiers: mask, label: symbols.join(" "), modifier_only: true })
            }
        }
    }

    /// Every Windows VK that must be held for the chord (modifiers + trigger).
    pub fn vks(&self) -> Vec<u16> {
        let mut vks: Vec<u16> = Vec::new();
        for (name, mask) in
            [("control", CARBON_CONTROL), ("shift", CARBON_SHIFT), ("option", CARBON_OPTION), ("command", CARBON_CMD)]
        {
            if self.modifiers & mask != 0 {
                if let Some(vk) = modifier_vk(name) {
                    vks.push(vk);
                }
            }
        }
        if !self.modifier_only {
            if let Some(vk) = vk_from_mac(self.key_code).filter(|vk| mac_from_vk(*vk) == Some(self.key_code)) {
                vks.push(vk);
            }
        }
        vks
    }
}

// ---------------------------------------------------------------------------
// Action and language tables (parity with the UI's TS tables; the server
// re-validates both, these drive the hook-free parts of the shell).

pub const ACTIONS: [(&str, &str); 4] =
    [("translate", "翻译"), ("summarize", "总结"), ("explain", "解释"), ("reply", "回复")];

pub fn action_label(action: &str) -> &'static str {
    ACTIONS.iter().find(|(id, _)| *id == action).map(|(_, label)| *label).unwrap_or("翻译")
}

pub fn is_valid_action(action: &str) -> bool {
    ACTIONS.iter().any(|(id, _)| *id == action)
}

/// Ordered enabled actions (parity with enabledSelectionActions): unknown ids
/// drop out, an empty set falls back to the default action, and the default
/// action itself always participates.
pub fn enabled_actions(saved: Option<&Vec<String>>, default_action: &str) -> Vec<String> {
    let ordered: Vec<String> = ACTIONS
        .iter()
        .map(|(id, _)| id.to_string())
        .filter(|id| saved.is_none_or(|list| list.contains(id)))
        .collect();
    if !ordered.is_empty() {
        return ordered;
    }
    let fallback = ACTIONS.iter().find(|(id, _)| *id == default_action).map(|(id, _)| id.to_string());
    vec![fallback.unwrap_or_else(|| "translate".to_string())]
}

/// Keep in parity with ui/selection-languages.ts.
pub const TRANSLATION_LANGUAGES: [&str; 16] = [
    "zh-CN", "zh-TW", "en", "ja", "ko", "fr", "de", "es", "pt", "it", "ru", "ar", "hi", "th", "vi", "id",
];

pub fn is_valid_language(language: &str) -> bool {
    TRANSLATION_LANGUAGES.contains(&language)
}

// ---------------------------------------------------------------------------
// Session phases (SelectionPhase parity, minus the voice/OCR-only states).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Holding,
    Selecting,
    Requesting,
    Completed,
    Failed,
    Cancelled,
}

impl Phase {
    pub fn label(&self) -> &'static str {
        match self {
            Phase::Holding => "holding",
            Phase::Selecting => "selecting",
            Phase::Requesting => "requesting",
            Phase::Completed => "completed",
            Phase::Failed => "failed",
            Phase::Cancelled => "cancelled",
        }
    }
}

/// One selection flow's transient state (a fresh instance per gesture, like
/// SelectionSession.begin).
pub struct Session {
    pub phase: Phase,
    pub action: String,
    pub result: String,
    pub error: String,
    /// Conversation history entries for follow-ups: (role, text) pairs in
    /// order. Filled from the server's `turn` echo on completion.
    pub history: Vec<(String, String)>,
}

impl Session {
    pub fn begin(action: &str) -> Session {
        Session {
            phase: Phase::Holding,
            action: action.to_string(),
            result: String::new(),
            error: String::new(),
            history: Vec::new(),
        }
    }

    /// Pure transition helpers (unit-tested): every guard rejects stale or
    /// out-of-order events instead of corrupting the session.
    pub fn start_selecting(&mut self) {
        if self.phase == Phase::Holding {
            self.phase = Phase::Selecting;
        }
    }

    pub fn start_requesting(&mut self) {
        if matches!(self.phase, Phase::Selecting | Phase::Completed | Phase::Failed) {
            self.phase = Phase::Requesting;
            self.result.clear();
            self.error.clear();
        }
    }

    pub fn append_delta(&mut self, text: &str) {
        if self.phase == Phase::Requesting {
            self.result.push_str(text);
        }
    }

    pub fn complete(&mut self, text: &str, user_turn: &str) {
        if self.phase != Phase::Requesting {
            return;
        }
        self.result = text.to_string();
        self.history.push(("user".into(), user_turn.to_string()));
        self.history.push(("assistant".into(), text.to_string()));
        self.phase = Phase::Completed;
    }

    pub fn fail(&mut self, message: &str) {
        if self.phase != Phase::Requesting {
            return;
        }
        self.error = message.to_string();
        self.phase = Phase::Failed;
    }

    /// Wheel/button action switching: only enabled actions cycle, and the
    /// switch re-requests with the same content (history resets, Mac parity).
    /// Switching requires delivered content (a completed/failed attempt).
    pub fn switch_action(&mut self, enabled: &[String]) -> Option<String> {
        if !matches!(self.phase, Phase::Completed | Phase::Failed | Phase::Cancelled) {
            return None;
        }
        let position = enabled.iter().position(|action| *action == self.action)?;
        let next = enabled.get((position + 1) % enabled.len())?;
        self.action = next.clone();
        self.phase = Phase::Completed; // Content stays; re-request runs next.
        self.result.clear();
        self.error.clear();
        self.history.clear();
        Some(next.clone())
    }

    pub fn begin_follow_up(&mut self) -> Result<(), String> {
        match self.phase {
            Phase::Completed => {
                self.phase = Phase::Requesting;
                self.result.clear();
                self.error.clear();
                Ok(())
            }
            Phase::Requesting => Err("正在回答中，请稍候。".into()),
            _ => Err("请先完成一次框选，再继续追问。".into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Shell-side preferences (the server is authoritative; the shell keeps a
// cached copy plus its own shortcut/panel-position config file).

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    pub shortcut: Shortcut,
    #[serde(rename = "panelX")]
    pub panel_x: i32,
    #[serde(rename = "panelY")]
    pub panel_y: i32,
    #[serde(rename = "panelPositionSaved")]
    pub panel_position_saved: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pocket::keys::RecordChord;

    fn chord(modifiers: &[&str], ordinary: Option<u8>) -> RecordChord {
        RecordChord {
            modifiers: modifiers.iter().map(|s| s.to_string()).collect(),
            ordinary,
        }
    }

    #[test]
    fn default_shortcut_is_alt_shift_q() {
        let shortcut = Shortcut::default();
        assert_eq!(shortcut.key_code, 12);
        assert_eq!(shortcut.modifiers, 2560);
        assert_eq!(shortcut.label, "⌥ ⇧ Q");
        let vks = shortcut.vks();
        assert!(vks.contains(&0x10) && vks.contains(&0x12) && vks.contains(&0x51), "{vks:?}");
    }

    #[test]
    fn chords_convert_to_carbon_labels_and_back() {
        let shortcut = Shortcut::from_chord(&chord(&["option", "shift"], Some(12))).unwrap();
        assert_eq!(shortcut.modifiers, CARBON_OPTION | CARBON_SHIFT);
        assert_eq!(shortcut.label, "⌥ ⇧ Q");
        assert_eq!(shortcut.vks(), vec![0x10, 0x12, 0x51]);
        // Modifier-only chords are allowed (single-key mode also works).
        let modifier_only = Shortcut::from_chord(&chord(&["command", "control"], None)).unwrap();
        assert_eq!(modifier_only.label, "⌃ ⌘");
        assert_eq!(modifier_only.vks(), vec![0x11, 0x5B]);
        // An ordinary key without modifiers records as a single key.
        let single = Shortcut::from_chord(&chord(&[], Some(18))).unwrap();
        assert_eq!(single.label, "1");
        assert_eq!(single.vks(), vec![0x31]);
        // Unknown Carbon codes are rejected rather than half-saved.
        assert!(Shortcut::from_chord(&chord(&[], Some(255))).is_none());
    }

    #[test]
    fn action_table_matches_the_ui_contract() {
        assert_eq!(ACTIONS.len(), 4);
        assert_eq!(action_label("translate"), "翻译");
        assert_eq!(action_label("reply"), "回复");
        assert!(is_valid_action("summarize"));
        assert!(!is_valid_action("extractText"));
        assert_eq!(enabled_actions(None, "translate"), vec!["translate", "summarize", "explain", "reply"]);
        assert_eq!(
            enabled_actions(Some(&vec!["explain".into(), "bogus".into()]), "translate"),
            vec!["explain"]
        );
        // An empty enabled set falls back to the default action.
        assert_eq!(enabled_actions(Some(&vec![]), "summarize"), vec!["summarize"]);
        assert_eq!(enabled_actions(Some(&vec![]), "nope"), vec!["translate"]);
    }

    #[test]
    fn language_table_stays_in_parity() {
        assert_eq!(TRANSLATION_LANGUAGES.len(), 16);
        assert!(is_valid_language("zh-CN") && is_valid_language("vi"));
        assert!(!is_valid_language("xx-XX"));
    }

    #[test]
    fn session_transitions_follow_the_mac_flow() {
        let mut session = Session::begin("translate");
        assert_eq!(session.phase, Phase::Holding);
        session.start_selecting();
        assert_eq!(session.phase, Phase::Selecting);
        session.start_requesting();
        assert_eq!(session.phase, Phase::Requesting);
        session.append_delta("你");
        session.append_delta("好");
        assert_eq!(session.result, "你好");
        session.complete("你好", "{\"user_task\":\"…\"}");
        assert_eq!(session.phase, Phase::Completed);
        assert_eq!(session.history.len(), 2);
        // Stale events are ignored.
        session.fail("late");
        assert_eq!(session.phase, Phase::Completed);
        session.append_delta("late");
        assert_eq!(session.result, "你好");
    }

    #[test]
    fn session_switch_cycles_enabled_actions_and_resets_context() {
        let enabled = vec!["translate".into(), "summarize".into(), "explain".into(), "reply".into()];
        let mut session = Session::begin("translate");
        // Switching before a capture only makes sense with content; guard it.
        assert_eq!(session.switch_action(&enabled), None);
        session.phase = Phase::Completed;
        assert_eq!(session.switch_action(&enabled), Some("summarize".into()));
        assert_eq!(session.switch_action(&enabled), Some("explain".into()));
        assert_eq!(session.switch_action(&enabled), Some("reply".into()));
        assert_eq!(session.switch_action(&enabled), Some("translate".into()));
        // A disabled action still participates if it was the current one.
        let only_translate = vec!["translate".into()];
        assert_eq!(session.switch_action(&only_translate), Some("translate".into()));
        // Requests in flight must not switch.
        let mut requesting = Session::begin("translate");
        requesting.start_selecting();
        requesting.start_requesting();
        assert_eq!(requesting.switch_action(&enabled), None);
    }

    #[test]
    fn follow_ups_require_a_completed_session() {
        let mut session = Session::begin("translate");
        session.start_selecting();
        session.start_requesting();
        session.complete("答案", "turn");
        assert!(session.begin_follow_up().is_ok());
        assert_eq!(session.phase, Phase::Requesting);
        assert!(session.begin_follow_up().is_err(), "a live request cannot follow up");
        session.complete("答案2", "turn2");
        assert_eq!(session.history.len(), 4);
        let mut fresh = Session::begin("summarize");
        assert!(fresh.begin_follow_up().is_err());
    }
}
