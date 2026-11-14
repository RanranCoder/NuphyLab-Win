// JSONL op dispatch. The Windows M1 surface implements the lighting session
// contract; macOS-only input remapping, global key hooks and shortcut delivery
// report UNSUPPORTED until their milestones land.
use crate::device::Air75Device;
use crate::error::{failure, Result};
use crate::protocol;
use serde_json::{json, Value};

/// Windows has no TCC-style permission gate for HID access, low-level key
/// listening or SendInput, so the snapshot is always fully granted.
pub fn permission_snapshot() -> Value {
    json!({
        "inputMonitoring": true,
        "accessibility": true,
        "canPostEvents": true,
        "promptsRequested": false,
    })
}

pub fn execute(device: &mut Air75Device, command: &Value) -> Result<Value> {
    let op = command.get("op").and_then(|v| v.as_str()).unwrap_or_default();
    match op {
        "connect" => device.connect(),
        "status" => device.status(),
        "permissions" => Ok(permission_snapshot()),
        "bluetoothDevices" => Ok(json!({"devices": []})),
        "openPermissionSettings" => Err(failure(
            "INVALID_PERMISSION",
            "Windows 没有对应的系统权限页；HID 访问无需授权。",
        )),
        "arm" => device.arm(),
        "paint" => {
            let colors = parse_colors_value(command.get("colors"))?;
            let verify = command.get("verify").and_then(|v| v.as_bool()).unwrap_or(false);
            device.paint(&colors, verify)
        }
        "readColors" => {
            let bytes = device.read_rgb()?;
            Ok(json!({
                "colors": protocol::hex_colors(&bytes),
                "ledCount": 104,
                "customColorScale": if protocol::state_length(&device.firmware) == Some(22) { 200 } else { 255 },
            }))
        }
        "systemIdle" => match crate::winhid::system_idle_ms() {
            Some(idle_ms) => Ok(json!({"idleMs": idle_ms, "recordsKeys": false})),
            None => Ok(json!({"idleMs": Value::Null, "recordsKeys": false})),
        },
        "side" => {
            if command.get("restore").and_then(|v| v.as_bool()) == Some(true) {
                return device.restore_side();
            }
            let color = command
                .get("color")
                .and_then(|v| v.as_str())
                .ok_or_else(|| failure("INVALID_SIDE", "side requires a color string."))?;
            let brightness = command.get("brightness").and_then(|v| v.as_i64()).unwrap_or(60);
            if !(0..=100).contains(&brightness) {
                return Err(failure("INVALID_SIDE", "side brightness must be an integer from 0 to 100."));
            }
            device.set_side(protocol::parse_color(color)?, brightness as u8)
        }
        "sideSync" => {
            let color = command
                .get("color")
                .and_then(|v| v.as_str())
                .ok_or_else(|| failure("INVALID_SIDE", "sideSync requires a color string."))?;
            let brightness = command.get("brightness").and_then(|v| v.as_i64()).unwrap_or(60);
            if !(0..=100).contains(&brightness) {
                return Err(failure("INVALID_SIDE", "sideSync brightness must be an integer from 0 to 100."));
            }
            device.sync_side(protocol::parse_color(color)?, brightness as u8)
        }
        "restore" => device.restore(),
        "disconnect" => device.disconnect(),
        // hardware.close() always sends restoreKeys: there are no owned OS key
        // mappings on Windows, so a truthful no-op keeps clean shutdowns clean.
        "restoreKeys" => Ok(json!({"restored": true})),
        "adaptKeys" | "keyMappingStatus" => Err(failure(
            "UNSUPPORTED",
            "按键适配依赖 macOS 系统键位映射，Windows 版本不提供该功能。",
        )),
        "readKeymap" | "prepareFunctionKey" | "restoreFunctionKey" => Err(failure(
            "UNSUPPORTED",
            "F4 键位租约属于 macOS 适配功能，Windows 版本不提供该功能。",
        )),
        "keyMonitor" => crate::hooks::configure_key_monitor(command),
        "triggerMonitor" => crate::hooks::configure_trigger_monitor(command),
        "shortcut" => crate::hooks::shortcut(command),
        other => Err(failure("UNKNOWN_OPERATION", format!("Unknown operation: {other}"))),
    }
}

fn parse_colors_value(value: Option<&Value>) -> Result<Vec<u8>> {
    let colors: Option<Vec<String>> = value.and_then(|v| v.as_array()).map(|array| {
        array
            .iter()
            .map(|entry| entry.as_str().unwrap_or_default().to_string())
            .collect()
    });
    protocol::parse_colors(colors.as_ref(), 84)
}
