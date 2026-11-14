// macOS Carbon virtual key codes (the wire format of shared/trigger-keys.json
// and the codexKeys bridge) mapped to Windows virtual-key codes. Both the
// shell (Codex slot hotkeys) and the HID bridge (keyMonitor / triggerMonitor /
// shortcut injection) consume this table, so the mapping lives in one crate.
//
// The forward table covers every Carbon code with a sensible Windows
// counterpart. The inverse table deliberately covers only the unambiguous
// typing keys from shared/trigger-keys.json plus the plain modifiers: VK codes
// like 0x69 (VK_NUMPAD9 vs. F13's forward image) cannot be inverted safely,
// and listeners only ever match codes that come from that allowlist.

/// Forward: Carbon keyCode (0..=127) -> Windows VK.
pub fn vk_from_mac(mac: u8) -> Option<u16> {
    let vk = match mac {
        // Letters (KeyA=0 .. KeyZ=6 scattered; alphabetical Carbon layout).
        0 => 0x41, 1 => 0x53, 2 => 0x44, 3 => 0x46, 4 => 0x48, 5 => 0x47,
        6 => 0x5A, 7 => 0x58, 8 => 0x43, 9 => 0x56, 11 => 0x42, 12 => 0x51,
        13 => 0x57, 14 => 0x45, 15 => 0x52, 16 => 0x59, 17 => 0x54,
        31 => 0x4F, 32 => 0x55, 34 => 0x49, 35 => 0x50, 37 => 0x4C,
        38 => 0x4A, 40 => 0x4B, 45 => 0x4E, 46 => 0x4D,
        // Digits.
        18 => 0x31, 19 => 0x32, 20 => 0x33, 21 => 0x34, 22 => 0x36,
        23 => 0x35, 25 => 0x39, 26 => 0x37, 28 => 0x38, 29 => 0x30,
        // Punctuation.
        24 => 0xBB,  // Equal
        27 => 0xBD,  // Minus
        30 => 0xDD,  // BracketRight
        33 => 0xDB,  // BracketLeft
        39 => 0xDE,  // Quote
        41 => 0xBA,  // Semicolon
        42 => 0xDC,  // Backslash
        43 => 0xBC,  // Comma
        44 => 0xBF,  // Slash
        47 => 0xBE,  // Period
        50 => 0xC0,  // Backquote
        // Control keys.
        36 => 0x0D,  // Enter
        48 => 0x09,  // Tab
        49 => 0x20,  // Space
        51 => 0x08,  // Backspace
        53 => 0x1B,  // Escape
        57 => 0x14,  // CapsLock
        // Navigation block.
        114 => 0x2D, // Insert / Help
        115 => 0x24, // Home
        116 => 0x21, // PageUp
        117 => 0x2E, // Forward delete
        119 => 0x23, // End
        121 => 0x22, // PageDown
        123 => 0x25, // Left
        124 => 0x27, // Right
        125 => 0x28, // Down
        126 => 0x26, // Up
        // Function keys.
        96 => 0x74, 97 => 0x75, 98 => 0x76, 99 => 0x72, 100 => 0x77,
        101 => 0x78, 103 => 0x7A, 109 => 0x79, 111 => 0x7B, 118 => 0x73,
        120 => 0x71, 122 => 0x70,
        // F13..F19 (only reachable via injection; see the inverse-table note).
        105 => 0x7C, 106 => 0x7D, 107 => 0x7E, 113 => 0x7F, 64 => 0x80,
        79 => 0x81, 80 => 0x82,
        // Keypad.
        65 => 0x6E, // Decimal
        67 => 0x6A, // Multiply
        69 => 0x6B, // Add
        71 => 0x0C, // Clear
        75 => 0x6F, // Divide
        76 => 0x0D, // Keypad Enter
        78 => 0x6D, // Subtract
        81 => 0x0C, // Keypad Equals (no distinct VK; Clear's slot is closest but unused)
        82 => 0x60, 83 => 0x61, 84 => 0x62, 85 => 0x63, 86 => 0x64,
        87 => 0x65, 88 => 0x66, 89 => 0x67, 90 => 0x68, 91 => 0x69,
        // Modifiers.
        55 => 0x5B, // Command -> Left Windows
        56 => 0x10, // Shift
        58 => 0x12, // Option -> Alt
        59 => 0x11, // Control
        _ => return None,
    };
    Some(vk)
}

/// Inverse: Windows VK -> Carbon keyCode, restricted to the unambiguous
/// typing keys and plain modifiers (listeners never match anything else).
pub fn mac_from_vk(vk: u16) -> Option<u8> {
    let mac = match vk {
        0x41 => 0, 0x53 => 1, 0x44 => 2, 0x46 => 3, 0x48 => 4, 0x47 => 5,
        0x5A => 6, 0x58 => 7, 0x43 => 8, 0x56 => 9, 0x42 => 11, 0x51 => 12,
        0x57 => 13, 0x45 => 14, 0x52 => 15, 0x59 => 16, 0x54 => 17,
        0x4F => 31, 0x55 => 32, 0x49 => 34, 0x50 => 35, 0x4C => 37,
        0x4A => 38, 0x4B => 40, 0x4E => 45, 0x4D => 46,
        0x31 => 18, 0x32 => 19, 0x33 => 20, 0x34 => 21, 0x36 => 22,
        0x35 => 23, 0x39 => 25, 0x37 => 26, 0x38 => 28, 0x30 => 29,
        0xBB => 24, 0xBD => 27, 0xDD => 30, 0xDB => 33, 0xDE => 39,
        0xBA => 41, 0xDC => 42, 0xBC => 43, 0xBF => 44, 0xBE => 47, 0xC0 => 50,
        0x0D => 36, 0x09 => 48, 0x20 => 49, 0x08 => 51, 0x1B => 53,
        0x2D => 114, 0x24 => 115, 0x21 => 116, 0x2E => 117, 0x23 => 119,
        0x22 => 121, 0x25 => 123, 0x27 => 124, 0x28 => 125, 0x26 => 126,
        0x72 => 99, 0x73 => 118, 0x74 => 96, 0x75 => 97, 0x76 => 98,
        0x77 => 100, 0x78 => 101, 0x7A => 103, 0x79 => 109, 0x7B => 111,
        0x70 => 122, 0x71 => 120,
        0x5B => 55, 0x10 => 56, 0x12 => 58, 0x11 => 59,
        _ => return None,
    };
    Some(mac)
}

/// Carbon codes of F1..F12, mirroring AppHost.functionKeyCodes.
pub const FUNCTION_KEY_NAMES: &[(u8, &str)] = &[
    (122, "F1"), (120, "F2"), (99, "F3"), (118, "F4"), (96, "F5"),
    (97, "F6"), (98, "F7"), (100, "F8"), (101, "F9"), (109, "F10"),
    (103, "F11"), (111, "F12"),
];

pub fn function_key_name(mac: u8) -> Option<&'static str> {
    FUNCTION_KEY_NAMES.iter().find(|(code, _)| *code == mac).map(|(_, name)| *name)
}

/// Parses a modifier name list the way KeyHooks.flags does (unknown names are
/// rejected). Returns Windows VKs for injection; `fn` has no Windows event and
/// maps to None so callers can fail closed where it matters.
pub fn modifier_vks(name: &str) -> Option<Option<u16>> {
    match name.to_ascii_lowercase().as_str() {
        "command" | "cmd" | "super" => Some(Some(0x5B)),
        "commandorcontrol" | "cmdorctrl" => Some(Some(0x11)),
        "control" | "ctrl" => Some(Some(0x11)),
        "shift" => Some(Some(0x10)),
        "option" | "alt" => Some(Some(0x12)),
        "fn" => Some(None),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_table_covers_trigger_keys() {
        // Every keyCode from shared/trigger-keys.json must map.
        for mac in trigger_key_codes() {
            assert!(vk_from_mac(*mac).is_some(), "mac {} unmapped", mac);
        }
    }

    #[test]
    fn forward_and_inverse_agree_on_typing_keys() {
        // Round trips must hold exactly for the unambiguous set: the
        // trigger-keys allowlist plus plain modifiers. Keypad Enter shares
        // VK_RETURN with Enter, and F13+ collide with the numpad block, so
        // those forwards are injection-only by design.
        let mut canonical: Vec<u8> = vec![55, 56, 58, 59];
        canonical.extend_from_slice(trigger_key_codes());
        for mac in canonical {
            let vk = vk_from_mac(mac).expect("canonical key maps");
            assert_eq!(mac_from_vk(vk), Some(mac), "mac {} -> vk {vk:#x} round trip", mac);
        }
    }

    fn trigger_key_codes() -> &'static [u8] {
        &[
            49, 0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12,
            15, 1, 17, 32, 9, 13, 7, 16, 6, 29, 18, 19, 20, 21, 23, 22, 26,
            28, 25, 50, 27, 24, 33, 30, 42, 41, 39, 43, 47, 44, 53, 48, 36,
            51, 117, 114, 116, 121, 115, 119, 126, 125, 123, 124,
            122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111,
        ]
    }

    #[test]
    fn inverse_rejects_keypad_and_f13_plus() {
        assert!(mac_from_vk(0x69).is_none()); // VK_NUMPAD9
        assert!(mac_from_vk(0x7C).is_none()); // VK_F13
    }

    #[test]
    fn function_keys_have_names() {
        assert_eq!(function_key_name(122), Some("F1"));
        assert_eq!(function_key_name(111), Some("F12"));
        assert_eq!(function_key_name(49), None);
    }

    #[test]
    fn modifier_names_match_mac_shortcut_vocabulary() {
        assert_eq!(modifier_vks("Control"), Some(Some(0x11)));
        assert_eq!(modifier_vks("option"), Some(Some(0x12)));
        assert_eq!(modifier_vks("cmd"), Some(Some(0x5B)));
        assert_eq!(modifier_vks("fn"), Some(None));
        assert_eq!(modifier_vks("hyper"), None);
    }

    #[test]
    fn codex_slot_and_trigger_codes_map() {
        for mac in [18, 19, 20, 21, 23, 22, 45] {
            assert!(vk_from_mac(mac).is_some(), "slot key {}", mac);
        }
        // Space (the default lighting trigger).
        assert_eq!(vk_from_mac(49), Some(0x20));
    }
}
