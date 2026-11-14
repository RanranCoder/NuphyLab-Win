// NuPhy S4 protocol: 64-byte HID frames, XOR session key, response routing.
// Ported function-for-function from native/HIDBridge.swift's S4 enum; the
// self-test compares behavior against the Swift implementation's own checks.
use crate::error::{failure, Result};

pub const REPORT_SIZE: usize = 64;
// Air75 V3 D1 returns one byte. The following zero is unencrypted padding,
// not a high byte: decoding it with key 0x80 turned 104 into 32872.
pub const LED_COUNT_LENGTH: usize = 1;
pub const ALLOWED_COMMANDS: [u8; 10] = [0xa0, 0xa1, 0xb1, 0xb2, 0xb3, 0xd1, 0xd2, 0xd5, 0xd6, 0xd8];

pub fn state_length(firmware: &[u8]) -> Option<usize> {
    if firmware.len() != 8 || firmware[3] != 0xaa || firmware[2] != 1 || firmware[4] != 6 || firmware[5] != 0 {
        return None;
    }
    Some(if firmware[1] > 0 || firmware[0] >= 17 { 22 } else { 17 })
}

pub fn supports_per_key(firmware: &[u8]) -> bool {
    state_length(firmware).is_some() && (firmware[1] > 0 || firmware[0] >= 16)
}

pub fn rendered_custom_byte(value: u8, index: usize, firmware: &[u8]) -> u8 {
    if state_length(firmware) != Some(22) {
        return value;
    }
    // 1.0.23.6's palette renderer uses HSV value capped at 200 (0xce80). The
    // unlabelled final LED additionally scales green/blue.
    let scaled = value as i32 * 200 / 255;
    if index >= 250 {
        (scaled * 150 / 255) as u8
    } else {
        scaled as u8
    }
}

pub fn rendered_custom_colors(colors: &[u8], firmware: &[u8]) -> Vec<u8> {
    colors.iter().enumerate().map(|(i, &v)| rendered_custom_byte(v, i, firmware)).collect()
}

pub fn palette_from_rendered(colors: &[u8], firmware: &[u8], high: bool) -> Result<Vec<u8>> {
    colors
        .iter()
        .enumerate()
        .map(|(index, &value)| {
            // D2 is quantized: several raw inputs render identically. `low`
            // takes the first candidate (palette capture); `high` the last,
            // which is the other end of the identical-output interval.
            let mut candidates = (0..=255u32).filter(|&input| rendered_custom_byte(input as u8, index, firmware) == value);
            let chosen = if high { candidates.last() } else { candidates.next() };
            chosen
                .map(|input| input as u8)
                .ok_or_else(|| failure("CUSTOM_BACKUP_LOSSY", "Cannot reconstruct the keyboard's custom palette; original state retained."))
        })
        .collect()
}

pub fn valid_geometry(value: &[u8]) -> bool {
    value.len() == 8 && value[0] <= 1 && value[1..5] == [8, 4, 6, 16]
        && (value[5] & 0x80 == 0 || value[5] & 0x7f == 2)
}

pub fn valid_state(state: &[u8], firmware: &[u8]) -> bool {
    let Some(length) = state_length(firmware) else { return false };
    if state.len() != length { return false; }
    state[0] <= (if length == 22 { 22 } else { 21 })
        && state[1] <= 100 && state[2] <= 4 && state[3] <= 3 && state[4] <= 1
        && state[9] <= 4 && state[10] <= 100 && state[11] <= 4 && state[12] <= 1
        && (length == 17 || (state[17] <= 7 && state[21] <= 4))
}

pub fn readable_state(state: &[u8], firmware: &[u8]) -> bool {
    if state_length(firmware) != Some(22) || state.len() != 22 {
        return valid_state(state, firmware);
    }
    // NuPhyIO clamps the *read* shape-speed byte; some 1.0.23.6 boards return
    // e.g. 94 here. That is not a different packet schema. Retain the raw byte
    // in the journal, but never send it through the setter.
    let mut checked = state.to_vec();
    checked[21] = 0;
    valid_state(&checked, firmware)
}

pub fn state_write_payload(target: &[u8], current: &[u8], firmware: &[u8]) -> Result<Vec<u8>> {
    if !readable_state(target, firmware) || !readable_state(current, firmware) {
        return Err(failure("INVALID_STATE", "Refusing an invalid lighting configuration."));
    }
    // Official D6 supports contiguous field writes. Keep the proven complete
    // main/side prefix (including RGB triplets), and leave the five preset
    // fields untouched when this operation doesn't own them.
    if target.len() == 22 && target[target.len() - 5..] == current[current.len() - 5..] {
        return Ok(target[..17].to_vec());
    }
    if !valid_state(target, firmware) {
        return Err(failure("INVALID_STATE", "Cannot overwrite an unrecognized preset parameter; original backup retained."));
    }
    Ok(target.to_vec())
}

// 1.0.23.6 stores the main colour as byte HSV, with V scaled to 200. D5
// converts it back to RGB. Feeding that RGB straight back into D6 can change
// the hue, e.g. [64,0,14] -> [64,0,17]. Find an input whose readback is EXACTLY
// the saved colour instead of weakening restore verification.
pub fn main_color_readback(rgb: &[u8]) -> [u8; 3] {
    let (r, g, b) = (rgb[0] as i32, rgb[1] as i32, rgb[2] as i32);
    let (hi, lo) = (r.max(g).max(b), r.min(g).min(b));
    let delta = hi - lo;
    let value = ((hi * 200 / 255) * 255 + 100) / 200;
    if delta == 0 {
        return [value as u8; 3];
    }
    let saturation = (delta as f32 / hi as f32 * 255.0 + 0.5) as i32;
    let mut hue: f32;
    if hi == r {
        hue = (g - b) as f32 / delta as f32 * 43.0;
    } else if hi == g {
        hue = ((b - r) as f32 / delta as f32 + 2.0) * 43.0;
    } else {
        hue = ((r - g) as f32 / delta as f32 + 4.0) * 43.0;
    }
    if hue < 0.0 {
        hue += 255.0;
    }
    let h = (hue + 0.5) as i32 & 255;
    let region = h / 43;
    let remainder = (h - region * 43) * 6;
    let p = ((255 - saturation) * value + 127) / 255;
    let q = ((255 - saturation * remainder / 255) * value + 127) / 255;
    let t = ((255 - saturation * (255 - remainder) / 255) * value + 127) / 255;
    match region {
        0 => [value as u8, t as u8, p as u8],
        1 => [q as u8, value as u8, p as u8],
        2 => [p as u8, value as u8, t as u8],
        3 => [p as u8, q as u8, value as u8],
        4 => [t as u8, p as u8, value as u8],
        _ => [value as u8, p as u8, q as u8],
    }
}

pub fn exact_main_color_input(target: &[u8]) -> Result<[u8; 3]> {
    let target = [target[0], target[1], target[2]];
    if main_color_readback(&target) == target {
        return Ok(target);
    }
    // Bound the search to one hue quantization interval plus value rounding.
    // A colour outside the proven model fails closed, keeping its journal.
    for radius in [4i32, 8, 16] {
        for r in (target[0] as i32 - radius).max(0)..=(target[0] as i32 + radius).min(255) {
            for g in (target[1] as i32 - radius).max(0)..=(target[1] as i32 + radius).min(255) {
                for b in (target[2] as i32 - radius).max(0)..=(target[2] as i32 + radius).min(255) {
                    let input = [r as u8, g as u8, b as u8];
                    if main_color_readback(&input) == target {
                        return Ok(input);
                    }
                }
            }
        }
    }
    Err(failure(
        "RESTORE_COLOR_UNREPRESENTABLE",
        format!("Cannot exactly encode saved main colour {:?}; original backup retained.", target),
    ))
}

pub fn states_match(expected: &[u8], observed: &[u8], firmware: &[u8]) -> bool {
    let Some(length) = state_length(firmware) else { return false };
    if expected.len() != length || observed.len() != length {
        return false;
    }
    // Official Air75 V3 1.0.16.6: both D6 dispatch tables skip offsets 5 and
    // 13; D5 leaves these palette-index slots unpopulated. Every other byte
    // remains strict. 1.0.23.6 D5 mirrors shape at offset 5 but still never
    // populates offset 13.
    let unused_palette_slots = firmware[..6] == [0x10, 0, 1, 0xaa, 6, 0];
    let unused_side_palette = firmware[..6] == [23, 0, 1, 0xaa, 6, 0];
    expected.iter().enumerate().all(|(i, &e)| {
        (unused_palette_slots && (i == 5 || i == 13))
            || (unused_side_palette && i == 13)
            || e == observed[i]
    })
}

pub fn checksum(bytes: &[u8]) -> u8 {
    bytes[4..].iter().fold(0u8, |acc, &b| acc.wrapping_add(b))
}

pub fn state_mismatch_code(expected: &[u8], observed: &[u8]) -> String {
    // A disabled board master switch ACKs D6 but skips its setter. Only
    // classify after delayed readback fails, never from zero brightness alone.
    let wants_light = [17usize, 22].contains(&expected.len())
        && (expected[0] != 0 || expected[1] > 0 || expected[10] > 0);
    let stays_off = observed.len() == expected.len()
        && [17usize, 22].contains(&observed.len())
        && observed[0] == 0 && observed[1] == 0 && observed[10] == 0;
    if wants_light && stays_off { "LIGHTING_DISABLED".into() } else { "STATE_VERIFY_FAILED".into() }
}

pub fn restore_target(state: &[u8]) -> Vec<u8> {
    // D5 can expose mode 0 while the board's master switch is off, but D6 does
    // not store mode 0. Restore its visible off state using stock static mode
    // at zero brightness; all remaining fields are preserved.
    let mut target = state.to_vec();
    if [17usize, 22].contains(&state.len()) && state[0] == 0 && state[1] == 0 && state[10] == 0 {
        target[0] = 2;
    }
    target
}

pub fn frame(command: u8, length: usize, address: usize, payload: &[u8], handle: u8, key: u8) -> Result<[u8; REPORT_SIZE]> {
    let handle_ok = handle == 0 || (handle == 1 && [0xd5u8, 0xd6, 0xd8].contains(&command));
    if !ALLOWED_COMMANDS.contains(&command)
        || !(0..=56).contains(&length)
        || payload.len() > 56
        || address > 65535
        || (!payload.is_empty() && payload.len() != length)
        || !handle_ok
    {
        return Err(failure("INVALID_FRAME", "Command, length, or address is outside the bridge's allowlist."));
    }
    if command == 0xb3 {
        // No generic keymap writer: only standard F4 <-> factory Spotlight,
        // one aligned key in the Mac base layer.
        let ok = length == 2 && address < 192 && address % 2 == 0 && handle == 0
            && (payload == [0, 61] || payload == [126, 7]);
        if !ok {
            return Err(failure("INVALID_FRAME", "Only the journaled factory F4 lease is permitted."));
        }
    }
    let mut bytes = [0u8; REPORT_SIZE];
    bytes[0] = 0x55;
    bytes[1] = command;
    bytes[4] = length as u8 ^ key;
    bytes[5] = (address & 255) as u8 ^ key;
    bytes[6] = (address >> 8) as u8 ^ key;
    bytes[7] = handle ^ key; // D5/D6 use the board's active Mac (0) / Win (1) profile.
    for (i, &value) in payload.iter().enumerate() {
        bytes[8 + i] = value ^ key;
    }
    bytes[3] = checksum(&bytes);
    Ok(bytes)
}

pub fn validate(raw: &[u8], command: u8) -> Result<()> {
    if raw.len() != REPORT_SIZE || raw[0] != 0xaa || raw[1] != command || raw[2] != 0 {
        return Err(failure("INVALID_REPLY", "HID response marker, command or report length did not match."));
    }
    if raw[3] != checksum(raw) {
        return Err(failure("INVALID_CHECKSUM", "HID response checksum did not match."));
    }
    Ok(())
}

pub fn decode(raw: &[u8], command: u8, length: usize, address: usize, handle: u8, key: u8) -> Result<Vec<u8>> {
    validate(raw, command)?;
    let route = [length as u8, (address & 255) as u8, (address >> 8) as u8, handle];
    let raw_route = &raw[4..8];
    let xored: Vec<u8> = raw_route.iter().map(|&b| b ^ key).collect();
    if raw_route != route && xored != route {
        return Err(failure("ROUTE_MISMATCH", "HID response route did not match; another configurator may own the session."));
    }
    Ok(raw[8..8 + length].iter().map(|&b| b ^ key).collect())
}

pub fn identify(raw: &[u8]) -> Result<(u8, Vec<u8>)> {
    validate(raw, 0xa1)?;
    // The firmware-info marker is a constant AA in payload byte 3. Accept
    // either documented plaintext or XORed routing, without EE.
    let key = raw[11] ^ 0xaa;
    let info = decode(raw, 0xa1, 8, 0, 0, key)?;
    if info[3] != 0xaa {
        return Err(failure("FIRMWARE_REPLY", "Unrecognized firmware identification response."));
    }
    Ok((key, info))
}

pub fn firmware_version(info: &[u8]) -> String {
    if info.len() != 8 || info[3] != 0xaa {
        return "unknown".into();
    }
    // NuPhyIO S4 parseFirmwareVersion: bytes 2.1.0, then little-endian 4–5.
    format!("{}.{}.{}.{}", info[2], info[1], info[0], info[4] as u32 | (info[5] as u32) << 8)
}

pub fn decode_led_count(raw: &[u8], key: u8) -> Result<usize> {
    let payload = decode(raw, 0xd1, LED_COUNT_LENGTH, 0, 0, key)?;
    let count = payload[0] as usize;
    if count != 104 {
        return Err(failure("WRONG_LED_COUNT", format!("灯位数量校验失败：应为 104，实际为 {count}。已停止灯效写入。")));
    }
    Ok(count)
}

pub fn parse_color(value: &str) -> Result<[u8; 3]> {
    let s = value.strip_prefix('#').unwrap_or(value);
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(failure("INVALID_COLOR", "Colors must be six hexadecimal digits, for example #ff4080."));
    }
    let number = u32::from_str_radix(s, 16)
        .map_err(|_| failure("INVALID_COLOR", "Colors must be six hexadecimal digits, for example #ff4080."))?;
    Ok([((number >> 16) & 255) as u8, ((number >> 8) & 255) as u8, (number & 255) as u8])
}

pub fn parse_colors(value: Option<&Vec<String>>, count: usize) -> Result<Vec<u8>> {
    let Some(colors) = value else {
        return Err(failure("INVALID_COLORS", format!("colors must contain exactly {count} strings in #rrggbb form.")));
    };
    if colors.len() != count {
        return Err(failure("INVALID_COLORS", format!("colors must contain exactly {count} strings in #rrggbb form.")));
    }
    let mut bytes = Vec::with_capacity(count * 3);
    for color in colors {
        bytes.extend_from_slice(&parse_color(color)?);
    }
    Ok(bytes)
}

pub fn hex_colors(bytes: &[u8]) -> Vec<String> {
    bytes.chunks_exact(3).map(|c| format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])).collect()
}
