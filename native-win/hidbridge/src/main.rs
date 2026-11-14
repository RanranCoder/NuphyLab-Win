// JSONL stdin/stdout bridge. Requests are processed strictly in order; the
// reply goes out before the next line is read. See native/PROTOCOL.md.
mod device;
mod error;
mod hooks;
mod journal;
mod ops;
mod protocol;
mod selftest;
mod winhid;

use device::{emit, diagnostic, Air75Device};
use error::{failure, Result};
use serde_json::{json, Value};
use std::io::Read;

const MAX_REQUEST_BYTES: usize = 32768;

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.iter().any(|a| a == "--self-test") {
        let result = selftest::run();
        match result {
            Ok(checks) => emit(json!({"ok": true, "selfTest": "passed", "checks": checks, "hardwareOpened": false, "inputMonitorOpened": false})),
            Err(error) => {
                diagnostic(&format!("{error}"));
                std::process::exit(1);
            }
        }
        return;
    }
    if arguments.iter().any(|a| a == "--help") {
        println!("air75-hid-bridge --backup <absolute\\path.json> [--self-test]");
        println!("JSONL stdin/stdout. See native/PROTOCOL.md. HID opens only after connect or restore.");
        return;
    }
    let mut backup_path: Option<String> = None;
    let mut index = 0;
    while index < arguments.len() {
        if arguments[index] == "--backup" && index + 1 < arguments.len()
            && std::path::Path::new(&arguments[index + 1]).is_absolute()
        {
            backup_path = Some(arguments[index + 1].clone());
            index += 2;
        } else {
            diagnostic("Use --backup followed by an absolute path.");
            std::process::exit(64);
        }
    }
    let backup = backup_path.unwrap_or_else(|| {
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        format!("{home}\\AppData\\Roaming\\Air75 Studio\\lighting-backup.json")
    });
    let mut device = Air75Device::new(std::path::PathBuf::from(backup));
    // Events surface before responses; process one request at a time so the
    // reply ordering stays identical to the macOS implementation's main queue.
    let mut quitting = false;
    for line in BoundedLines::new(std::io::stdin().lock()) {
        let line = match line {
            Ok(line) => line,
            Err(BoundedLineError::TooLarge) => {
                emit(json!({"id": Value::Null, "ok": false, "error": {"code": "REQUEST_TOO_LARGE", "message": "A JSONL request must be smaller than 32 KiB."}}));
                continue;
            }
            Err(BoundedLineError::Io(error)) => {
                device::diagnostic(&format!("stdin read failed: {error}"));
                break;
            }
        };
        if line.is_empty() {
            continue;
        }
        let command: Value = match serde_json::from_str(&line) {
            Ok(command) => command,
            Err(_) => {
                emit(json!({"id": Value::Null, "ok": false, "error": {"code": "INVALID_JSON", "message": "Every line must be a JSON object."}}));
                continue;
            }
        };
        let id = command.get("id").cloned().unwrap_or(Value::Null);
        if command.get("op").and_then(|v| v.as_str()) == Some("quit") {
            emit(json!({"id": id, "ok": true, "result": {"quitting": true}}));
            quitting = true;
            break;
        }
        let result = match execute_quiet(&mut device, &command) {
            Ok(result) => json!({"id": id, "ok": true, "result": result}),
            Err(error) => json!({"id": id, "ok": false, "error": {"code": error.code, "message": error.message}}),
        };
        emit(result);
    }
    shutdown(device, quitting);
}

fn execute_quiet(device: &mut Air75Device, command: &Value) -> Result<Value> {
    // A malformed request must not crash the bridge before a device restore.
    if command.get("op").and_then(|v| v.as_str()).is_none() {
        return Err(failure("INVALID_REQUEST", "Every line must include op and an id."));
    }
    ops::execute(device, command)
}

fn shutdown(mut device: Air75Device, _quitting: bool) {
    hooks::stop_all();
    let lighting_restored = device.shutdown();
    // No global key hooks exist on Windows yet: key restoration is a no-op.
    emit(json!({
        "event": "shutdownComplete",
        "lightingRestored": lighting_restored && !device.has_pending_backups(),
        "lightingDeferred": lighting_restored && device.has_pending_backups(),
        "keysRestored": true,
        "keysDeferred": false,
    }));
    if !(lighting_restored && !device.has_pending_backups()) {
        // Swift distinguishes successful restoration from retained failure; the
        // parent reacts to exit code 2 by reporting a retained backup.
        std::process::exit(2);
    }
}

enum BoundedLineError {
    TooLarge,
    Io(std::io::Error),
}

/// Reads one UTF-8 line at a time with the size limit enforced *while*
/// buffering, so oversized input cannot balloon memory before rejection.
struct BoundedLines<R: Read> {
    inner: R,
}

impl<R: Read> BoundedLines<R> {
    fn new(inner: R) -> Self {
        BoundedLines { inner }
    }
    fn next_line(&mut self) -> std::result::Result<Option<String>, BoundedLineError> {
        let mut bytes = Vec::with_capacity(512);
        let mut chunk = [0u8; 1];
        loop {
            match self.inner.read(&mut chunk) {
                Ok(0) => {
                    if bytes.is_empty() {
                        return Ok(None);
                    }
                    break;
                }
                Ok(_) => {
                    if chunk[0] == b'\n' {
                        break;
                    }
                    bytes.push(chunk[0]);
                    if bytes.len() > MAX_REQUEST_BYTES {
                        // Discard only this line so the parent's stream stays
                        // aligned, then report the oversized request.
                        loop {
                            match self.inner.read(&mut chunk) {
                                Ok(0) => return Err(BoundedLineError::TooLarge),
                                Ok(_) if chunk[0] == b'\n' => return Err(BoundedLineError::TooLarge),
                                Ok(_) => {}
                                Err(_) => return Err(BoundedLineError::TooLarge),
                            }
                        }
                    }
                }
                Err(error) => return Err(BoundedLineError::Io(error)),
            }
        }
        while bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }
}

impl<R: Read> Iterator for BoundedLines<R> {
    type Item = std::result::Result<String, BoundedLineError>;
    fn next(&mut self) -> Option<Self::Item> {
        match self.next_line() {
            Ok(Some(line)) => Some(Ok(line)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        }
    }
}
