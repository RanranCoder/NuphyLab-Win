// Keyboard Pocket key model: the protocol keeps macOS Carbon key codes (the
// UI's slotKeyCodes / keyChoices vocabulary), translated to Windows VKs through
// the shared keymap crate. Also hosts the pure shortcut recorder state machine
// (port of PocketShortcutRecorder's PocketRecordingState, simplified: Windows
// has no secure-input or focus-monitor constraints here) and settings.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use keymap::{mac_from_vk, vk_from_mac};

pub const SLOT_COUNT: usize = 9;

/// Modifier names shared with the Mac protocol. `fn` stays in the vocabulary
/// for cross-platform settings parity; Windows cannot observe hardware Fn keys,
/// so a prefix that requires Fn never matches (UI shows "等待检测 Fn").
pub const MODIFIER_NAMES: [&str; 5] = ["fn", "control", "option", "shift", "command"];

/// Canonical modifier order used when reporting recorded prefixes.
const CANONICAL_MODIFIERS: [&str; 5] = ["fn", "control", "shift", "option", "command"];

/// Windows VKs treated as modifiers by the recorder and matcher. Left/right
/// variants collapse onto the canonical name (Shift/Control/Alt/Win).
fn modifier_vk_name(vk: u16) -> Option<&'static str> {
    match vk {
        0x10 | 0xA0 | 0xA1 => Some("shift"),
        0x11 | 0xA2 | 0xA3 => Some("control"),
        0x12 | 0xA4 | 0xA5 => Some("option"),
        0x5B | 0x5C => Some("command"),
        _ => None,
    }
}

/// Windows display name for a VK that can serve as a pocket key.
pub fn vk_display_name(vk: u16) -> Option<String> {
    let name: String = match vk {
        0x30..=0x39 => return Some(char::from_u32(48 + (vk - 0x30) as u32).unwrap_or('?').to_string()),
        0x41..=0x5A => return Some(char::from_u32(65 + (vk - 0x41) as u32).unwrap_or('?').to_string()),
        0x70..=0x7B => return Some(format!("F{}", vk - 0x70 + 1)),
        0x7C..=0x82 => return Some(format!("F{}", vk - 0x7C + 13)),
        0x20 => "Space".to_string(),
        0x09 => "Tab".to_string(),
        0x0D => "Enter".to_string(),
        0x08 => "Backspace".to_string(),
        0x1B => "Esc".to_string(),
        0x14 => "CapsLock".to_string(),
        0x2D => "Insert".to_string(),
        0x24 => "Home".to_string(),
        0x21 => "Page Up".to_string(),
        0x2E => "Delete".to_string(),
        0x23 => "End".to_string(),
        0x22 => "Page Down".to_string(),
        0x25 => "Left".to_string(),
        0x27 => "Right".to_string(),
        0x28 => "Down".to_string(),
        0x26 => "Up".to_string(),
        0xBB => "=".to_string(),
        0xBD => "-".to_string(),
        0xDD => "]".to_string(),
        0xDB => "[".to_string(),
        0xDE => "'".to_string(),
        0xBA => ";".to_string(),
        0xDC => "\\".to_string(),
        0xBC => ",".to_string(),
        0xBF => "/".to_string(),
        0xBE => ".".to_string(),
        0xC0 => "`".to_string(),
        0x60..=0x69 => return Some(format!("Num {}", vk - 0x60)),
        0x6A => "Num *".to_string(),
        0x6B => "Num +".to_string(),
        0x6D => "Num -".to_string(),
        0x6E => "Num .".to_string(),
        0x6F => "Num /".to_string(),
        _ => return None,
    };
    Some(name)
}

/// Carbon codes usable on Windows: forward-mappable and unambiguous in the
/// inverse table, so both configuration and the low-level hook agree.
pub fn usable_mac_codes() -> Vec<u8> {
    let mut codes = Vec::new();
    for mac in 0..=127u8 {
        if let Some(vk) = vk_from_mac(mac) {
            if mac_from_vk(vk) == Some(mac) && vk_display_name(vk).is_some() && modifier_vk_name(vk).is_none() {
                codes.push(mac);
            }
        }
    }
    codes
}

pub fn mac_name(mac: u8) -> Option<String> {
    vk_from_mac(mac).and_then(vk_display_name)
}

pub fn mac_to_vk(mac: u8) -> Option<u16> {
    vk_from_mac(mac).filter(|vk| mac_from_vk(*vk) == Some(mac))
}

// ---------------------------------------------------------------------------
// Settings

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PocketSettings {
    pub enabled: bool,
    #[serde(rename = "previewEnabled")]
    pub preview_enabled: bool,
    #[serde(rename = "clearAfterUse")]
    pub clear_after_use: bool,
    #[serde(rename = "plainText")]
    pub plain_text: bool,
    pub sequential: bool,
    #[serde(rename = "sequenceIndex")]
    pub sequence_index: i32,
    #[serde(rename = "retainDays")]
    pub retain_days: i64,
    #[serde(rename = "useModifiers")]
    pub use_modifiers: Vec<String>,
    #[serde(rename = "captureModifiers")]
    pub capture_modifiers: Vec<String>,
    #[serde(rename = "useTriggerKeyCode")]
    pub use_trigger: Option<u8>,
    #[serde(rename = "captureTriggerKeyCode")]
    pub capture_trigger: Option<u8>,
    #[serde(rename = "slotKeyCodes")]
    pub slot_key_codes: Vec<u8>,
    #[serde(rename = "slotOrder")]
    pub slot_order: Vec<usize>,
}

impl Default for PocketSettings {
    fn default() -> Self {
        PocketSettings {
            enabled: false,
            preview_enabled: true,
            clear_after_use: false,
            plain_text: false,
            sequential: false,
            sequence_index: 0,
            retain_days: 0,
            use_modifiers: vec!["fn".into()],
            capture_modifiers: vec!["fn".into(), "shift".into()],
            use_trigger: None,
            capture_trigger: None,
            slot_key_codes: vec![18, 19, 20, 21, 23, 22, 26, 28, 25],
            slot_order: (0..SLOT_COUNT).collect(),
        }
    }
}

impl PocketSettings {
    /// Port of PocketSettings.apply: patch only the fields present in `values`,
    /// with Mac's clamping rules.
    pub fn apply(&mut self, values: &serde_json::Value) {
        let get = |key: &str| values.get(key);
        if let Some(x) = get("enabled").and_then(serde_json::Value::as_bool) {
            self.enabled = x;
        }
        if let Some(x) = get("previewEnabled").and_then(serde_json::Value::as_bool) {
            self.preview_enabled = x;
        }
        if let Some(x) = get("clearAfterUse").and_then(serde_json::Value::as_bool) {
            self.clear_after_use = x;
        }
        if let Some(x) = get("plainText").and_then(serde_json::Value::as_bool) {
            self.plain_text = x;
        }
        if let Some(x) = get("sequential").and_then(serde_json::Value::as_bool) {
            self.sequential = x;
        }
        if let Some(x) = get("sequenceIndex").and_then(serde_json::Value::as_i64) {
            self.sequence_index = x.clamp(0, 8) as i32;
        }
        if let Some(x) = get("retainDays").and_then(serde_json::Value::as_i64) {
            self.retain_days = x.max(0);
        }
        if let Some(x) = get("useModifiers").and_then(parse_modifiers) {
            self.use_modifiers = x;
        }
        if let Some(x) = get("captureModifiers").and_then(parse_modifiers) {
            self.capture_modifiers = x;
        }
        if let Some(x) = get("useTriggerKeyCode") {
            self.use_trigger = parse_trigger(x);
        }
        if let Some(x) = get("captureTriggerKeyCode") {
            self.capture_trigger = parse_trigger(x);
        }
        if let Some(x) = get("slotKeyCodes").and_then(serde_json::Value::as_array) {
            self.slot_key_codes = x.iter().filter_map(|v| v.as_u64().map(|n| n as u8)).collect();
        }
        if let Some(x) = get("slotOrder").and_then(serde_json::Value::as_array) {
            self.slot_order = x
                .iter()
                .filter_map(|v| v.as_u64().map(|n| n as usize))
                .collect();
        }
    }

    /// Port of PocketSettings.validate, Windows-adjusted: the Mac rule that
    /// store/retrieve cannot differ only by Fn is dropped (Fn never matches on
    /// Windows, so an Fn-only difference cannot fire).
    pub fn validate(&self) -> Result<(), String> {
        let zh_en = |zh: &str, en: &str| format!("{zh} / {en}");
        for (label, modifiers, trigger) in [
            ("取用", &self.use_modifiers, self.use_trigger),
            ("收纳", &self.capture_modifiers, self.capture_trigger),
        ] {
            if modifiers.is_empty() && trigger.is_none() {
                return Err(zh_en(
                    &format!("{label}前缀至少需要一个修饰键或普通按键。"),
                    &format!("The {label} prefix needs at least one modifier or a key."),
                ));
            }
            if modifiers.iter().any(|m| !MODIFIER_NAMES.contains(&m.as_str())) {
                return Err(zh_en(
                    "快捷键含有不支持的修饰键。",
                    "The shortcut contains an unsupported modifier.",
                ));
            }
            if modifiers.iter().collect::<HashSet<_>>().len() != modifiers.len() {
                return Err(zh_en(
                    "快捷键不能重复修饰键。",
                    "The shortcut cannot repeat a modifier.",
                ));
            }
            if let Some(code) = trigger {
                if mac_name(code).is_none() {
                    return Err(zh_en(
                        "前缀含有无效键码或修饰键。",
                        "The prefix contains an invalid key code.",
                    ));
                }
                if self.slot_key_codes.contains(&code) {
                    return Err(zh_en(
                        "前缀键不能同时作为口袋槽位按键。",
                        "The prefix key cannot also be a pocket slot key.",
                    ));
                }
            }
        }
        if self.use_modifiers.iter().collect::<HashSet<_>>() == self.capture_modifiers.iter().collect::<HashSet<_>>()
            && self.use_trigger == self.capture_trigger
        {
            return Err(zh_en(
                "取用与收纳不能使用完全相同的前缀。",
                "Store and retrieve must use different prefixes.",
            ));
        }
        if self.slot_key_codes.len() != SLOT_COUNT {
            return Err(zh_en(
                "需要为九个口袋各设置一个按键。",
                "Assign one key to each of the nine slots.",
            ));
        }
        if self.slot_key_codes.iter().collect::<HashSet<_>>().len() != SLOT_COUNT {
            return Err(zh_en(
                "九个口袋的按键不能重复。",
                "Each slot must have a different key.",
            ));
        }
        if self.slot_key_codes.iter().any(|c| mac_name(*c).is_none()) {
            return Err(zh_en(
                "口袋按键含有无效键码或修饰键。",
                "A slot key contains an invalid key code.",
            ));
        }
        let valid_order = self.slot_order.len() == SLOT_COUNT
            && self.slot_order.iter().collect::<HashSet<_>>().len() == SLOT_COUNT
            && self.slot_order.iter().all(|i| (0..SLOT_COUNT).contains(i));
        if !valid_order {
            return Err(zh_en(
                "显示顺序必须包含全部九个口袋且不能重复。",
                "The display order must contain all nine slots exactly once.",
            ));
        }
        Ok(())
    }

    /// True when either prefix requires the (unobservable on Windows) Fn key.
    pub fn uses_fn(&self) -> bool {
        self.use_modifiers.iter().any(|m| m == "fn") || self.capture_modifiers.iter().any(|m| m == "fn")
    }

    /// Display names for the nine slot keys (Windows layout names).
    pub fn slot_key_names(&self) -> Vec<String> {
        self.slot_key_codes
            .iter()
            .map(|code| mac_name(*code).unwrap_or_else(|| format!("键 {code}")))
            .collect()
    }
}

fn parse_modifiers(value: &serde_json::Value) -> Option<Vec<String>> {
    let items = value.as_array()?;
    Some(
        items
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_ascii_lowercase())
            .collect(),
    )
}

fn parse_trigger(value: &serde_json::Value) -> Option<u8> {
    if value.is_null() {
        return None;
    }
    value.as_u64().filter(|n| *n <= u8::MAX as u64).map(|n| n as u8)
}

// ---------------------------------------------------------------------------
// Shortcut recorder (port of PocketRecordingState, Windows-simplified)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordKind {
    Prefix,
    Slot,
}

#[derive(Clone, Debug)]
pub struct RecordChord {
    pub modifiers: Vec<String>,
    pub ordinary: Option<u8>, // Carbon code
}

#[derive(Clone, Debug)]
pub enum RecordOutcome {
    Pending,
    Chord(RecordChord),
    Cancelled,
    Failed(String),
}

/// Pure recording state: a chord completes only after every recorded key has
/// been released. Esc cancels. Pre-held keys at session start are ignored by
/// the caller feeding only fresh key events.
#[derive(Debug)]
pub struct Recorder {
    pub kind: RecordKind,
    modifier_down: HashSet<u16>,
    modifier_seen: HashSet<&'static str>,
    ordinary_down: HashSet<u16>,
    ordinary_seen: Option<u16>,
    finished: bool,
}

impl Recorder {
    pub fn new(kind: RecordKind) -> Recorder {
        Recorder {
            kind,
            modifier_down: HashSet::new(),
            modifier_seen: HashSet::new(),
            ordinary_down: HashSet::new(),
            ordinary_seen: None,
            finished: false,
        }
    }

    pub fn observe(&mut self, vk: u16, down: bool) -> RecordOutcome {
        if self.finished {
            return RecordOutcome::Pending;
        }
        if vk == 0x1B && down {
            // Esc cancels (Mac keyCode 53).
            self.finished = true;
            return RecordOutcome::Cancelled;
        }
        if let Some(name) = modifier_vk_name(vk) {
            if down {
                self.modifier_down.insert(vk);
                self.modifier_seen.insert(name);
            } else {
                self.modifier_down.remove(&vk);
            }
        } else if down {
            if self.ordinary_down.insert(vk) {
                if self.ordinary_seen.is_some() && self.ordinary_seen != Some(vk) {
                    self.finished = true;
                    return RecordOutcome::Failed(if self.kind == RecordKind::Slot {
                        "槽位请使用一个普通按键，不带修饰键。 / Use one ordinary key without modifiers for a slot.".into()
                    } else {
                        "普通按键最多一个，可以搭配一个修饰键。 / Use one ordinary key, optionally with one modifier.".into()
                    });
                }
                self.ordinary_seen = Some(vk);
            }
        } else {
            self.ordinary_down.remove(&vk);
        }
        // Duplicate modifiers of different sides (L/R Shift) stay one name;
        // that never exceeds the two-key budget on its own.
        let total = self.modifier_seen.len() + if self.ordinary_seen.is_some() { 1 } else { 0 };
        if self.kind == RecordKind::Slot && (!self.modifier_seen.is_empty() || total > 1) {
            self.finished = true;
            return RecordOutcome::Failed(
                "槽位请使用一个普通按键，不带修饰键。 / Use one ordinary key without modifiers for a slot.".into(),
            );
        }
        if total > 2 {
            self.finished = true;
            return RecordOutcome::Failed(
                "前缀只能使用一个或两个按键。 / Choose one or two keys for a prefix.".into(),
            );
        }
        // Complete only when every recorded key has been released.
        if self.modifier_down.is_empty() && self.ordinary_down.is_empty() && total > 0 {
            self.finished = true;
            let ordinary_mac = self.ordinary_seen.and_then(mac_from_vk);
            if self.ordinary_seen.is_some() && ordinary_mac.is_none() {
                return RecordOutcome::Failed(
                    "这组按键无法用作快捷键。 / This key combination cannot be used as a shortcut.".into(),
                );
            }
            let modifiers: Vec<String> = CANONICAL_MODIFIERS
                .iter()
                .filter(|name| self.modifier_seen.contains(*name))
                .map(|name| name.to_string())
                .collect();
            return RecordOutcome::Chord(RecordChord { modifiers, ordinary: ordinary_mac });
        }
        RecordOutcome::Pending
    }
}

// ---------------------------------------------------------------------------
// Hotkey matcher helpers (port of PocketSlotMatcher, Windows-simplified)

#[derive(Clone, Debug)]
pub struct Prefix {
    pub action: &'static str, // "use" | "capture"
    pub modifiers: Vec<u16>,  // required observable VKs (fn contributes none)
    pub trigger: Option<u16>, // VK of the optional trigger key
    pub requires_fn: bool,    // an Fn-only or Fn-containing prefix never matches
}

impl Prefix {
    /// Exact modifier-set match against the currently held VKs. A prefix that
    /// requires Fn can never match on Windows (Fn is hardware-level there).
    pub fn modifiers_match(&self, held: &HashSet<u16>) -> bool {
        if self.requires_fn {
            return false;
        }
        self.modifiers.iter().copied().collect::<HashSet<u16>>() == *held
    }

    /// Trigger satisfied: no trigger key, or the trigger key is held.
    pub fn trigger_held(&self, down: &HashSet<u16>) -> bool {
        self.trigger.is_none_or(|vk| down.contains(&vk))
    }
}

#[derive(Clone, Debug, Default)]
pub struct PocketConfig {
    pub enabled: bool,
    pub prefixes: Vec<Prefix>,
    pub slot_vks: Vec<Option<u16>>, // per slot index
}

impl PocketConfig {
    pub fn from_settings(settings: &PocketSettings) -> PocketConfig {
        let prefix = |modifiers: &[String], trigger: Option<u8>, action: &'static str| Prefix {
            action,
            modifiers: modifiers.iter().filter_map(|name| modifier_vk(name)).collect(),
            trigger: trigger.and_then(mac_to_vk),
            requires_fn: modifiers.iter().any(|name| name == "fn"),
        };
        PocketConfig {
            enabled: settings.enabled,
            prefixes: vec![
                prefix(&settings.use_modifiers, settings.use_trigger, "use"),
                prefix(&settings.capture_modifiers, settings.capture_trigger, "capture"),
            ],
            slot_vks: settings
                .slot_key_codes
                .iter()
                .map(|code| mac_to_vk(*code))
                .collect(),
        }
    }
}

fn modifier_vk(name: &str) -> Option<u16> {
    match name {
        "control" => Some(0x11),
        "option" => Some(0x12),
        "shift" => Some(0x10),
        "command" => Some(0x5B),
        _ => None, // "fn": unobservable on Windows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(mods: &[&str], ordinary: Option<u16>) -> RecordOutcome {
        let mut recorder = Recorder::new(RecordKind::Prefix);
        let mut terminal: Option<RecordOutcome> = None;
        let step = |outcome: RecordOutcome, terminal: &mut Option<RecordOutcome>| {
            if terminal.is_none() && !matches!(outcome, RecordOutcome::Pending) {
                *terminal = Some(outcome);
            }
        };
        for name in mods {
            let outcome = recorder.observe(modifier_vk(name).unwrap(), true);
            step(outcome, &mut terminal);
        }
        if let Some(vk) = ordinary {
            let outcome = recorder.observe(vk, true);
            step(outcome, &mut terminal);
        }
        for name in mods {
            let outcome = recorder.observe(modifier_vk(name).unwrap(), false);
            step(outcome, &mut terminal);
        }
        if let Some(vk) = ordinary {
            let outcome = recorder.observe(vk, false);
            step(outcome, &mut terminal);
        }
        terminal.unwrap_or(RecordOutcome::Pending)
    }

    #[test]
    fn default_settings_validate() {
        assert!(PocketSettings::default().validate().is_ok());
    }

    #[test]
    fn slot_recording_rejects_modifiers() {
        let mut recorder = Recorder::new(RecordKind::Slot);
        // The modifier down itself already fails a slot recording.
        let outcome = recorder.observe(0x11, true); // Ctrl down
        assert!(matches!(outcome, RecordOutcome::Failed(_)));
        // Further events stay pending (session finished).
        assert!(matches!(recorder.observe(0x31, true), RecordOutcome::Pending));
    }

    #[test]
    fn prefix_shift_plus_one_round_trips_to_mac_code() {
        match chord(&["shift"], Some(0x31)) {
            RecordOutcome::Chord(c) => {
                assert_eq!(c.modifiers, vec!["shift"]);
                assert_eq!(c.ordinary, Some(18)); // Carbon "1"
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn modifier_only_prefix_records() {
        match chord(&["control", "option"], None) {
            RecordOutcome::Chord(c) => {
                assert_eq!(c.modifiers, vec!["control", "option"]);
                assert_eq!(c.ordinary, None);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn three_keys_fail() {
        assert!(matches!(chord(&["control", "shift"], Some(0x41)), RecordOutcome::Failed(_)));
    }

    #[test]
    fn esc_cancels_recording() {
        let mut recorder = Recorder::new(RecordKind::Prefix);
        assert!(matches!(recorder.observe(0x1B, true), RecordOutcome::Cancelled));
    }

    #[test]
    fn incomplete_chord_stays_pending() {
        let mut recorder = Recorder::new(RecordKind::Prefix);
        recorder.observe(0x11, true);
        assert!(matches!(recorder.observe(0x43, true), RecordOutcome::Pending));
    }

    #[test]
    fn settings_patch_applies_camel_case() {
        let mut settings = PocketSettings::default();
        let values: serde_json::Value = serde_json::json!({
            "enabled": true,
            "clearAfterUse": true,
            "retainDays": 7,
            "sequenceIndex": 12,
            "slotOrder": [8,7,6,5,4,3,2,1,0],
            "useModifiers": ["control"],
            "useTriggerKeyCode": null,
        });
        settings.apply(&values);
        assert!(settings.enabled && settings.clear_after_use);
        assert_eq!(settings.retain_days, 7);
        assert_eq!(settings.sequence_index, 8);
        assert_eq!(settings.slot_order, vec![8, 7, 6, 5, 4, 3, 2, 1, 0]);
        assert_eq!(settings.use_modifiers, vec!["control"]);
        assert_eq!(settings.use_trigger, None);
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn validation_rejects_bad_slot_order() {
        let mut settings = PocketSettings::default();
        settings.slot_order = vec![0, 1, 2, 3, 4, 5, 6, 7, 7];
        assert!(settings.validate().is_err());
        settings.slot_order = vec![0, 1, 2, 3, 4, 5, 6, 7];
        assert!(settings.validate().is_err());
    }

    #[test]
    fn validation_rejects_identical_prefixes() {
        let mut settings = PocketSettings::default();
        settings.use_modifiers = vec!["control".into()];
        settings.capture_modifiers = vec!["control".into()];
        settings.use_trigger = Some(18);
        settings.capture_trigger = Some(18);
        assert!(settings.validate().is_err());
    }

    #[test]
    fn validation_rejects_trigger_equal_slot_key() {
        let mut settings = PocketSettings::default();
        settings.use_trigger = Some(18);
        assert!(settings.validate().is_err());
    }

    #[test]
    fn default_slot_keys_map_to_digits() {
        let settings = PocketSettings::default();
        let names: Vec<String> = settings
            .slot_key_codes
            .iter()
            .map(|c| mac_name(*c).unwrap())
            .collect();
        assert_eq!(names, vec!["1", "2", "3", "4", "5", "6", "7", "8", "9"]);
        for code in &settings.slot_key_codes {
            assert!(mac_to_vk(*code).is_some());
        }
    }

    #[test]
    fn usable_codes_cover_letters_and_digits() {
        let codes = usable_mac_codes();
        assert!(codes.contains(&0)); // A
        assert!(codes.contains(&18)); // 1
        assert!(codes.contains(&122)); // F1
        assert!(codes.contains(&49)); // Space
        assert!(!codes.contains(&55)); // Command modifier excluded
    }

    #[test]
    fn config_from_settings_requires_trigger_held() {
        let mut settings = PocketSettings::default();
        settings.use_modifiers = vec!["control".into()];
        settings.use_trigger = Some(18);
        let config = PocketConfig::from_settings(&settings);
        let prefix = &config.prefixes[0];
        let mut held = HashSet::new();
        held.insert(0x11);
        assert!(prefix.modifiers_match(&held));
        held.insert(0x10);
        assert!(!prefix.modifiers_match(&held)); // extra Shift breaks the match
        let mut down = HashSet::new();
        down.insert(0x31);
        assert!(prefix.trigger_held(&down));
        assert!(!prefix.trigger_held(&HashSet::new()));
    }

    #[test]
    fn fn_only_prefix_never_matches() {
        let settings = PocketSettings::default(); // use prefix contains fn only
        let config = PocketConfig::from_settings(&settings);
        let held: HashSet<u16> = HashSet::new();
        let use_prefix = &config.prefixes[0];
        assert!(use_prefix.requires_fn);
        assert!(use_prefix.modifiers.is_empty());
        assert!(!use_prefix.modifiers_match(&held));
    }
}
