// Air75 V3 session state machine. Ported from HIDBridge.swift's Air75Device.
// All hardware access goes through the Transport trait; the self-test injects
// a scripted transport so the complete session lifecycle runs without a device.
use crate::error::{failure, Result};
use crate::journal::{iso8601_now, LightingBackup, LightingJournals};
use crate::protocol::{self, REPORT_SIZE};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// One request/response exchange over the 64-byte report pipe.
pub trait Transport {
    /// Send a request frame and return the matching 0xaa response frame.
    /// Implementations must not resend an uncertain write.
    fn exchange(&mut self, frame: &[u8; REPORT_SIZE], timeout: Duration) -> Result<[u8; REPORT_SIZE]>;
    /// Mark the transport closed; returns the serial number observed at open.
    fn close(&mut self);
    fn serial(&self) -> String;
}

pub fn emit(event: Value) {
    let mut line = serde_json::to_string(&event).unwrap_or_default();
    line.push('\n');
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let _ = handle.write_all(line.as_bytes());
    let _ = handle.flush();
}

pub fn diagnostic(message: &str) {
    eprintln!("{message}");
}

pub struct Air75Device {
    pub journals: LightingJournals,
    // None while closed. The test transport stands in for the Windows HID one.
    transport: Option<Box<dyn Transport>>,
    test: Option<TestTransportState>,
    test_connected: bool,
    key: u8,
    lighting_profile: u8,
    serial: String,
    backup: Option<LightingBackup>,
    mutation_attempted: bool,
    first_paint_verified: bool,
    last_paint: Option<Vec<u8>>,
    last_rgb_write_at: Option<Instant>,
    protocol_ready: bool,
    removed: bool,
    pub firmware: Vec<u8>,
    pub led_count: usize,
    pub armed: bool,
}

// The scripted transport closure is stored boxed so Air75Device stays Send-agnostic.
type TestExchange = dyn FnMut(&[u8; REPORT_SIZE]) -> Result<[u8; REPORT_SIZE]>;
struct TestTransportState {
    exchange: Box<TestExchange>,
}

impl Air75Device {
    pub fn new(backup_url: PathBuf) -> Air75Device {
        Air75Device {
            journals: LightingJournals::new(backup_url),
            transport: None,
            test: None,
            test_connected: false,
            key: 0,
            lighting_profile: 0,
            serial: String::new(),
            backup: None,
            mutation_attempted: false,
            first_paint_verified: false,
            last_paint: None,
            last_rgb_write_at: None,
            protocol_ready: false,
            removed: false,
            firmware: Vec::new(),
            led_count: 0,
            armed: false,
        }
    }

    /// Self-test seam: exercise the complete session lifecycle without HID.
    pub fn new_test(backup_url: PathBuf, exchange: Box<TestExchange>) -> Air75Device {
        let mut device = Air75Device::new(backup_url);
        device.test = Some(TestTransportState { exchange });
        device
    }

    /// Test seam standing in for the macOS removal callback: the scripted
    /// board never disappears on its own, so the lifecycle test calls this.
    pub fn simulate_removal(&mut self) {
        if self.test.is_some() {
            self.test_connected = false;
        }
    }

    pub fn backup_url(&self) -> PathBuf {
        self.journals.url(self.lighting_profile)
    }

    pub fn connected(&self) -> bool {
        let live = if self.test.is_some() { self.test_connected } else { self.transport.is_some() };
        live && !self.removed
    }

    pub fn owns_changes(&self) -> bool {
        self.mutation_attempted
    }

    pub fn recovery_pending(&self) -> bool {
        self.backup.is_some() || self.journals.has_legacy() || self.backup_url().exists()
    }

    pub fn deferred_profiles(&self) -> Vec<u8> {
        self.journals.profiles().into_iter().filter(|p| *p != self.lighting_profile).collect()
    }

    pub fn has_pending_backups(&self) -> bool {
        self.recovery_pending() || !self.deferred_profiles().is_empty()
    }

    fn require_connected(&self) -> Result<()> {
        if !self.connected() {
            return Err(failure("NOT_CONNECTED", "Connect the Air75 V3 by USB first."));
        }
        Ok(())
    }

    fn open_transport(&mut self) -> Result<()> {
        self.close_transport();
        if let Some(test) = self.test.as_mut() {
            // Take the scripted transport out only during construction-free runs:
            // the test exchange is stored on the device itself and consulted
            // directly in exchange(); nothing to open.
            let _ = test;
            self.test_connected = true;
            self.serial = "self-test".into();
            return Ok(());
        }
        let transport = crate::winhid::open()?;
        self.serial = transport.serial();
        self.transport = Some(transport);
        Ok(())
    }

    pub fn close_transport(&mut self) {
        if let Some(mut transport) = self.transport.take() {
            transport.close();
        }
        self.test_connected = false;
        self.removed = false;
        self.armed = false;
        self.last_rgb_write_at = None;
        self.protocol_ready = false;
    }

    fn exchange(&mut self, frame: &[u8; REPORT_SIZE], timeout: Duration) -> Result<[u8; REPORT_SIZE]> {
        self.require_connected()?;
        if let Some(test) = self.test.as_mut() {
            return (test.exchange)(frame);
        }
        match self.transport.as_mut().unwrap().exchange(frame, timeout) {
            Ok(reply) => Ok(reply),
            Err(error) => {
                if error.code == "DISCONNECTED" && !self.removed {
                    self.removed = true;
                    self.armed = false;
                    emit(json!({
                        "event": "disconnected",
                        "needsRestore": self.recovery_pending(),
                        "message": "Keyboard disconnected; its lighting backup has been retained."
                    }));
                }
                Err(error)
            }
        }
    }

    fn request(&mut self, command: u8, length: usize, address: usize, payload: &[u8]) -> Result<Vec<u8>> {
        let handle = if [0xd5u8, 0xd6, 0xd8].contains(&command) { self.lighting_profile } else { 0 };
        let frame = protocol::frame(command, length, address, payload, handle, self.key)?;
        let raw = self.exchange(&frame, Duration::from_millis(650))?;
        protocol::decode(&raw, command, length, address, handle, self.key)
    }

    fn refresh_lighting_profile(&mut self) -> Result<()> {
        let geometry = self.request(0xa0, 8, 0, &[])?;
        if !protocol::valid_geometry(&geometry) {
            return Err(failure("UNSUPPORTED_PROTOCOL", "键盘协议结构已改变，请更新 NuPhyLab 后重试。未修改键盘设置。"));
        }
        self.lighting_profile = geometry[0];
        Ok(())
    }

    fn verify_lighting_profile_for_write(&mut self) -> Result<()> {
        self.refresh_lighting_profile()?;
        if let Some(saved) = &self.backup {
            if saved.firmware[..6] != self.firmware[..6] {
                self.armed = false;
                return Err(failure("BACKUP_FIRMWARE_MISMATCH", "固件与恢复记录不一致，已保留备份并停止写入。请重新连接键盘。"));
            }
            if saved.lighting_profile.unwrap_or(0) != self.lighting_profile {
                self.armed = false;
                return Err(failure("LIGHTING_PROFILE_CHANGED", "键盘档位已改变，请重新播放灯效。"));
            }
        }
        Ok(())
    }

    fn select_recovery_profile(&mut self, reconnected: bool) -> Result<()> {
        self.journals.migrate_legacy(&self.serial)?;
        if let Some(saved) = &self.backup {
            if saved.lighting_profile.unwrap_or(0) != self.lighting_profile {
                // The old profile's durable journal stays on disk. Drop only the
                // live ownership, so a new session can use its own profile safely.
                self.backup = None;
                self.mutation_attempted = false;
                self.armed = false;
                self.first_paint_verified = false;
                self.last_paint = None;
            }
        }
        if self.backup.is_none() && self.backup_url().exists() {
            self.backup = Some(self.load_backup()?);
        }
        if let Some(saved) = self.backup.clone() {
            if saved.firmware[..6] != self.firmware[..6] {
                if !self.protocol_ready || (self.mutation_attempted && !reconnected) {
                    return Err(failure("BACKUP_FIRMWARE_MISMATCH", "固件与恢复记录不一致，已保留备份并停止写入。请重新连接键盘。"));
                }
                // Firmware updates can change the configuration schema. Keep the
                // old journal as history instead of writing old settings into the
                // new firmware. Its currently read settings become the new baseline.
                let archived = self.journals.archive_before_firmware_change(&saved, &self.serial)?;
                diagnostic(&format!("Prior firmware recovery record preserved at {}", archived.display()));
                self.backup = None;
                self.mutation_attempted = false;
                self.armed = false;
                self.first_paint_verified = false;
                self.last_paint = None;
                self.last_rgb_write_at = None;
            }
        }
        Ok(())
    }

    fn identify(&mut self) -> Result<()> {
        let initial = protocol::frame(0xa1, 8, 0, &[], 0, 0)?;
        let raw = self.exchange(&initial, Duration::from_millis(650))?;
        let (new_key, new_firmware) = protocol::identify(&raw)?;
        // Trailing A1 bytes are reserved/session data, not the firmware version.
        if self.firmware.get(..6) != new_firmware.get(..6) {
            self.protocol_ready = false;
        }
        self.key = new_key;
        self.firmware = new_firmware;
        let count_frame = protocol::frame(0xd1, protocol::LED_COUNT_LENGTH, 0, &[], 0, self.key)?;
        let raw = self.exchange(&count_frame, Duration::from_millis(650))?;
        self.led_count = protocol::decode_led_count(&raw, self.key)?;
        Ok(())
    }

    pub fn connect(&mut self) -> Result<Value> {
        if !self.connected() {
            self.open_transport()?;
        }
        // A separate configurator may have changed the in-RAM session key.
        // Refresh it using read-only A1 rather than mutating the key with EE.
        if let Err(error) = self.identify() {
            self.close_transport();
            return Err(error);
        }
        let state = self.read_state()?;
        self.read_rgb()?; // Validate the complete D2 route before allowing writes.
        self.protocol_ready = true;
        // A successfully reprobed firmware is a new lighting session even if
        // this helper controlled the pre-upgrade firmware. Retire that ownership
        // only here, after A1/A0/D1/D5/D2 all validate; status/restore still block.
        self.select_recovery_profile(true)?;
        let result = json!({
            "connected": true,
            "firmware": self.firmware,
            "firmwareHex": self.firmware.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "),
            "firmwareVersion": protocol::firmware_version(&self.firmware),
            "ledCount": self.led_count,
            "model": "Air75 V3 ANSI",
            "vendorId": 0x19f5,
            "productId": 0x1028,
            "transport": "USB",
            "keyLedCount": 84,
            "sideLedCount": 20,
            "sideIndependent": false,
            "originalState": state,
            "state": state,
            "lightingProfile": self.lighting_profile,
            "armed": self.armed,
            "needsRestore": self.recovery_pending(),
            "recoveryPending": self.recovery_pending(),
            "deferredProfiles": self.deferred_profiles(),
            "backupPath": self.backup_url().to_string_lossy(),
            "capabilities": self.capabilities(),
        });
        Ok(result)
    }

    fn capabilities(&self) -> Value {
        json!({
            "perKey": self.protocol_ready && protocol::supports_per_key(&self.firmware),
            "sideWholeZone": self.protocol_ready,
            "sidePixels": false,
            "protocol": "air75-s4",
            "stateBytes": protocol::state_length(&self.firmware).unwrap_or(0),
            "readProbeVerified": self.protocol_ready,
        })
    }

    pub fn status(&mut self) -> Result<Value> {
        let mut state: Option<Vec<u8>> = None;
        if self.connected() {
            state = Some(self.read_state()?);
            self.select_recovery_profile(false)?;
        }
        let mut result = json!({
            "connected": self.connected(),
            "armed": self.armed,
            "needsRestore": self.recovery_pending(),
            "recoveryPending": self.recovery_pending(),
            "deferredProfiles": self.deferred_profiles(),
            "backupPath": self.backup_url().to_string_lossy(),
            "firmware": self.firmware,
            "ledCount": self.led_count,
            "error": Value::Null,
            "permissions": crate::ops::permission_snapshot(),
        });
        if self.connected() {
            result["state"] = json!(state);
            result["lightingProfile"] = json!(self.lighting_profile);
            result["firmwareVersion"] = json!(protocol::firmware_version(&self.firmware));
            result["capabilities"] = self.capabilities();
        }
        Ok(result)
    }

    pub fn read_state(&mut self) -> Result<Vec<u8>> {
        self.refresh_lighting_profile()?;
        let Some(length) = protocol::state_length(&self.firmware) else {
            return Err(failure("UNSUPPORTED_PROTOCOL", "键盘协议结构已改变，请更新 NuPhyLab 后重试。未修改键盘设置。"));
        };
        let state = self.request(0xd5, length, 0, &[])?;
        if !protocol::readable_state(&state, &self.firmware) {
            self.protocol_ready = false;
            return Err(failure("UNSUPPORTED_PROTOCOL", "键盘协议结构已改变，请更新 NuPhyLab 后重试。未修改键盘设置。"));
        }
        Ok(state)
    }

    pub fn read_rgb(&mut self) -> Result<Vec<u8>> {
        self.require_connected()?;
        // D8 acknowledgement precedes the firmware's rendered D2 buffer update.
        // Reading immediately can mix 18 old LEDs with the rest of the new frame.
        // Wait only on diagnostic reads; ordinary animation writes stay unblocked.
        if let Some(written_at) = self.last_rgb_write_at {
            let remaining = written_at + Duration::from_millis(45);
            if remaining > Instant::now() {
                std::thread::sleep(remaining - Instant::now());
            }
        }
        let mut bytes = Vec::with_capacity(312);
        while bytes.len() < 312 {
            let count = (312 - bytes.len()).min(54);
            let chunk = self.request(0xd2, count, bytes.len(), &[])?;
            bytes.extend_from_slice(&chunk);
            if bytes.len() < 312 {
                std::thread::sleep(Duration::from_millis(8));
            }
        }
        Ok(bytes)
    }

    fn load_backup(&self) -> Result<LightingBackup> {
        let record = self.journals.read(&self.backup_url(), &self.serial)?;
        if record.lighting_profile.unwrap_or(0) != self.lighting_profile {
            return Err(failure("INVALID_BACKUP", "The lighting backup does not belong to this profile."));
        }
        Ok(record)
    }

    fn ensure_backup(&mut self) -> Result<()> {
        self.require_connected()?;
        if self.mutation_attempted && self.backup.is_some() {
            return Ok(());
        }
        if self.recovery_pending() {
            return Err(failure("RECOVERY_REQUIRED", "A previous lighting backup exists. Call restore before starting another lighting session."));
        }
        if !self.protocol_ready || !protocol::supports_per_key(&self.firmware) {
            return Err(failure("UNVERIFIED_FIRMWARE", "此固件未通过逐键灯效能力检查，请检查固件或更新 NuPhyLab。"));
        }
        let state = self.read_state()?;
        let captured_profile = self.lighting_profile;
        if state[9] > 4 {
            return Err(failure("UNSUPPORTED_SIDE_MODE", "The current side mode is outside the five stock modes; no configuration was changed."));
        }
        if state.len() == 17 && state[0] == 21 && state[1] != 100 {
            return Err(failure("CUSTOM_BACKUP_LOSSY", "Another custom renderer is active below 100% brightness. Its original RAM values cannot be recovered exactly; select a stock effect first."));
        }
        let colors = self.read_rgb()?;
        self.refresh_lighting_profile()?;
        if captured_profile != self.lighting_profile {
            return Err(failure("LIGHTING_PROFILE_CHANGED", "键盘档位已改变，请重新播放灯效。"));
        }
        let record = LightingBackup {
            created_at: iso8601_now(),
            serial: self.serial.clone(),
            firmware: self.firmware.clone(),
            state,
            rendered_colors: colors,
            custom_colors: None,
            lighting_profile: Some(self.lighting_profile),
        };
        let url = self.backup_url();
        if let Some(parent) = url.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| failure("BACKUP_SAVE_FAILED", format!("No lighting was changed because the backup could not be saved: {e}")))?;
        }
        // Exclusive creation prevents two bridges from replacing the original.
        let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&url).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                failure("BACKUP_CREATE_FAILED", "Cannot exclusively create backup; another lighting session may be active.")
            } else {
                failure("BACKUP_SAVE_FAILED", format!("No lighting was changed because the backup could not be saved: {e}"))
            }
        })?;
        {
            use std::io::Write;
            let mut handle = file;
            let mut text = serde_json::to_string_pretty(&record.to_json())
                .map_err(|e| failure("BACKUP_SAVE_FAILED", format!("{e}")))?;
            text.push('\n');
            handle.write_all(text.as_bytes()).map_err(|e| failure("BACKUP_SAVE_FAILED", format!("{e}")))?;
            handle.sync_all().map_err(|e| failure("BACKUP_SAVE_FAILED", format!("{e}")))?;
        }
        self.backup = Some(record);
        Ok(())
    }

    fn write_state_verified(&mut self, state: &[u8]) -> Result<()> {
        self.verify_lighting_profile_for_write()?;
        let current = self.read_state()?;
        let mut payload = protocol::state_write_payload(state, &current, &self.firmware)?;
        if self.firmware[..6] == [23, 0, 1, 170, 6, 0] {
            let input = protocol::exact_main_color_input(&state[6..9])?;
            payload[6..9].copy_from_slice(&input);
        }
        // Do not add a separate brightness-only commit: it can rescale RGB.
        let initial_echo = self.request(0xd6, payload.len(), 0, &payload)?;
        if initial_echo != payload {
            return Err(failure("STATE_ACK_MISMATCH", "D6 did not echo the complete lighting payload."));
        }
        std::thread::sleep(Duration::from_millis(120));
        let mut last_read = Vec::new();
        for attempt in 0..5 {
            last_read = self.read_state()?;
            if protocol::states_match(state, &last_read, &self.firmware) {
                return Ok(());
            }
            if attempt < 4 {
                std::thread::sleep(Duration::from_millis(120));
            }
        }
        diagnostic(&format!("D6 verify expected={state:?} actual={last_read:?}"));
        Err(failure(
            protocol::state_mismatch_code(state, &last_read),
            format!("D5 mismatch: expected {state:?}, observed {last_read:?}."),
        ))
    }

    pub fn arm(&mut self) -> Result<Value> {
        if self.armed {
            return Ok(json!({"armed": true, "backupPath": self.backup_url().to_string_lossy(), "alreadyArmed": true}));
        }
        self.ensure_backup()?;
        let mut next = self.read_state()?;
        next[0] = 21;
        next[1] = 100;
        next[4] = 1;
        // New firmware renders the stored palette through the main colour's
        // value as well as brightness. Use white while owning per-key output.
        if next.len() == 22 {
            next[6..9].copy_from_slice(&[255, 255, 255]);
        }
        self.mutation_attempted = true;
        match self.write_state_verified(&next) {
            Ok(()) => {
                if next.len() == 22 && self.backup.as_ref().is_some_and(|saved| saved.custom_colors.is_none()) {
                    // Capture the board's loaded custom palette before the first
                    // D8 can change it. The state-only journal already protects
                    // crashes during this temporary switch to custom mode.
                    let saved = self.backup.clone().unwrap();
                    let rendered = self.read_rgb()?;
                    let custom = protocol::palette_from_rendered(&rendered[..252], &self.firmware, false)?;
                    let updated = self.journals.save_custom_colors(&custom, &saved, &self.serial)?;
                    self.backup = Some(updated);
                }
                self.armed = true;
                self.first_paint_verified = false;
                self.last_paint = None;
                Ok(json!({"armed": true, "state": next, "backupPath": self.backup_url().to_string_lossy(), "verified": true}))
            }
            Err(error) => Err(self.rollback_error(error)),
        }
    }

    fn write_rgb(&mut self, colors: &[u8], previous: Option<&[u8]>) -> Result<()> {
        self.verify_lighting_profile_for_write()?;
        if colors.len() != 252 {
            return Err(failure("INVALID_COLORS", "Expected RGB data for 84 keys."));
        }
        // D8 carries independent index/R/G/B records. Keep the proven 13-record
        // packet size, but do not resend 84 LEDs when only task keys change.
        // First paint, explicit verification and restoration always use a full
        // frame; last_paint advances only after every packet is acknowledged.
        let changed: Vec<usize> = (0..84)
            .filter(|&index| match previous {
                Some(previous) if previous.len() == 252 => {
                    colors[index * 3..index * 3 + 3] != previous[index * 3..index * 3 + 3]
                }
                _ => true,
            })
            .collect();
        for start in (0..changed.len()).step_by(13) {
            let end = (start + 13).min(changed.len());
            let mut records = Vec::with_capacity((end - start) * 4);
            for &index in &changed[start..end] {
                records.push(index as u8);
                records.extend_from_slice(&colors[index * 3..index * 3 + 3]);
            }
            let echoed = self.request(0xd8, records.len(), 0, &records)?;
            if echoed != records {
                return Err(failure("COLOR_ACK_MISMATCH", "D8 did not echo the complete indexed RGB records."));
            }
        }
        self.last_rgb_write_at = Some(Instant::now());
        Ok(())
    }

    pub fn paint(&mut self, colors: &[u8], verify: bool) -> Result<Value> {
        if !self.armed {
            return Err(failure("NOT_ARMED", "Call arm before painting the 84 key LEDs."));
        }
        if self.last_paint.as_deref() == Some(colors) && !verify {
            return Ok(json!({"painted": 84, "skipped": true, "verification": "unchanged"}));
        }
        let full_verification = verify || !self.first_paint_verified;
        let previous = if full_verification { None } else { self.last_paint.clone() };
        match (|| -> Result<()> {
            self.write_rgb(colors, previous.as_deref())?;
            if full_verification {
                let rendered = self.read_rgb()?;
                let expected = protocol::rendered_custom_colors(colors, &self.firmware);
                if rendered[..252] != expected[..] {
                    let mismatches: Vec<usize> = expected.iter().enumerate().filter(|(i, _)| rendered[*i] != expected[*i]).map(|(i, _)| i).collect();
                    let samples: Vec<String> = mismatches
                        .iter()
                        .take(12)
                        .map(|&i| format!("{}:{}->{}", i, expected[i], rendered[i]))
                        .collect();
                    diagnostic(&format!(
                        "D2 verify firmware={} mismatchedChannels={} samples={}",
                        protocol::firmware_version(&self.firmware),
                        mismatches.len(),
                        samples.join(",")
                    ));
                    return Err(failure("COLOR_VERIFY_FAILED", "D2 did not show the requested key colors; original lighting will be restored."));
                }
                self.first_paint_verified = true;
            }
            Ok(())
        })() {
            Ok(()) => {
                self.last_paint = Some(colors.to_vec());
                Ok(json!({"painted": 84, "skipped": false, "verification": if full_verification { "readback" } else { "validatedAck" }}))
            }
            Err(error) => Err(self.rollback_error(error)),
        }
    }

    pub fn set_side(&mut self, color: [u8; 3], brightness: u8) -> Result<Value> {
        if brightness > 100 {
            return Err(failure("INVALID_SIDE", "side brightness must be an integer from 0 to 100."));
        }
        let mut state = self.read_state()?;
        state[9] = 2;
        state[10] = brightness;
        state[12] = 0;
        state[13] = 0;
        state[14..17].copy_from_slice(&color);
        if protocol::states_match(&state, &self.read_state()?, &self.firmware) {
            return Ok(json!({"sideIndependent": false, "changed": false, "state": state}));
        }
        self.ensure_backup()?;
        self.mutation_attempted = true;
        match self.write_state_verified(&state) {
            Ok(()) => Ok(json!({"sideIndependent": false, "changed": true, "state": state, "verified": true})),
            Err(error) => Err(self.rollback_error(error)),
        }
    }

    /// Frame-following side-light sync. Runs on throttled animation frames while
    /// a session owns lighting, so it settles for the D6 echo instead of the
    /// full sleep-plus-readback verify: a dropped write is corrected by the next
    /// sync frame rather than by a rollback. Never rolls back on its own; the
    /// caller treats sync failures as cosmetic.
    pub fn sync_side(&mut self, color: [u8; 3], brightness: u8) -> Result<Value> {
        if !self.armed {
            return Err(failure("NOT_ARMED", "Side sync requires an armed lighting session."));
        }
        self.verify_lighting_profile_for_write()?;
        let current = self.read_state()?;
        let mut state = current.clone();
        state[9] = 2;
        state[10] = brightness;
        state[12] = 0;
        state[13] = 0;
        state[14..17].copy_from_slice(&color);
        if protocol::states_match(&state, &current, &self.firmware) {
            return Ok(json!({"synced": false, "state": state}));
        }
        self.mutation_attempted = true;
        let mut payload = protocol::state_write_payload(&state, &current, &self.firmware)?;
        if self.firmware[..6] == [23, 0, 1, 170, 6, 0] {
            let input = protocol::exact_main_color_input(&state[6..9])?;
            payload[6..9].copy_from_slice(&input);
        }
        let echoed = self.request(0xd6, payload.len(), 0, &payload)?;
        if echoed != payload {
            return Err(failure("STATE_ACK_MISMATCH", "D6 did not echo the side sync payload."));
        }
        Ok(json!({"synced": true, "state": state, "verified": false}))
    }

    pub fn restore_side(&mut self) -> Result<Value> {
        self.require_connected()?;
        let Some(saved) = self.backup.clone() else {
            return Ok(json!({"changed": false, "reason": "noBackup", "sideIndependent": false}));
        };
        let mut state = self.read_state()?;
        state[9..17].copy_from_slice(&saved.state[9..17]);
        if protocol::states_match(&state, &self.read_state()?, &self.firmware) {
            return Ok(json!({"changed": false, "state": state, "sideIndependent": false}));
        }
        self.mutation_attempted = true;
        match self.write_state_verified(&state) {
            Ok(()) => Ok(json!({"changed": true, "restored": true, "verified": true, "state": state, "sideIndependent": false})),
            Err(error) => Err(self.rollback_error(error)),
        }
    }

    fn rollback_error(&mut self, original: crate::error::BridgeError) -> crate::error::BridgeError {
        if original.code == "LIGHTING_PROFILE_CHANGED" {
            return original;
        }
        match self.restore() {
            Ok(_) => crate::error::BridgeError {
                code: original.code,
                message: format!("{}. Original lighting was restored and verified.", original.message),
            },
            Err(error) => crate::error::BridgeError {
                code: "RESTORE_REQUIRED".into(),
                message: format!(
                    "Lighting operation failed: {}. Restoration also failed: {}. Backup retained at {}.",
                    original.message, error, self.backup_url().display()
                ),
            },
        }
    }

    pub fn restore(&mut self) -> Result<Value> {
        if !self.connected() && self.has_pending_backups() {
            self.connect()?;
        } else if self.connected() {
            self.identify()?;
            self.read_state()?;
            self.select_recovery_profile(false)?;
        }
        if !self.recovery_pending() {
            return Ok(json!({"restored": false, "reason": "noBackupForCurrentProfile", "verified": true, "needsRestore": false, "recoveryPending": false, "deferredProfiles": self.deferred_profiles()}));
        }
        let saved = match self.backup.clone() {
            Some(current) => current,
            None => {
                let loaded = self.load_backup()?;
                self.backup = Some(loaded.clone());
                loaded
            }
        };
        self.verify_lighting_profile_for_write()?;
        if saved.firmware[..6] != self.firmware[..6] {
            return Err(failure("BACKUP_FIRMWARE_MISMATCH", "Firmware changed since the backup. Refusing to restore an unverified configuration."));
        }
        self.mutation_attempted = true;
        if saved.state.len() == 17 && saved.state[0] == 21 && saved.state[1] != 100 {
            return Err(failure("CUSTOM_BACKUP_LOSSY", "Cannot exactly restore a dimmed custom framebuffer."));
        }
        let custom = saved
            .custom_colors
            .clone()
            .or_else(|| if saved.state[0] == 21 { Some(saved.rendered_colors[..252].to_vec()) } else { None });
        if let Some(colors) = custom {
            let mut current = self.read_state()?;
            let mut full_palette = current.clone();
            full_palette[0] = 21;
            full_palette[1] = 100;
            full_palette[4] = 1;
            if current.len() == 22 {
                full_palette[6..9].copy_from_slice(&[255, 255, 255]);
            }
            if current != full_palette {
                current = full_palette;
                self.write_state_verified(&current)?;
            }
            self.write_rgb(&colors, None)?;
            // The inspected new firmware debounces palette persistence for
            // one second. Leave that idle window before completing recovery.
            if saved.custom_colors.is_some() {
                std::thread::sleep(Duration::from_millis(1150));
            }
            let rendered = self.read_rgb()?;
            if rendered[..252] != protocol::rendered_custom_colors(&colors, &self.firmware)[..] {
                return Err(failure("RESTORE_COLOR_MISMATCH", "The original custom palette did not verify. Backup retained."));
            }
        }
        let mut target = protocol::restore_target(&saved.state);
        let restores_off = target != saved.state;
        if restores_off && protocol::states_match(&saved.state, &self.read_state()?, &self.firmware) {
            // A rejected arm may have left the board switched off throughout.
            // No write is needed to restore an already unchanged state.
            target = saved.state.clone();
        } else {
            self.write_state_verified(&target)?;
        }
        if restores_off {
            let rendered = self.read_rgb()?;
            if !rendered.iter().all(|&b| b == 0) {
                return Err(failure("RESTORE_COLOR_MISMATCH", "The prior off state did not render completely dark. Backup retained."));
            }
        }
        if saved.state[0] == 21 {
            std::thread::sleep(Duration::from_millis(40));
            let mut actual = self.read_rgb()?;
            if let Some(captured) = &saved.custom_colors {
                if saved.state.len() == 22 && actual[..252] != saved.rendered_colors[..252] {
                    // D2 is quantized. Try the other end of each identical-output
                    // interval only where the original dimmed output needs it.
                    let full = protocol::rendered_custom_colors(captured, &self.firmware);
                    let upper = protocol::palette_from_rendered(&full, &self.firmware, true)?;
                    let mut adjusted = captured.clone();
                    for i in 0..adjusted.len() {
                        if actual[i] != saved.rendered_colors[i] {
                            adjusted[i] = upper[i];
                        }
                    }
                    self.write_rgb(&adjusted, None)?;
                    std::thread::sleep(Duration::from_millis(1150));
                    actual = self.read_rgb()?;
                }
            }
            if actual[..252] != saved.rendered_colors[..252] {
                let mismatches: Vec<usize> = (0..252).filter(|&i| actual[i] != saved.rendered_colors[i]).collect();
                diagnostic(&format!(
                    "D2 restore verify firmware={} mismatches={} first={}",
                    protocol::firmware_version(&self.firmware),
                    mismatches.len(),
                    mismatches.iter().take(8).map(|&i| format!("{}: want {} got {}", i, saved.rendered_colors[i], actual[i])).collect::<Vec<_>>().join(", ")
                ));
                return Err(failure("RESTORE_COLOR_MISMATCH", "State restored, but the prior custom key colors did not verify. Backup retained."));
            }
        }
        let observed = self.read_state()?;
        if !protocol::states_match(&target, &observed, &self.firmware) {
            return Err(failure("RESTORE_STATE_MISMATCH", "Lighting state changed during restoration. Backup retained."));
        }
        // A moving stock effect will not reproduce an old frame. D5 verifies
        // its complete configuration; D2 is required for an original custom frame.
        std::fs::remove_file(self.backup_url())
            .map_err(|e| failure("BACKUP_CLEANUP_FAILED", format!("Lighting is verified restored, but the backup could not be removed: {e}")))?;
        self.backup = None;
        self.mutation_attempted = false;
        self.armed = false;
        self.first_paint_verified = false;
        self.last_paint = None;
        Ok(json!({
            "restored": true,
            "verified": true,
            "state": observed,
            "needsRestore": false,
            "recoveryPending": false,
            "deferredProfiles": self.deferred_profiles(),
            "verification": if restores_off { "allLightsOff" } else if saved.state[0] == 21 { "stateAndCustomColors" } else { "completeState" },
        }))
    }

    pub fn disconnect(&mut self) -> Result<Value> {
        let mut restored = json!({"restored": false});
        // A failed restore must not trap the USB handle open. Keep the durable
        // journal and explicitly report deferred restoration after releasing it.
        if self.recovery_pending() {
            match self.restore() {
                Ok(result) => restored = result,
                Err(error) => {
                    restored = json!({"restored": false, "verified": false, "error": format!("{error}")});
                    diagnostic(&format!("Disconnect recovery deferred: {error}"));
                }
            }
        }
        self.close_transport();
        Ok(json!({
            "connected": false,
            "armed": false,
            "restoration": restored,
            "recoveryPending": self.recovery_pending(),
            "needsRestore": self.recovery_pending(),
        }))
    }

    pub fn shutdown(&mut self) -> bool {
        let mut ok = true;
        if self.mutation_attempted {
            if !self.connected() {
                // USB removal makes restoration impossible. A validated
                // durable journal is a safe deferred exit, not a restore.
                match self.backup.clone().map(Ok).unwrap_or_else(|| self.load_backup()) {
                    Ok(saved) => {
                        let retained = self.journals.url(saved.lighting_profile.unwrap_or(0));
                        match self.journals.read(&retained, &self.serial) {
                            Ok(_) => emit(json!({"event": "restoreDeferred", "reason": "disconnected", "backupPath": retained.to_string_lossy()})),
                            Err(error) => {
                                ok = false;
                                diagnostic(&format!("RESTORE REQUIRED: {error}"));
                                emit(json!({"event": "restoreFailed", "error": format!("{error}"), "backupPath": self.backup_url().to_string_lossy()}));
                            }
                        }
                    }
                    Err(error) => {
                        ok = false;
                        diagnostic(&format!("RESTORE REQUIRED: {error}"));
                        emit(json!({"event": "restoreFailed", "error": format!("{error}"), "backupPath": self.backup_url().to_string_lossy()}));
                    }
                }
            } else {
                match self.restore() {
                    Ok(result) => emit(json!({"event": "restoredOnExit", "result": result})),
                    Err(error) => {
                        ok = false;
                        diagnostic(&format!("RESTORE REQUIRED: {error}"));
                        emit(json!({"event": "restoreFailed", "error": format!("{error}"), "backupPath": self.backup_url().to_string_lossy()}));
                    }
                }
            }
        }
        self.close_transport();
        ok
    }
}
