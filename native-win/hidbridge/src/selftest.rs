// Offline self-test, ported from HIDBridge.swift's selfTest() and
// testFirmwareUpgradeLifecycle(). It exercises framing, both routing variants,
// corruption rejection, XOR-key derivation, journal migration and the complete
// connect/arm/paint/restore lifecycle against a scripted board — no HID opens.
use crate::device::Air75Device;
use crate::error::{failure, Result};
use crate::journal::{LightingBackup, LightingJournals};
use crate::protocol::{self, REPORT_SIZE};
use serde_json::json;
use std::path::PathBuf;

struct Check {
    count: std::cell::Cell<usize>,
}

impl Check {
    fn new() -> Check {
        Check { count: std::cell::Cell::new(0) }
    }
    fn ok(&self, yes: bool, name: &str) -> Result<()> {
        if !yes {
            return Err(failure("SELF_TEST", name));
        }
        self.count.set(self.count.get() + 1);
        Ok(())
    }
}

fn temp_dir(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("{name}-{}-{unique}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("self-test temp dir");
    dir
}

pub fn run() -> Result<usize> {
    let check = Check::new();
    test_journals(&check)?;
    test_s4_frames(&check)?;
    test_state_validation(&check)?;
    test_identify(&check)?;
    test_led_count(&check)?;
    test_colors(&check)?;
    test_firmware_upgrade_lifecycle(&check)?;
    test_side_sync(&check)?;
    test_input_hooks(&check)?;
    test_typing(&check)?;
    Ok(check.count.get())
}

// Pure branches of typing.rs plus (when the assets are provided) a real
// catalog load and sample existence sweep. No hooks or audio devices run here.
fn test_typing(check: &Check) -> Result<()> {
    use crate::typing::{key_group, load_catalog, PressState, Preferences, KeyGroup};
    // Key groups mirror TypingKeyGroup.key.
    check.ok(key_group(0x20) == Some(KeyGroup::Space), "space maps to the space group")?;
    check.ok(key_group(0x0D) == Some(KeyGroup::Enter) && key_group(0x6C) == Some(KeyGroup::Enter), "Return and keypad enter sound")?;
    check.ok(key_group(0xA0) == Some(KeyGroup::ShiftLeft) && key_group(0xA1) == Some(KeyGroup::ShiftRight), "both physical shifts sound")?;
    check.ok(key_group(0x14) == Some(KeyGroup::Caps) && key_group(0x41) == Some(KeyGroup::Regular), "caps lock and letters sound")?;
    check.ok(
        [0x11u16, 0x12, 0x5B, 0x5C, 0x70, 0x1B, 0x2E].iter().all(|vk| key_group(*vk).is_none()),
        "Ctrl/Alt/Win, F keys, Escape and forward delete stay silent",
    )?;
    // Press dedup and repeat filtering.
    let mut presses = PressState::default();
    check.ok(presses.down(0x41, false) == Some(KeyGroup::Regular), "first press sounds")?;
    check.ok(presses.down(0x41, false).is_none() && presses.down(0x41, true).is_none(), "held and repeated downs stay silent")?;
    check.ok(presses.up(0x41) == Some(KeyGroup::Regular) && presses.up(0x41).is_none(), "release sounds once")?;
    // Patch validation boundaries.
    let prefs = Preferences::default();
    let ids: std::collections::HashSet<&str> = ["mahjong-crisp", "wood-bamboo"].into_iter().collect();
    check.ok(prefs.apply_patch(&json!({"volume": 100, "release": 0}), &ids).is_ok(), "volume/release accept 0..100")?;
    check.ok(prefs.apply_patch(&json!({"volume": 100.5}), &ids).is_err(), "volume above 100 rejected")?;
    check.ok(prefs.apply_patch(&json!({"enabled": 1}), &ids).is_err(), "non-boolean flags rejected")?;
    check.ok(prefs.apply_patch(&json!({"gains": {"caps": 150}}), &ids).is_ok(), "gains accept 150")?;
    check.ok(prefs.apply_patch(&json!({"gains": {"caps": 151}}), &ids).is_err(), "gains above 150 rejected")?;
    check.ok(prefs.apply_patch(&json!({"gains": {"regular": 100}}), &ids).is_err(), "regular has no gain entry")?;
    check.ok(prefs.apply_patch(&json!({"pack": "wood-bamboo"}), &ids).is_ok(), "known pack accepted")?;
    check.ok(prefs.apply_patch(&json!({"pack": "buckling"}), &ids).is_err(), "unknown pack rejected")?;
    check.ok(prefs.apply_patch(&json!({"favorites": ["mahjong-crisp", "mahjong-crisp"]}), &ids).is_err(), "duplicate favorites rejected")?;
    check.ok((prefs.master_gain() - (0.5f64.powf(1.7) * 0.65)).abs() < 1e-9, "master gain formula matches Swift")?;
    check.ok((prefs.gain(KeyGroup::Regular, true) - 0.7).abs() < 1e-9, "release gain formula matches Swift")?;
    // Real catalog when the assets are available; skipped otherwise so the
    // self-test also passes in bare build environments.
    let assets = std::env::var_os("AIR_STUDIO_TYPING_ASSETS")
        .map(std::path::PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty() && path.is_dir());
    if let Some(root) = assets {
        let (catalog, public) = load_catalog(&root)?;
        check.ok(catalog.packs.len() == 18, "typing catalog carries 18 packs")?;
        check.ok(public.get("assets").is_none(), "the asset list stays out of the status snapshot")?;
        let mut samples = 0usize;
        for pack in &catalog.packs {
            for group in crate::typing::KEY_GROUPS {
                for release in [false, true] {
                    for path in pack.paths(group, release) {
                        samples += 1;
                        check.ok(root.join("audio").join(&pack.id).join(&path).is_file(), "catalog sample exists on disk")?;
                    }
                }
            }
        }
        check.ok(samples > 18 * 8, "every pack resolves non-empty sample paths")?;
    }
    Ok(())
}

// Pure-validation branches of hooks.rs, porting the KeyHooks/TriggerHooks
// cases from HIDBridge.swift's selfTest(). No hooks or injection run offline.
fn test_input_hooks(check: &Check) -> Result<()> {
    use crate::hooks::{parse_flags, shortcut, trigger_should_fire, validate_key_monitor, validate_trigger, MOD_CONTROL, MOD_OPTION};
    // Trigger validation mirrors TriggerHooks.requestedCode.
    check.ok(validate_trigger(&json!({"enabled": true, "keyCode": 49}))? == Some(49), "space lighting trigger accepted")?;
    check.ok(validate_trigger(&json!({"enabled": false}))? == None, "lighting trigger disabled without key")?;
    check.ok(validate_trigger(&json!({})).is_err(), "lighting trigger requires enabled")?;
    check.ok(validate_trigger(&json!({"enabled": 1})).is_err(), "lighting trigger enabled must be boolean")?;
    check.ok(validate_trigger(&json!({"enabled": true, "keyCode": 10})).is_err(), "lighting trigger rejects key outside the allowlist")?;
    check.ok(validate_trigger(&json!({"enabled": true, "keyCode": 63})).is_err(), "fn cannot be a lighting trigger")?;
    check.ok(
        matches!(validate_trigger(&json!({"enabled": true, "keyCode": 49, "suppress": false})), Err(e) if e.code == "INVALID_TRIGGER_MONITOR"),
        "lighting trigger rejects suppress",
    )?;
    check.ok(
        matches!(validate_trigger(&json!({"enabled": true, "keyCode": 49, "modifiers": []})), Err(e) if e.code == "INVALID_TRIGGER_MONITOR"),
        "lighting trigger rejects modifiers",
    )?;
    // shouldTrigger predicate (Swift :2191-2193).
    check.ok(trigger_should_fire(true, false, 0, 49, 49), "selected trigger key matches")?;
    check.ok(!trigger_should_fire(true, true, 0, 49, 49), "held trigger does not repeat")?;
    check.ok(!trigger_should_fire(true, false, MOD_CONTROL, 49, 49), "trigger with modifier ignored")?;
    check.ok(!trigger_should_fire(false, false, 0, 49, 49), "trigger keyUp ignored")?;
    check.ok(!trigger_should_fire(true, false, 0, 49, 48), "other key ignored")?;
    // Key monitor validation mirrors KeyHooks.configure.
    check.ok(validate_key_monitor(&json!({"enabled": false}))? == None, "key monitor disabled")?;
    let defaults = validate_key_monitor(&json!({}))?;
    check.ok(defaults.as_ref().map(|b| b.len()) == Some(7), "key monitor default has seven bindings")?;
    check.ok(
        defaults.as_ref().map(|b| b.iter().filter(|x| matches!(x.action, crate::hooks::BindingAction::Slot(_))).count()) == Some(6),
        "default bindings contain six slots",
    )?;
    check.ok(
        matches!(validate_key_monitor(&json!({"bindings": []})), Err(e) if e.code == "INVALID_BINDINGS"),
        "empty key monitor binding list rejected",
    )?;
    let eight: Vec<_> = (0..8).map(|i| json!({"name": format!("s{i}"), "keyCode": 18, "modifiers": ["control", "option"]})).collect();
    check.ok(
        matches!(validate_key_monitor(&json!({"bindings": eight})), Err(e) if e.code == "INVALID_BINDINGS"),
        "more than seven bindings rejected",
    )?;
    check.ok(
        matches!(validate_key_monitor(&json!({"bindings": [{"name": "bare", "keyCode": 18}]})), Err(e) if e.code == "INVALID_BINDING"),
        "binding without the Control+Option chord rejected",
    )?;
    check.ok(
        matches!(validate_key_monitor(&json!({"bindings": [{"name": "wrong", "keyCode": 0, "modifiers": ["control", "option"]}]})), Err(e) if e.code == "INVALID_BINDING"),
        "binding outside the slot/newTask keys rejected",
    )?;
    let duplicate = vec![
        json!({"name": "a", "keyCode": 18, "modifiers": ["control", "option"]}),
        json!({"name": "b", "keyCode": 18, "modifiers": ["control", "option"]}),
    ];
    check.ok(
        matches!(validate_key_monitor(&json!({"bindings": duplicate})), Err(e) if e.code == "DUPLICATE_BINDING"),
        "duplicate slot binding rejected",
    )?;
    check.ok(
        matches!(parse_flags(Some(&json!("control"))), Err(e) if e.code == "INVALID_MODIFIERS"),
        "non-array modifiers rejected",
    )?;
    check.ok(
        matches!(parse_flags(Some(&json!(["hyper"]))), Err(e) if e.code == "INVALID_MODIFIERS"),
        "unknown modifier rejected",
    )?;
    check.ok(parse_flags(Some(&json!(["Control", "option"])))? == (MOD_CONTROL | MOD_OPTION), "modifier names parse")?;
    check.ok(parse_flags(None)? == 0, "absent modifiers parse as empty")?;
    // Shortcut validation (pure branches; nothing is injected).
    check.ok(
        matches!(shortcut(&json!({"phase": "wiggle", "keyCode": 18})), Err(e) if e.code == "INVALID_SHORTCUT"),
        "invalid shortcut phase rejected",
    )?;
    check.ok(
        matches!(shortcut(&json!({"keyCode": 200})), Err(e) if e.code == "INVALID_SHORTCUT"),
        "shortcut keyCode out of range rejected",
    )?;
    check.ok(
        matches!(shortcut(&json!({"keyCode": 18, "target": "other.app"})), Err(e) if e.code == "INVALID_TARGET"),
        "shortcut target restricted to Codex",
    )?;
    check.ok(
        matches!(shortcut(&json!({"keyCode": 18, "modifiers": ["fn"]})), Err(e) if e.code == "INVALID_SHORTCUT"),
        "fn modifier injection rejected on Windows",
    )?;
    check.ok(
        matches!(shortcut(&json!({"action": "task", "threadId": "../escape"})), Err(e) if e.code == "INVALID_TASK"),
        "task threadId charset enforced",
    )?;
    check.ok(
        matches!(shortcut(&json!({"action": "task"})), Err(e) if e.code == "INVALID_TASK"),
        "task without threadId rejected",
    )?;
    check.ok(
        matches!(shortcut(&json!({"keyCode": 18, "delivery": "globalHold"})), Err(e) if e.code == "INVALID_SHORTCUT"),
        "globalHold requires the down phase",
    )?;
    Ok(())
}

fn test_journals(check: &Check) -> Result<()> {
    let dir = temp_dir("nuphylab-profile-test");
    let journal = LightingJournals::new(dir.join("original-lighting.json"));
    let record = LightingBackup {
        created_at: "test".into(),
        serial: "unit-board".into(),
        firmware: vec![16, 0, 1, 170, 6, 0, 0, 0],
        state: vec![6, 50, 2, 0, 1, 0, 0, 5, 128, 4, 60, 2, 1, 0, 255, 0, 0],
        rendered_colors: vec![0; 312],
        custom_colors: None,
        lighting_profile: Some(1),
    };
    let win_data = serde_json::to_vec(&record.to_json()).unwrap();
    std::fs::write(&journal.legacy_url, &win_data).unwrap();
    journal.migrate_legacy("unit-board")?;
    check.ok(!journal.has_legacy() && journal.profiles() == vec![1], "Win backup migrates without creating a Mac recovery requirement")?;
    check.ok(std::fs::read(journal.url(1)).unwrap() == win_data, "migration preserves the original byte-for-byte")?;

    // A pre-profile release always addressed Mac (lightingProfile absent).
    let mut mac_record = record.clone();
    mac_record.lighting_profile = None;
    let mac_data = serde_json::to_vec(&mac_record.to_json()).unwrap();
    std::fs::write(&journal.legacy_url, &mac_data).unwrap();
    journal.migrate_legacy("unit-board")?;
    check.ok(journal.profiles() == vec![0, 1], "legacy Mac and Win journals coexist")?;
    check.ok(std::fs::read(journal.url(1)).unwrap() == win_data, "Mac migration cannot replace the Win original")?;
    check.ok(
        journal.read(&journal.url(0), "unit-board")?.lighting_profile.is_none(),
        "old profile-less backup stays readable",
    )?;
    std::fs::write(&journal.legacy_url, &win_data).unwrap();
    match journal.migrate_legacy("unit-board") {
        Err(error) => check.ok(error.code == "BACKUP_MIGRATION_FAILED", "conflicting journals cannot overwrite originals")?,
        Ok(()) => return Err(failure("SELF_TEST", "overwrote a pending journal")),
    }
    check.ok(
        std::fs::read(&journal.legacy_url).unwrap() == win_data && std::fs::read(journal.url(1)).unwrap() == win_data,
        "both originals survive a migration conflict",
    )?;
    std::fs::remove_file(&journal.legacy_url).unwrap();
    std::fs::remove_file(journal.url(0)).unwrap();
    check.ok(journal.profiles() == vec![1], "completing one profile leaves the other profile pending")?;
    match journal.read(&journal.url(1), "different-board") {
        Err(error) => check.ok(error.code == "BACKUP_DEVICE_MISMATCH", "device identity still enforced")?,
        Ok(_) => return Err(failure("SELF_TEST", "accepted another device's journal")),
    }
    std::fs::write(&journal.legacy_url, b"broken").unwrap();
    match journal.migrate_legacy("unit-board") {
        Err(error) => check.ok(error.code == "BACKUP_READ_FAILED" && journal.has_legacy(), "corrupt original is preserved")?,
        Ok(()) => return Err(failure("SELF_TEST", "moved corrupt recovery data")),
    }
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

fn test_s4_frames(check: &Check) -> Result<()> {
    let frame = protocol::frame(0xd5, 17, 0, &[], 0, 0x5a)?;
    check.ok(frame.len() == REPORT_SIZE && frame[0] == 0x55 && frame[3] == protocol::checksum(&frame), "request checksum")?;
    check.ok(frame[4] == 17 ^ 0x5a && frame[7] == 0x5a && frame[8] == 0, "route XOR and zero padding")?;

    let lights_off: Vec<u8> = vec![0, 0, 3, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 255, 255, 255];
    let mut lights_on = lights_off.clone();
    lights_on[0] = 21;
    lights_on[1] = 100;
    lights_on[4] = 1;
    check.ok(protocol::state_mismatch_code(&lights_on, &lights_off) == "LIGHTING_DISABLED", "disabled master switch mismatch")?;
    check.ok(protocol::state_mismatch_code(&lights_on, &lights_on) == "STATE_VERIFY_FAILED", "ordinary mismatch keeps general error")?;
    check.ok(protocol::state_mismatch_code(&lights_off, &lights_off) == "STATE_VERIFY_FAILED", "restoring off does not demand hardware wake")?;
    check.ok(protocol::state_mismatch_code(&lights_on, &[]) == "STATE_VERIFY_FAILED", "truncated state cannot be classified as off")?;
    let off_target = protocol::restore_target(&lights_off);
    check.ok(off_target[0] == 2 && off_target[1..] == lights_off[1..], "unwritable mode zero restores a stock dark state")?;
    check.ok(protocol::restore_target(&lights_on) == lights_on, "normal state restores exactly")?;
    let mut side_on = lights_off.clone();
    side_on[10] = 60;
    check.ok(protocol::restore_target(&side_on) == side_on, "side lighting is not mistaken for a master-off state")?;

    let state: Vec<u8> = vec![2, 75, 2, 0, 0, 0, 0, 190, 174, 4, 60, 2, 1, 0, 255, 0, 0];
    let verified_firmware: Vec<u8> = vec![16, 0, 1, 170, 6, 0, 0, 0];
    let mut reserved_drift = state.clone();
    reserved_drift[5] = 81;
    reserved_drift[13] = 198;
    check.ok(protocol::states_match(&state, &reserved_drift, &verified_firmware), "unimplemented palette fields cannot fail restoration")?;
    check.ok(!protocol::states_match(&state, &reserved_drift, &[17, 0, 1, 170, 6, 0, 0, 0]), "other firmware keeps exact comparison")?;
    check.ok(!protocol::states_match(&state, &[], &verified_firmware), "truncated readback fails")?;
    for index in 0..state.len() {
        if index == 5 || index == 13 {
            continue;
        }
        let mut mismatch = reserved_drift.clone();
        mismatch[index] ^= 1;
        check.ok(!protocol::states_match(&state, &mismatch, &verified_firmware), "every implemented setting remains strict")?;
    }

    // Both reply routing forms, per profile, with corruption rejection.
    for profile in [0u8, 1] {
        for plaintext in [true, false] {
            let mut reply = protocol::frame(0xd5, 17, 0, &state, profile, 0x5a)?;
            check.ok(reply[7] == profile ^ 0x5a, "lighting route follows selected Mac/Win profile")?;
            reply[0] = 0xaa;
            if plaintext {
                reply[4..8].copy_from_slice(&[17, 0, 0, profile]);
                reply[3] = protocol::checksum(&reply);
            }
            check.ok(protocol::decode(&reply, 0xd5, 17, 0, profile, 0x5a)? == state, "both profile routes decode")?;
            match protocol::decode(&reply, 0xd5, 17, 0, 1 - profile, 0x5a) {
                Err(error) => check.ok(error.code == "ROUTE_MISMATCH", "wrong profile remains rejected")?,
                Ok(_) => return Err(failure("SELF_TEST", "wrong profile accepted")),
            }
        }
    }
    for plaintext in [true, false] {
        let mut reply = protocol::frame(0xd5, 17, 0, &state, 0, 0x5a)?;
        reply[0] = 0xaa;
        if plaintext {
            reply[4..8].copy_from_slice(&[17, 0, 0, 0]);
            reply[3] = protocol::checksum(&reply);
        }
        check.ok(protocol::decode(&reply, 0xd5, 17, 0, 0, 0x5a)? == state, "both reply routing forms")?;
        reply[10] ^= 1;
        match protocol::decode(&reply, 0xd5, 17, 0, 0, 0x5a) {
            Err(error) => check.ok(error.code == "INVALID_CHECKSUM", "corruption rejected")?,
            Ok(_) => return Err(failure("SELF_TEST", "corruption accepted")),
        }
    }

    // Legacy backup field defaults: an absent lightingProfile belongs to Mac 0.
    let old_backup = LightingBackup {
        created_at: "test".into(),
        serial: String::new(),
        firmware: vec![16, 0, 1, 170, 6, 0, 0, 0],
        state: state.clone(),
        rendered_colors: vec![0; 312],
        custom_colors: None,
        lighting_profile: None,
    };
    let decoded_old = LightingBackup::from_json(&old_backup.to_json())?;
    check.ok(decoded_old.lighting_profile.unwrap_or(0) == 0, "legacy backup belongs to old fixed Mac route")?;
    let mut win_backup = old_backup.clone();
    win_backup.lighting_profile = Some(1);
    check.ok(LightingBackup::from_json(&win_backup.to_json())?.lighting_profile == Some(1), "Win profile survives backup persistence")?;

    // Extended 22-byte schema carries the same routing rules per profile.
    let shape_state: Vec<u8> = vec![22, 75, 2, 0, 0, 3, 0, 190, 174, 4, 60, 2, 1, 0, 255, 0, 0, 3, 24, 48, 96, 2];
    for profile in [0u8, 1] {
        let mut reply = protocol::frame(0xd5, 22, 0, &shape_state, profile, 0x80)?;
        reply[0] = 0xaa;
        check.ok(protocol::decode(&reply, 0xd5, 22, 0, profile, 0x80)? == shape_state, "22-byte state including all shape fields roundtrips")?;
        match protocol::decode(&reply, 0xd5, 17, 0, profile, 0x80) {
            Err(error) => check.ok(error.code == "ROUTE_MISMATCH", "wrong state length rejected")?,
            Ok(_) => return Err(failure("SELF_TEST", "truncated schema route accepted")),
        }
        let frame = protocol::frame(0xd8, 4, 0, &[0, 8, 16, 24], profile, 0x80)?;
        check.ok(frame[7] == profile ^ 0x80, "custom colors use the active profile")?;
    }
    // B3 cannot write arbitrary keys or layers.
    for (address, payload) in [(1usize, vec![0u8, 61]), (196, vec![0, 61]), (8, vec![0, 4]), (8, vec![])] {
        match protocol::frame(0xb3, 2, address, &payload, 0, 0) {
            Err(error) => check.ok(error.code == "INVALID_FRAME", "B3 cannot write arbitrary keys/layers")?,
            Ok(_) => return Err(failure("SELF_TEST", "arbitrary B3 accepted")),
        }
    }
    match protocol::frame(0xef, 0, 0, &[], 0, 0) {
        Err(error) => check.ok(error.code == "INVALID_FRAME", "IAP forbidden")?,
        Ok(_) => return Err(failure("SELF_TEST", "IAP accepted")),
    }
    Ok(())
}

fn test_state_validation(check: &Check) -> Result<()> {
    let verified_firmware: Vec<u8> = vec![16, 0, 1, 170, 6, 0, 0, 0];
    let latest_firmware: Vec<u8> = vec![23, 0, 1, 170, 6, 0, 0, 0];
    let future_firmware: Vec<u8> = vec![99, 0, 1, 170, 6, 0, 0, 0];
    let state: Vec<u8> = vec![2, 75, 2, 0, 0, 0, 0, 190, 174, 4, 60, 2, 1, 0, 255, 0, 0];
    let shape_state: Vec<u8> = vec![22, 75, 2, 0, 0, 3, 0, 190, 174, 4, 60, 2, 1, 0, 255, 0, 0, 3, 24, 48, 96, 2];

    // Quantized HSV colour round-trip: every sampled colour has an exact input.
    check.ok(protocol::main_color_readback(&[64, 0, 14]) == [64, 0, 17], "captured firmware HSV round-trip drift")?;
    let direct = protocol::exact_main_color_input(&[64, 0, 14])?;
    check.ok(protocol::main_color_readback(&direct) == [64, 0, 14], "captured original colour restores exactly")?;
    for r in (0..=255u16).step_by(17) {
        for g in (0..=255u16).step_by(17) {
            for b in (0..=255u16).step_by(17) {
                let observed = protocol::main_color_readback(&[r as u8, g as u8, b as u8]);
                let input = protocol::exact_main_color_input(&observed)?;
                check.ok(protocol::main_color_readback(&input) == observed, "quantized colour has an exact restore input")?;
            }
        }
    }

    check.ok(protocol::state_length(&verified_firmware) == Some(17) && protocol::state_length(&latest_firmware) == Some(22), "official old and extended state schemas")?;
    check.ok(protocol::state_length(&[17, 0, 1, 170, 6, 0, 0, 0]) == Some(22), "shape schema begins at official 1.0.17.6 boundary")?;
    check.ok(protocol::state_length(&future_firmware) == Some(22) && protocol::supports_per_key(&future_firmware), "compatible future minor releases are not version-locked")?;
    check.ok(!protocol::supports_per_key(&[15, 0, 1, 170, 6, 0, 0, 0]), "pre-custom firmware remains read-only")?;
    for invalid in [vec![], vec![23, 0, 2, 170, 6, 0, 0, 0], vec![23, 0, 1, 0, 6, 0, 0, 0], vec![23, 0, 1, 170, 7, 0, 0, 0]] {
        check.ok(protocol::state_length(&invalid).is_none() && !protocol::supports_per_key(&invalid), "unknown protocol families fail closed")?;
    }
    check.ok(protocol::valid_state(&shape_state, &latest_firmware), "complete preset configuration accepted")?;
    check.ok(!protocol::valid_state(&state, &latest_firmware), "new firmware cannot silently use a truncated legacy backup")?;
    check.ok(!protocol::valid_state(&shape_state, &verified_firmware), "extended state cannot be sent to old firmware")?;
    for (offset, value) in [(0usize, 23u8), (1, 101), (2, 5), (3, 4), (4, 2), (9, 5), (10, 101), (11, 5), (12, 2), (17, 8), (21, 5)] {
        let mut invalid = shape_state.clone();
        invalid[offset] = value;
        check.ok(!protocol::valid_state(&invalid, &latest_firmware), "out-of-schema field rejected")?;
        check.ok(protocol::readable_state(&invalid, &latest_firmware) == (offset == 21), "read tolerance is restricted to preset speed")?;
    }
    for speed in 0..=255u8 {
        let mut raw_state = shape_state.clone();
        raw_state[21] = speed;
        let mut target = raw_state.clone();
        target[0] = 21;
        target[1] = 100;
        check.ok(protocol::readable_state(&raw_state, &latest_firmware), "readback preset speed retained without normalization")?;
        check.ok(
            protocol::state_write_payload(&target, &raw_state, &latest_firmware)? == target[..17].to_vec(),
            "untouched preset never enters D6 payload",
        )?;
    }
    let mut raw_preset = shape_state.clone();
    raw_preset[21] = 94;
    match protocol::state_write_payload(&raw_preset, &shape_state, &latest_firmware) {
        Err(error) => check.ok(error.code == "INVALID_STATE", "read tolerance cannot enable invalid preset writes")?,
        Ok(_) => return Err(failure("SELF_TEST", "out-of-range preset setter accepted")),
    }
    check.ok(protocol::state_write_payload(&shape_state, &raw_preset, &latest_firmware)? == shape_state, "valid changed presets retain full restoration")?;
    check.ok(protocol::state_write_payload(&state, &state, &verified_firmware)? == state, "legacy full-state writes unchanged")?;
    let mut changed_raw = raw_preset.clone();
    changed_raw[21] = 95;
    check.ok(!protocol::states_match(&raw_preset, &changed_raw, &latest_firmware), "read tolerance never skips restoration equality")?;
    for offset in 0..shape_state.len() {
        let mut changed = shape_state.clone();
        changed[offset] ^= 1;
        check.ok(protocol::states_match(&shape_state, &changed, &latest_firmware) == (offset == 13), "latest schema verifies every implemented byte")?;
        check.ok(!protocol::states_match(&shape_state, &changed, &future_firmware), "future releases get no inferred reserved-byte exceptions")?;
    }
    let geometry: Vec<u8> = vec![0, 8, 4, 6, 16, 2, 0, 0];
    check.ok(protocol::valid_geometry(&geometry), "ANSI S4 geometry accepted")?;
    for offset in 0..=4usize {
        let mut invalid = geometry.clone();
        invalid[offset] = 99;
        check.ok(!protocol::valid_geometry(&invalid), "changed model or matrix rejected")?;
    }
    // Off restoration retains preset fields; the extended schema still
    // recognizes a physically switched-off board.
    let mut extended_off = [lights_off_state(), vec![3, 24, 48, 96, 2]].concat();
    let restored_off = protocol::restore_target(&extended_off);
    check.ok(restored_off[0] == 2 && restored_off[restored_off.len() - 5..] == extended_off[extended_off.len() - 5..], "off restoration retains preset fields")?;
    extended_off[0] = 21;
    extended_off[1] = 100;
    check.ok(
        protocol::state_mismatch_code(&extended_off, &[lights_off_state(), vec![3, 24, 48, 96, 2]].concat()) == "LIGHTING_DISABLED",
        "new schema still recognizes physical master off",
    )?;
    Ok(())
}

fn lights_off_state() -> Vec<u8> {
    vec![0, 0, 3, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 255, 255, 255]
}

fn test_identify(check: &Check) -> Result<()> {
    let mut identity = protocol::frame(0xa1, 8, 0, &[0x10, 0, 1, 0xaa, 6, 0, 0, 0], 0, 0x37)?;
    identity[0] = 0xaa;
    let (key, info) = protocol::identify(&identity)?;
    check.ok(key == 0x37 && info[0] == 0x10, "A1 key derivation without EE")?;
    check.ok(protocol::firmware_version(&info) == "1.0.16.6", "current version from S4 fields")?;
    check.ok(protocol::firmware_version(&[14, 0, 1, 170, 6, 0, 0, 0]) == "1.0.14.6", "older version is readable")?;
    check.ok(protocol::firmware_version(&[10, 0, 0, 170, 6, 0, 0, 0]) == "0.0.10.6", "legacy zero-major version")?;
    check.ok(protocol::firmware_version(&[17, 2, 1, 170, 1, 2, 88, 99]) == "1.2.17.513", "16-bit fourth component and reserved tail")?;
    check.ok(protocol::firmware_version(&[16, 0, 1, 0, 6, 0, 0, 0]) == "unknown", "invalid version marker")?;
    check.ok(protocol::firmware_version(&[16, 0, 1, 170, 6]) == "unknown", "truncated version rejected")?;
    let mut legacy_identity = protocol::frame(0xa1, 8, 0, &[10, 0, 0, 170, 6, 0, 0, 0], 0, 0x80)?;
    legacy_identity[0] = 0xaa;
    let (_, legacy_info) = protocol::identify(&legacy_identity)?;
    check.ok(protocol::firmware_version(&legacy_info) == "0.0.10.6", "identify legacy firmware read-only")?;
    Ok(())
}

fn test_led_count(check: &Check) -> Result<()> {
    // Captured from an Air75 V3 with the existing session key 0x80.
    // D1 encrypts only E8 (68 XOR 80); all bytes after it remain zero.
    let captured_count: [u8; REPORT_SIZE] = {
        let mut frame = [0u8; REPORT_SIZE];
        frame[0] = 0xaa;
        frame[1] = 0xd1;
        frame[4] = 0x81;
        frame[5] = 0x80;
        frame[6] = 0x80;
        frame[7] = 0x80;
        frame[8] = 0xe8;
        frame[3] = protocol::checksum(&frame);
        frame
    };
    check.ok(protocol::decode_led_count(&captured_count, 0x80)? == 104, "captured nonzero-key LED count")?;
    for session_key in 0..=255u16 {
        for encrypted_route in [false, true] {
            // Wire fixture, constructed independently from the request encoder.
            let mut reply = [0u8; REPORT_SIZE];
            reply[0] = 0xaa;
            reply[1] = 0xd1;
            reply[4] = 1; // one-byte D1 payload length
            reply[8] = 104 ^ session_key as u8;
            if encrypted_route {
                for byte in reply.iter_mut().skip(4).take(4) {
                    *byte ^= session_key as u8;
                }
            }
            reply[3] = protocol::checksum(&reply);
            check.ok(protocol::decode_led_count(&reply, session_key as u8)? == 104, "one-byte D1 across every key and route")?;
        }
    }
    for wrong_count in [0u8, 83, 103, 105, 255] {
        let mut reply = captured_count;
        reply[8] = wrong_count ^ 0x80;
        reply[3] = protocol::checksum(&reply);
        match protocol::decode_led_count(&reply, 0x80) {
            Err(error) => check.ok(error.code == "WRONG_LED_COUNT", "unexpected LED count remains rejected")?,
            Ok(_) => return Err(failure("SELF_TEST", "wrong LED count accepted")),
        }
    }
    // Reproduce the old two-byte padding-as-count bug as a rejection.
    let mut stale_count = captured_count;
    stale_count[4] = 2 ^ 0x80;
    stale_count[3] = protocol::checksum(&stale_count);
    let legacy_payload = protocol::decode(&stale_count, 0xd1, 2, 0, 0, 0x80)?;
    check.ok(legacy_payload[0] as usize + 256 * legacy_payload[1] as usize == 32872, "reproduce old padding-as-count bug")?;
    match protocol::decode_led_count(&stale_count, 0x80) {
        Err(error) => check.ok(error.code == "ROUTE_MISMATCH", "stale D1 response remains rejected")?,
        Ok(_) => return Err(failure("SELF_TEST", "stale two-byte D1 route accepted")),
    }
    let mut corrupt_count = captured_count;
    corrupt_count[8] ^= 1;
    match protocol::decode_led_count(&corrupt_count, 0x80) {
        Err(error) => check.ok(error.code == "INVALID_CHECKSUM", "D1 checksum remains enforced")?,
        Ok(_) => return Err(failure("SELF_TEST", "corrupt D1 accepted")),
    }
    Ok(())
}

fn test_colors(check: &Check) -> Result<()> {
    check.ok(protocol::parse_colors(Some(&vec!["#ff4080".to_string(); 84]), 84)?.len() == 252, "84-key input")?;
    check.ok(protocol::hex_colors(&[255, 64, 128]) == vec!["#ff4080".to_string()], "RGB formatting")?;
    match protocol::parse_colors(Some(&vec!["#ffffff".to_string()]), 84) {
        Err(error) => check.ok(error.code == "INVALID_COLORS", "short frame rejected")?,
        Ok(_) => return Err(failure("SELF_TEST", "short frame accepted")),
    }
    Ok(())
}

/// The side sync writes the side zone through the plain D6 echo while a session
/// owns lighting, skips redundant writes, and never blocks later restoration.
fn test_side_sync(check: &Check) -> Result<()> {
    use std::cell::RefCell;
    struct Board {
        state: Vec<u8>,
        rgb: Vec<u8>,
        writes: usize,
    }
    impl Board {
        fn exchange(&mut self, frame: &[u8; REPORT_SIZE]) -> Result<[u8; REPORT_SIZE]> {
            let command = frame[1];
            let length = frame[4] as usize;
            let address = frame[5] as usize | (frame[6] as usize) << 8;
            let payload: Vec<u8> = match command {
                0xa1 => vec![23, 0, 1, 0xaa, 6, 0, 0, 0],
                0xa0 => vec![0, 8, 4, 6, 16, 2, 0, 0],
                0xd1 => vec![104],
                0xd5 => self.state.clone(),
                0xd2 => self.rgb[address..address + length].to_vec(),
                0xd6 => {
                    self.writes += 1;
                    let payload = frame[8..8 + length].to_vec();
                    self.state[address..address + length].copy_from_slice(&payload);
                    if address <= 6 && address + length >= 9 {
                        let quantized = protocol::main_color_readback(&self.state[6..9]);
                        self.state[6..9].copy_from_slice(&quantized);
                    }
                    payload
                }
                0xd8 => {
                    self.writes += 1;
                    let payload = frame[8..8 + length].to_vec();
                    for index in (0..length).step_by(4) {
                        let start = payload[index] as usize * 3;
                        self.rgb[start..start + 3].copy_from_slice(&payload[index + 1..index + 4]);
                    }
                    payload
                }
                _ => return Err(failure("SELF_TEST", "Unexpected side-sync command")),
            };
            if payload.len() != length {
                return Err(failure("SELF_TEST", "Incorrect side-sync response length"));
            }
            let mut reply = [0u8; REPORT_SIZE];
            reply[0] = 0xaa;
            reply[1] = command;
            reply[4..8].copy_from_slice(&frame[4..8]);
            reply[8..8 + length].copy_from_slice(&payload);
            reply[3] = protocol::checksum(&reply);
            Ok(reply)
        }
    }
    let dir = temp_dir("nuphylab-side-sync-test");
    let original = vec![6, 100, 2, 0, 1, 0, 0, 9, 255, 4, 60, 4, 1, 0, 255, 0, 0, 0, 80, 80, 80, 0];
    let board = std::rc::Rc::new(RefCell::new(Board {
        state: original.clone(),
        rgb: vec![0; 312],
        writes: 0,
    }));
    let journals = LightingJournals::new(dir.join("original.json"));
    let exchange_board = std::rc::Rc::clone(&board);
    let device = RefCell::new(Air75Device::new_test(
        journals.legacy_url.clone(),
        Box::new(move |frame| exchange_board.borrow_mut().exchange(frame)),
    ));
    {
        let mut device = device.borrow_mut();
        device.connect()?;
        match device.sync_side([48, 96, 255], 40) {
            Err(error) => check.ok(error.code == "NOT_ARMED", "side sync refuses an unarmed session")?,
            Ok(_) => return Err(failure("SELF_TEST", "unarmed side sync was accepted")),
        }
        device.arm()?;
        let first = device.sync_side([48, 96, 255], 40)?;
        {
            let board = board.borrow();
            check.ok(first["synced"] == json!(true), "first side sync writes")?;
            check.ok(
                board.state[9] == 2
                    && board.state[10] == 40
                    && board.state[12] == 0
                    && board.state[13] == 0
                    && board.state[14..17] == [48u8, 96, 255],
                "side sync writes static mode, brightness and colour",
            )?;
            check.ok(
                board.state[0] == 21 && board.state[1] == 100 && board.state[17..22] == original[17..22],
                "side sync preserves the owned main session and preset fields",
            )?;
        }
        let writes_after_first = board.borrow().writes;
        let second = device.sync_side([48, 96, 255], 40)?;
        check.ok(
            second["synced"] == json!(false) && board.borrow().writes == writes_after_first,
            "an unchanged side zone skips the D6 write",
        )?;
        let dimmer = device.sync_side([48, 96, 255], 12)?;
        check.ok(
            dimmer["synced"] == json!(true) && board.borrow().state[10] == 12,
            "a changed sync writes again",
        )?;
        device.restore()?;
        check.ok(
            board.borrow().state == original && !device.recovery_pending(),
            "restore returns the original side configuration exactly",
        )?;
        check.ok(device.shutdown(), "side sync session shuts down restored")?;
    }
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

/// The complete connect → arm → paint → restore lifecycle against a simulated
/// board, including the 1.0.16.6 → 1.0.23.6 firmware upgrade transition.
fn test_firmware_upgrade_lifecycle(check: &Check) -> Result<()> {
    use std::cell::RefCell;
    struct Board {
        firmware: Vec<u8>,
        state: Vec<u8>,
        rgb: Vec<u8>,
        writes: usize,
        rgb_packets: Vec<Vec<u8>>,
        reject_rgb_probe: bool,
        reject_state_write: bool,
    }
    impl Board {
        fn exchange(&mut self, frame: &[u8; REPORT_SIZE]) -> Result<[u8; REPORT_SIZE]> {
            let command = frame[1];
            let length = frame[4] as usize;
            let address = frame[5] as usize | (frame[6] as usize) << 8;
            let payload: Vec<u8> = match command {
                0xa1 => self.firmware.clone(),
                0xa0 => vec![0, 8, 4, 6, 16, 2, 0, 0],
                0xd1 => vec![104],
                0xd5 => self.state.clone(),
                0xd2 => {
                    if self.reject_rgb_probe {
                        return Err(failure("HID_TIMEOUT", "simulated incomplete read-only probe"));
                    }
                    let mut rendered = self.rgb.clone();
                    if self.firmware[0] >= 17 && self.state[0] == 21 {
                        let value = *self.state[6..9].iter().max().unwrap() as u32 * 200 / 255;
                        for i in 0..252 {
                            let v = self.rgb[i] as u32 * value / 255;
                            rendered[i] = if i >= 250 { (v * 150 / 255) as u8 } else { v as u8 };
                        }
                    }
                    rendered[address..address + length].to_vec()
                }
                0xd6 => {
                    if self.reject_state_write {
                        return Err(failure("HID_TIMEOUT", "simulated restore write timeout"));
                    }
                    self.writes += 1;
                    let payload = frame[8..8 + length].to_vec();
                    self.state[address..address + length].copy_from_slice(&payload);
                    if self.firmware[0] == 23 && address <= 6 && address + length >= 9 {
                        let quantized = protocol::main_color_readback(&self.state[6..9]);
                        self.state[6..9].copy_from_slice(&quantized);
                    }
                    payload
                }
                0xd8 => {
                    self.writes += 1;
                    let payload = frame[8..8 + length].to_vec();
                    self.rgb_packets.push(payload.clone());
                    for index in (0..length).step_by(4) {
                        let start = payload[index] as usize * 3;
                        self.rgb[start..start + 3].copy_from_slice(&payload[index + 1..index + 4]);
                    }
                    payload
                }
                _ => return Err(failure("SELF_TEST", "Unexpected lifecycle command")),
            };
            if payload.len() != length {
                return Err(failure("SELF_TEST", "Incorrect lifecycle response length"));
            }
            let mut reply = [0u8; REPORT_SIZE];
            reply[0] = 0xaa;
            reply[1] = command;
            reply[4..8].copy_from_slice(&frame[4..8]);
            reply[8..8 + length].copy_from_slice(&payload);
            reply[3] = protocol::checksum(&reply);
            Ok(reply)
        }
    }
    let dir = temp_dir("nuphylab-upgrade-test");
    let board = std::rc::Rc::new(RefCell::new(Board {
        firmware: vec![16, 0, 1, 170, 6, 0, 0, 0],
        state: vec![6, 25, 4, 0, 1, 0, 0, 1, 64, 4, 60, 0, 1, 0, 255, 0, 0],
        rgb: vec![20; 312],
        writes: 0,
        rgb_packets: Vec::new(),
        reject_rgb_probe: false,
        reject_state_write: false,
    }));
    let journals = LightingJournals::new(dir.join("original.json"));
    let exchange_board = std::rc::Rc::clone(&board);
    let device = RefCell::new(Air75Device::new_test(
        journals.legacy_url.clone(),
        Box::new(move |frame| exchange_board.borrow_mut().exchange(frame)),
    ));
    {
        let mut device = device.borrow_mut();
        device.connect()?;
        device.arm()?;
        device.paint(&vec![8u8; 252], true)?;
        let old_journal = std::fs::read(journals.url(0)).unwrap();
        check.ok(device.owns_changes(), "old session owns its actual mutations before upgrade")?;
        board.borrow_mut().firmware = vec![23, 0, 1, 170, 6, 0, 0, 0];
        {
            let mut board = board.borrow_mut();
            board.state = vec![22, 75, 2, 0, 0, 3, 0, 190, 174, 4, 60, 2, 1, 0, 255, 0, 0, 3, 24, 48, 96, 2];
            let quantized = protocol::main_color_readback(&board.state[6..9]);
            board.state[6..9].copy_from_slice(&quantized);
            // Use exact inverse representatives so the fixture can check both raw
            // palette restoration and the rendered-readback quantization path.
            let rendered: Vec<u8> = (0..252).map(|i| (i % 251) as u8).collect();
            board.rgb = protocol::palette_from_rendered(&protocol::rendered_custom_colors(&rendered, &board.firmware), &board.firmware, false)?
                .into_iter()
                .chain(std::iter::repeat(0u8).take(60))
                .collect();
        }
        let upgraded_state = board.borrow().state.clone();
        let upgraded_rgb = board.borrow().rgb.clone();
        let prior_writes = board.borrow().writes;
        match device.restore() {
            Err(error) => check.ok(error.code == "BACKUP_FIRMWARE_MISMATCH", "upgrade stops stale owned restoration")?,
            Ok(_) => return Err(failure("SELF_TEST", "cross-firmware live restore was accepted")),
        }
        check.ok(
            board.borrow().writes == prior_writes && std::fs::read(journals.url(0)).unwrap() == old_journal,
            "blocked restore writes nothing and preserves the original journal",
        )?;
        board.borrow_mut().reject_rgb_probe = true;
        match device.connect() {
            Err(error) => check.ok(error.code == "HID_TIMEOUT", "reconnect must finish every read-only probe")?,
            Ok(_) => return Err(failure("SELF_TEST", "incomplete reconnect was accepted")),
        }
        check.ok(
            device.owns_changes() && std::fs::read(journals.url(0)).unwrap() == old_journal,
            "failed probe cannot retire ownership or archive the original",
        )?;
        board.borrow_mut().reject_rgb_probe = false;
        let reconnected = device.connect()?;
        check.ok(
            reconnected["recoveryPending"] == json!(false) && !device.owns_changes(),
            "same-process verified reconnect retires old-firmware ownership",
        )?;
        let history: Vec<_> = std::fs::read_dir(dir.join("firmware-recovery-history")).unwrap().collect();
        check.ok(history.len() == 1 && std::fs::read(history[0].as_ref().unwrap().path()).unwrap() == old_journal, "same-process upgrade archives the exact old journal once")?;
        {
            let board = board.borrow();
            check.ok(
                board.writes == prior_writes && board.state == upgraded_state && board.rgb == upgraded_rgb,
                "firmware transition never writes the old schema into the new board",
            )?;
        }
        device.arm()?;
        board.borrow_mut().rgb_packets.clear();
        let mut animated = vec![32u8; 252];
        device.paint(&animated, false)?;
        {
            let board = board.borrow();
            check.ok(
                board.rgb_packets.len() == 7 && board.rgb_packets.iter().flatten().count() == 336,
                "first paint after arm sends all 84 LEDs and validates readback",
            )?;
        }
        board.borrow_mut().rgb_packets.clear();
        for index in [0usize, 1, 12, 13, 26, 52, 83] {
            animated[index * 3] = 64;
        }
        device.paint(&animated, false)?;
        {
            let board = board.borrow();
            check.ok(
                board.rgb_packets.len() == 1
                    && (0..28).step_by(4).map(|i| board.rgb_packets[0][i]).collect::<Vec<_>>() == vec![0u8, 1, 12, 13, 26, 52, 83],
                "sparse frame sends actual changed LED indices in one acknowledged packet",
            )?;
        }
        check.ok(
            {
                let firmware = board.borrow().firmware.clone();
                device.read_rgb()?[..252] == protocol::rendered_custom_colors(&animated, &firmware)[..]
            },
            "delta output preserves all unchanged LED values",
        )?;
        board.borrow_mut().rgb_packets.clear();
        device.paint(&animated, false)?;
        check.ok(board.borrow().rgb_packets.is_empty(), "unchanged frame sends no D8")?;
        device.paint(&animated, true)?;
        check.ok(board.borrow().rgb_packets.len() == 7, "explicit verification resends a full frame even when unchanged")?;
        board.borrow_mut().firmware[6] = 124;
        board.borrow_mut().firmware[7] = 124;
        let restored = device.restore()?;
        {
            let board = board.borrow();
            check.ok(
                restored["verified"] == json!(true) && board.state == upgraded_state && board.rgb == upgraded_rgb,
                "new session preserves the entire new state and original custom palette",
            )?;
        }
        let status = device.status()?;
        check.ok(status["capabilities"]["perKey"] == json!(true), "reserved A1 changes do not invalidate a verified protocol")?;
        device.arm()?;
        let same_firmware_journal = std::fs::read(journals.url(0)).unwrap();
        device.simulate_removal();
        let same_firmware_reconnect = device.connect()?;
        check.ok(
            same_firmware_reconnect["recoveryPending"] == json!(true)
                && device.owns_changes()
                && std::fs::read(journals.url(0)).unwrap() == same_firmware_journal,
            "ordinary unplug/reconnect retains same-firmware recovery instead of archiving it",
        )?;
        device.restore()?;
        {
            let mut board = board.borrow_mut();
            board.state = vec![21, 40, 2, 0, 1, 3, 64, 128, 192, 4, 60, 2, 1, 0, 255, 0, 0, 3, 24, 48, 96, 2];
            let quantized = protocol::main_color_readback(&board.state[6..9]);
            board.state[6..9].copy_from_slice(&quantized);
            board.rgb = (0..312usize).map(|i| (i * 73 % 256) as u8).collect();
        }
        let dim_state = board.borrow().state.clone();
        let dim_rendered = device.read_rgb()?;
        device.arm()?;
        device.paint(&vec![15u8; 252], true)?;
        device.restore()?;
        {
            let actual = device.read_rgb()?;
            let board = board.borrow();
            check.ok(board.state == dim_state && actual == dim_rendered, "quantized palette candidates restore the exact original dimmed custom output")?;
        }
        // Capture from the replacement 1.0.23.6 board: NuPhyIO tolerates its
        // out-of-range readback speed. It must survive a full lighting lifecycle.
        {
            let mut board = board.borrow_mut();
            board.state = vec![6, 100, 2, 0, 1, 6, 0, 9, 255, 4, 60, 2, 1, 0, 255, 0, 0, 6, 94, 94, 94, 94];
        }
        let raw_preset_state = board.borrow().state.clone();
        let raw_preset_palette = protocol::rendered_custom_colors(&board.borrow().rgb[..252].to_vec(), &board.borrow().firmware);
        device.connect()?;
        device.arm()?;
        let raw_record = journals.read(&journals.url(0), "self-test")?;
        check.ok(raw_record.state == raw_preset_state, "journal preserves unnormalized preset readback")?;
        device.paint(&vec![48u8; 252], true)?;
        {
            let board = board.borrow();
            let suffix_matches = board.state[board.state.len() - 5..] == raw_preset_state[raw_preset_state.len() - 5..];
            check.ok(suffix_matches, "playback leaves all five preset bytes untouched")?;
        }
        device.restore()?;
        {
            let board = board.borrow();
            check.ok(
                board.state == raw_preset_state
                    && protocol::rendered_custom_colors(&board.rgb[..252].to_vec(), &board.firmware) == raw_preset_palette,
                "restore retains raw preset state and prior rendered custom palette",
            )?;
        }
        device.arm()?;
        let disconnect_journal = std::fs::read(journals.url(0)).unwrap();
        board.borrow_mut().reject_state_write = true;
        let disconnected = device.disconnect()?;
        check.ok(
            disconnected["connected"] == json!(false) && disconnected["armed"] == json!(false),
            "failed restoration must still release the connection",
        )?;
        check.ok(
            disconnected["recoveryPending"] == json!(true) && std::fs::read(journals.url(0)).unwrap() == disconnect_journal,
            "failed disconnect restoration retains the exact recovery journal",
        )?;
        board.borrow_mut().reject_state_write = false;
        device.connect()?;
        device.restore()?;
        {
            let board = board.borrow();
            check.ok(board.state == raw_preset_state && !device.recovery_pending(), "a later explicit reconnect completes deferred restoration")?;
        }
        check.ok(device.shutdown(), "upgraded session shuts down with all owned state restored")?;
    }
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[test]
fn the_ported_self_test_passes() {
    run().expect("self-test");
}
