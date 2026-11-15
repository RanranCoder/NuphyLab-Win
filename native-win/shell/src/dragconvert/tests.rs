// Drag Convert module tests: trigger mapping/matching, settings validation
// and patching, history bounds and store round trips.

use super::store::{Batch, DragPreferences, DragStore};
use super::trigger::{self, DragHookRuntime};
use super::*;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{name}-{}", crate::pocket::store::uuid_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn trigger_vk_maps_modifiers_and_mac_codes() {
    assert_eq!(trigger::trigger_vk("shift"), Some(0x10));
    assert_eq!(trigger::trigger_vk("option"), Some(0x12));
    assert_eq!(trigger::trigger_vk("control"), Some(0x11));
    assert_eq!(trigger::trigger_vk("command"), Some(0x5B));
    assert_eq!(trigger::trigger_vk("key:0"), Some(0x41)); // A
    assert_eq!(trigger::trigger_vk("key:49"), Some(0x20)); // Space
    assert_eq!(trigger::trigger_vk("key:122"), Some(0x70)); // F1
    assert_eq!(trigger::trigger_vk("key:29"), Some(0x30)); // 0
    assert_eq!(trigger::trigger_vk("key:53"), Some(0x1B)); // Esc maps, but…
    assert!(!trigger::is_valid_trigger("key:53"), "Esc stays cancellation-only");
    assert!(trigger::trigger_vk("key:999").is_none());
    assert!(trigger::trigger_vk("").is_none());
}

#[test]
fn trigger_table_matches_json_and_labels_exist() {
    let table = trigger::trigger_key_table();
    assert!(table.len() >= 40, "shared table loaded: {}", table.len());
    assert!(trigger::is_valid_trigger("shift"));
    assert!(trigger::is_valid_trigger("key:11"));
    assert!(!trigger::is_valid_trigger("key:53"));
    let labels = trigger::trigger_labels();
    assert_eq!(labels.get("shift").map(String::as_str), Some("⇧ Shift"));
    assert_eq!(labels.get("key:0").map(String::as_str), Some("A"));
}

#[test]
fn hook_matcher_shows_only_during_real_drag() {
    let mut runtime = DragHookRuntime::new();
    runtime.enabled = true;
    runtime.vk = Some(0x10); // shift
    // Shift press without a mouse button: typing, never a menu.
    assert_eq!(trigger::handle_key_event(&mut runtime, 0x10, true, false), None);
    // Shift press while dragging: show the wheel.
    assert_eq!(trigger::handle_key_event(&mut runtime, 0x10, true, true), Some(trigger::MenuMessage::Show));
    // Repeat press is ignored.
    assert_eq!(trigger::handle_key_event(&mut runtime, 0x10, true, true), None);
    // Esc closes (the key itself passes through and cancels the drag).
    assert_eq!(trigger::handle_key_event(&mut runtime, 0x1B, true, true), Some(trigger::MenuMessage::Hide));
    runtime.menu_visible = true;
    // Trigger release closes.
    assert_eq!(trigger::handle_key_event(&mut runtime, 0x10, false, true), Some(trigger::MenuMessage::Hide));
    // Other keys never match.
    assert_eq!(trigger::handle_key_event(&mut runtime, 0x41, true, true), None);
}

#[test]
fn hook_disabled_never_shows() {
    let mut runtime = DragHookRuntime::new();
    runtime.enabled = false;
    runtime.vk = Some(0x10);
    assert_eq!(trigger::handle_key_event(&mut runtime, 0x10, true, true), None);
    assert!(!runtime.menu_visible);
}

#[test]
fn settings_patch_preserves_output_directory() {
    let mut preferences = DragPreferences::default();
    preferences.output_directory = Some(r"D:\output".into());
    preferences.apply_patch(&serde_json::json!({ "enabled": true, "trigger": "key:11", "quality": 0.96, "maxDimension": 2400, "outputDirectory": "C:/hack" }));
    assert!(preferences.enabled);
    assert_eq!(preferences.trigger, "key:11");
    assert_eq!(preferences.quality, 0.96);
    assert_eq!(preferences.max_dimension, 2400);
    assert_eq!(preferences.output_directory.as_deref(), Some(r"D:\output"));
    assert!(preferences.validate().is_ok());
}

#[test]
fn settings_validation_boundaries() {
    let mut preferences = DragPreferences::default();
    assert!(preferences.validate().is_ok(), "defaults valid");
    for quality in [0.72, 0.88, 0.96, 1.0] {
        preferences.quality = quality;
        assert!(preferences.validate().is_ok(), "quality {quality}");
    }
    for quality in [0.49, 1.01, f64::NAN, f64::INFINITY] {
        preferences.quality = quality;
        assert!(preferences.validate().is_err(), "quality {quality} rejected");
    }
    preferences.quality = 0.88;
    for dimension in [800u32, 1200, 1600, 2400] {
        preferences.max_dimension = dimension;
        assert!(preferences.validate().is_ok(), "dimension {dimension}");
    }
    preferences.max_dimension = 1024;
    assert!(preferences.validate().is_err());
    preferences.max_dimension = 1600;
    preferences.trigger = "key:53".into();
    assert!(preferences.validate().is_err(), "Esc rejected");
    preferences.trigger = "f17".into();
    assert!(preferences.validate().is_err(), "unknown trigger rejected");
    preferences.trigger = "shift".into();
    preferences.output_directory = Some("relative/path".into());
    assert!(preferences.validate().is_err(), "relative output rejected");
}

#[test]
fn history_is_bounded_and_newest_first() {
    let mut history = Vec::new();
    for round in 0..25 {
        Batch::push_history(&mut history, Batch::new("zip", round));
    }
    assert_eq!(history.len(), super::store::MAXIMUM_HISTORY);
    assert_eq!(history[0].format, "zip");
    assert_eq!(history[0].input_count, Some(24));
    assert_eq!(history.last().unwrap().input_count, Some(5));
}

#[test]
fn batch_serializes_ui_shape() {
    let mut batch = Batch::new("jpg", 3);
    batch.succeeded = 2;
    batch.failed = 1;
    batch.files.push(super::store::FileResult { input: "a.png".into(), output: "a.jpg".into(), error: String::new() });
    batch.cancelled = Some(false);
    let value = serde_json::to_value(&batch).unwrap();
    assert_eq!(value["format"], "jpg");
    assert_eq!(value["inputCount"], 3);
    assert!(value.get("cancelled").is_none(), "false cancels stay absent");
    assert_eq!(value["files"][0]["input"], "a.png");
    assert!(value.get("time").is_some());
}

#[test]
fn store_settings_round_trip_and_corruption_recovery() {
    let root = temp_dir("drag-store");
    let store = DragStore::open(&root);
    let mut settings = DragPreferences::default();
    settings.enabled = true;
    settings.trigger = "key:11".into();
    settings.quality = 0.72;
    settings.output_directory = Some(r"D:\copies".into());
    store.save_settings(&settings).unwrap();
    let reloaded = DragStore::open(&root).load_settings().unwrap();
    assert_eq!(reloaded, settings);
    // Corrupt file → error, defaults stay usable.
    std::fs::write(root.join("settings.json"), "{broken").unwrap();
    assert!(DragStore::open(&root).load_settings().is_err());
    // No temp leftovers after the round trip.
    let leftovers: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".settings"))
        .collect();
    assert!(leftovers.is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn store_history_round_trip_filters_dropped_outputs() {
    let root = temp_dir("drag-history");
    let store = DragStore::open(&root);
    let mut history = Vec::new();
    let mut outputs = std::collections::BTreeMap::new();
    let keep = Batch::new("zip", 1);
    let mut drop = Batch::new("jpg", 1);
    drop.id = "dropped".into();
    outputs.insert(keep.id.clone(), vec![PathBuf::from(r"D:\out\a.zip")]);
    outputs.insert("dropped".into(), vec![PathBuf::from(r"D:\out\b.jpg")]);
    history.push(keep.clone());
    history.push(drop);
    store.save_history(&history, &outputs).unwrap();
    let (reloaded, reloaded_outputs) = DragStore::open(&root).load_history();
    assert_eq!(reloaded.len(), 2);
    assert_eq!(reloaded[0].id, keep.id);
    assert_eq!(reloaded_outputs.len(), 1, "outputs of dropped batches vanish");
    assert!(reloaded_outputs.contains_key(&keep.id));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn snapshot_shape_matches_ui_contract() {
    let mut core = DragCore::open(&DragStore::open(&temp_dir("drag-snapshot")));
    core.settings.enabled = true;
    let value = snapshot(&core);
    for key in [
        "preferences",
        "ready",
        "busy",
        "completed",
        "total",
        "history",
        "issue",
        "progress",
        "currentAction",
        "currentFile",
        "cancelling",
        "triggerLabels",
        "inputPermissionNeeded",
    ] {
        assert!(value.get(key).is_some(), "snapshot missing {key}");
    }
    assert_eq!(value["inputPermissionNeeded"], false);
    assert_eq!(value["preferences"]["maxDimension"], 1600);
    assert_eq!(value["ready"], true);
    let degraded = core.current_issue();
    assert!(!degraded.is_empty(), "missing pdfium/ffmpeg surfaces an issue hint");
    core.availability = engine::Availability { pdfium: true, ffmpeg: true };
    assert!(core.current_issue().is_empty(), "full availability stays quiet");
}
