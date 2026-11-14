// WASAPI audio capture, porting AudioCapture.swift: system loopback (default
// render endpoint, AUDCLNT_STREAMFLAGS_LOOPBACK — no OS version gate on
// Windows) and microphone (default or chosen capture endpoint). Raw PCM stays
// in this process; only RMS + 8-band aggregates leave the analyzer.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wasapi::{
    get_default_device, initialize_mta, AudioCaptureClient, Device, DeviceState, DeviceCollection,
    Direction, SampleType, ShareMode, WaveFormat,
};

use crate::worker::WorkerLog;
use crate::https;

const FFT_SIZE: usize = 2048;
/// Band edges in Hz, matching AudioFeatureAnalyzer (vDSP) exactly.
const BAND_EDGES: [f64; 9] = [40.0, 80.0, 160.0, 320.0, 640.0, 1280.0, 2560.0, 5120.0, 12000.0];
const EMIT_INTERVAL: Duration = Duration::from_millis(45);

pub struct Feature {
    pub rms: f64,
    pub bands: [f64; 8],
}

/// 2048-sample rolling analyzer: Hann window, RMS and 8 summed-magnitude
/// bands, throttled to one feature per 45 ms (AudioFeatureAnalyzer parity).
pub struct Analyzer {
    samples: Vec<f32>,
    window: Vec<f32>,
    fft: Arc<dyn rustfft::Fft<f32>>,
    last_emit: Option<Instant>,
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer {
    pub fn new() -> Analyzer {
        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|n| 0.5 * (1.0 - (2.0 * std::f32::consts::PI * n as f32 / FFT_SIZE as f32).cos()))
            .collect();
        let mut planner = rustfft::FftPlanner::new();
        Analyzer { samples: Vec::with_capacity(FFT_SIZE * 2), window, fft: planner.plan_fft_forward(FFT_SIZE), last_emit: None }
    }

    pub fn consume(&mut self, input: &[f32], rate: f64) -> Option<Feature> {
        self.consume_at(input, rate, Instant::now())
    }

    pub fn consume_at(&mut self, input: &[f32], rate: f64, now: Instant) -> Option<Feature> {
        if rate <= 0.0 || input.is_empty() {
            return None;
        }
        self.samples.extend(input.iter().map(|v| if v.is_finite() { v.clamp(-1.0, 1.0) } else { 0.0 }));
        if self.samples.len() > FFT_SIZE {
            let drop = self.samples.len() - FFT_SIZE;
            self.samples.drain(..drop);
        }
        if self.samples.len() < FFT_SIZE {
            return None;
        }
        if let Some(last) = self.last_emit {
            if now.duration_since(last) < EMIT_INTERVAL {
                return None;
            }
        }
        self.last_emit = Some(now);
        let rms = (self.samples.iter().map(|v| (v * v) as f64).sum::<f64>() / FFT_SIZE as f64).sqrt();
        let mut spectrum: Vec<num_complex::Complex<f32>> = self
            .samples
            .iter()
            .zip(&self.window)
            .map(|(sample, window)| num_complex::Complex::new(sample * window, 0.0))
            .collect();
        self.fft.process(&mut spectrum);
        let mut bands = [0.0f64; 8];
        for (band, value) in bands.iter_mut().enumerate() {
            let lo = ((BAND_EDGES[band] * FFT_SIZE as f64 / rate).ceil() as usize).max(1);
            let hi = (((BAND_EDGES[band + 1] * FFT_SIZE as f64 / rate).ceil() as usize).max(lo + 1)).min(FFT_SIZE / 2);
            if lo >= hi {
                continue;
            }
            let mut power = 0.0f64;
            for bin in spectrum[lo..hi].iter() {
                power += (bin.re * bin.re + bin.im * bin.im) as f64;
            }
            *value = (power.sqrt() * 2.0 / FFT_SIZE as f64).min(1.0);
        }
        Some(Feature { rms, bands })
    }
}

// ---------------------------------------------------------------------------
// Packet delivery: one dispatcher thread coalesces to the latest audioPacket
// (sendPendingAudio parity) with a 2 s request timeout.

static DISPATCHER: OnceLock<mpsc::Sender<Value>> = OnceLock::new();

fn dispatch(packet: Value) {
    let sender = DISPATCHER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<Value>();
        std::thread::spawn(move || {
            while let Ok(first) = receiver.recv() {
                // Drain to the newest packet while a post is free to run.
                let mut latest = first;
                while let Ok(newer) = receiver.try_recv() {
                    latest = newer;
                }
                if latest.get("op").and_then(Value::as_str) == Some("audioPacket") {
                    let base = latest.get("__base").and_then(Value::as_str).unwrap_or_default().to_owned();
                    let token = latest.get("__token").and_then(Value::as_str).unwrap_or_default().to_owned();
                    if let Some(body) = latest.as_object_mut() {
                        body.remove("__base");
                        body.remove("__token");
                    }
                    https::post_command(&base, &token, &latest, Duration::from_secs(2));
                }
            }
        });
        sender
    });
    let _ = sender.send(packet);
}

fn packet(session: &str, status: &str, error: Option<&str>, base: &str, token: &str) -> Value {
    let mut packet = json!({"op": "audioPacket", "session": session, "status": status});
    if let Some(error) = error {
        packet["error"] = json!(error);
    }
    packet["__base"] = json!(base);
    packet["__token"] = json!(token);
    packet
}

// ---------------------------------------------------------------------------
// Inventory (4 s throttle) and session management.

static SESSION: Mutex<Session> = Mutex::new(Session { signature: String::new() });
static GENERATION: AtomicU64 = AtomicU64::new(0);
static LAST_INVENTORY: Mutex<Option<Instant>> = Mutex::new(None);

#[derive(Default)]
struct Session {
    signature: String,
}

fn capture_devices(log: &Arc<WorkerLog>) -> Vec<(String, String)> {
    let mut devices = Vec::new();
    let Ok(collection) = DeviceCollection::new(&Direction::Capture) else {
        log.append(b"[audio] capture device enumeration failed\n");
        return devices;
    };
    for device in &collection {
        let Ok(device) = device else { continue };
        if device.get_state().map(|state| state == DeviceState::Active).unwrap_or(false) {
            if let (Ok(id), Ok(name)) = (device.get_id(), device.get_friendlyname()) {
                devices.push((id, name));
            }
        }
    }
    if devices.is_empty() {
        log.append(b"[audio] no active capture devices found\n");
    }
    devices
}

fn post_inventory(base: &str, token: &str, log: &Arc<WorkerLog>) {
    {
        let mut last = LAST_INVENTORY.lock().unwrap();
        if last.map(|at| at.elapsed() < Duration::from_secs(4)).unwrap_or(false) {
            return;
        }
        *last = Some(Instant::now());
    }
    let devices: Vec<Value> = capture_devices(log)
        .into_iter()
        .map(|(id, name)| json!({"id": id, "name": name}))
        .collect();
    let inventory = json!({
        "op": "audioInventory",
        // System loopback needs no OS version and no permission on Windows.
        "systemSupported": true,
        // DXGI desktop duplication drives the screen-ambient source.
        "screenSupported": true,
        "devices": devices,
        "microphonePermission": true,
    });
    let base = base.to_owned();
    let token = token.to_owned();
    std::thread::spawn(move || https::post_command(&base, &token, &inventory, Duration::from_secs(5)));
}

/// AudioCapture.sync parity, driven by the /api/state poll (0.5 s). A changed
/// desired signature restarts the session; identical signatures keep running.
pub fn sync(desired: Option<&Value>, base: &str, token: &str, log: &Arc<WorkerLog>) {
    post_inventory(base, token, log);
    let desired = desired.filter(|value| {
        value.get("enabled") == Some(&Value::Bool(true))
            && value.get("session").and_then(Value::as_str).is_some_and(|s| !s.is_empty())
            && matches!(value.get("source").and_then(Value::as_str), Some("system") | Some("microphone"))
    });
    let Some(desired) = desired else {
        stop_active();
        return;
    };
    let session = desired.get("session").and_then(Value::as_str).unwrap_or_default().to_owned();
    let source = desired.get("source").and_then(Value::as_str).unwrap_or_default().to_owned();
    let device_id = desired.get("deviceId").and_then(Value::as_str).unwrap_or_default().to_owned();
    let signature = format!("{session}|{source}|{device_id}");
    let mut guard = SESSION.lock().unwrap();
    if guard.signature == signature {
        return;
    }
    guard.signature = signature.clone();
    drop(guard);
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    dispatch(packet(&session, "requesting", None, base, token));
    let log = log.clone();
    let base = base.to_owned();
    let token = token.to_owned();
    std::thread::spawn(move || {
        capture_thread(source, (!device_id.is_empty()).then_some(device_id), session, base, token, log, generation);
    });
}

pub fn stop_active() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    SESSION.lock().unwrap().signature = String::new();
}

fn capture_thread(
    source: String,
    device_id: Option<String>,
    session: String,
    base: String,
    token: String,
    log: Arc<WorkerLog>,
    generation: u64,
) {
    let failed = |code: &str| {
        GENERATION.fetch_add(1, Ordering::SeqCst);
        SESSION.lock().unwrap().signature = String::new();
        dispatch(packet(&session, "error", Some(code), &base, &token));
    };
    if initialize_mta().is_err() {
        log.append(b"[audio] COM initialization failed\n");
        failed("systemCapture");
        return;
    }
    let direction = if source == "system" { &Direction::Render } else { &Direction::Capture };
    let device: Option<Device> = match (source.as_str(), device_id.as_deref()) {
        ("microphone", Some(id)) => DeviceCollection::new(&Direction::Capture).ok().and_then(|collection| {
            for item in &collection {
                if let Ok(item) = item {
                    if item.get_id().map(|found| found == *id).unwrap_or(false) {
                        return Some(item);
                    }
                }
            }
            None
        }),
        _ => get_default_device(direction).ok(),
    };
    let Some(device) = device else {
        failed("inputUnavailable");
        return;
    };
    let started = start_capture(&device, direction, &session, &base, &token, &log, generation);
    if let Err(code) = started {
        failed(code);
    }
}

fn start_capture(
    device: &Device,
    direction: &Direction,
    session: &str,
    base: &str,
    token: &str,
    log: &Arc<WorkerLog>,
    generation: u64,
) -> Result<(), &'static str> {
    let is_loopback = direction == &Direction::Render;
    let client = device.get_iaudioclient().map_err(|error| {
        log.append(format!("[audio] get_iaudioclient failed: {error}\n").as_bytes());
        if is_loopback { "systemCapture" } else { "inputUnavailable" }
    })?;
    // Loopback must be initialized with the endpoint's mix format verbatim;
    // the shared-mode engine mix is float32, which the analyzer consumes
    // directly. Microphone capture requests float32 through the converter.
    let format = if is_loopback {
        let mix = client.get_mixformat().map_err(|error| {
            log.append(format!("[audio] get_mixformat failed: {error}\n").as_bytes());
            "systemFormat"
        })?;
        let float = mix.get_subformat().map(|sub| matches!(sub, SampleType::Float)).unwrap_or(false) && mix.get_bitspersample() == 32;
        if !float {
            log.append(format!("[audio] unexpected mix format: {}bit {}\n", mix.get_bitspersample(), mix.get_subformat().map(|s| s.to_string()).unwrap_or_default()).as_bytes());
            return Err("systemFormat");
        }
        mix
    } else {
        let mix = client.get_mixformat().map_err(|error| {
            log.append(format!("[audio] get_mixformat failed: {error}\n").as_bytes());
            "inputUnavailable"
        })?;
        WaveFormat::new(32, 32, &SampleType::Float, mix.get_samplespersec() as usize, mix.get_nchannels().max(1) as usize, None)
    };
    let rate = format.get_samplespersec();
    let channels = format.get_nchannels().max(1) as usize;
    let blockalign = format.get_blockalign() as usize;
    let (_, min_period) = client.get_periods().map_err(|error| {
        log.append(format!("[audio] get_periods failed: {error}\n").as_bytes());
        if is_loopback { "systemCapture" } else { "inputUnavailable" }
    })?;
    let mut client = client;
    // The initialize direction is ALWAYS Capture: on a Render device the
    // crate combines it with AUDCLNT_STREAMFLAGS_LOOPBACK. Passing Render here
    // would silently drop the loopback flag and the capture service request
    // on a render client would fail (AUDCLNT_E_WRONG_ENDPOINT_TYPE).
    client
        .initialize_client(&format, min_period, &Direction::Capture, &ShareMode::Shared, !is_loopback)
        .map_err(|error| {
            log.append(format!("[audio] initialize_client failed: {error}\n").as_bytes());
            if is_loopback { "systemCapture" } else { "inputUnavailable" }
        })?;
    // The crate always sets AUDCLNT_STREAMFLAGS_EVENTCALLBACK; a handle must
    // be registered before Start even though loopback delivers no events and
    // is polled instead.
    let _event = client.set_get_eventhandle().map_err(|error| {
        log.append(format!("[audio] set_event_handle failed: {error}\n").as_bytes());
        if is_loopback { "systemCapture" } else { "inputUnavailable" }
    })?;
    let capture: AudioCaptureClient = client.get_audiocaptureclient().map_err(|error| {
        log.append(format!("[audio] get_audiocaptureclient failed: {error}\n").as_bytes());
        if is_loopback { "systemCapture" } else { "inputUnavailable" }
    })?;
    client.start_stream().map_err(|error| {
        log.append(format!("[audio] start_stream failed: {error}\n").as_bytes());
        "systemCapture"
    })?;

    dispatch(packet(session, "listening", None, base, token));
    log.append(format!("[audio] capture started: source rate={rate} channels={channels}\n").as_bytes());

    let mut analyzer = Analyzer::new();
    let mut queue: VecDeque<u8> = VecDeque::with_capacity(8 * blockalign * FFT_SIZE);
    let mut mono: Vec<f32> = Vec::with_capacity(4096);
    let mut last_packet = Instant::now();
    let mut result = Ok(());
    loop {
        if GENERATION.load(Ordering::SeqCst) != generation {
            break;
        }
        if let Err(error) = capture.read_from_device_to_deque(&mut queue) {
            log.append(format!("[audio] capture read failed: {error}\n").as_bytes());
            result = Err(if is_loopback { "systemCapture" } else { "inputUnavailable" });
            break;
        }
        if queue.is_empty() {
            // Loopback delivers no data while every app is silent: keep the
            // listening status alive so the server does not time us out.
            if last_packet.elapsed() > Duration::from_secs(1) {
                dispatch(packet(session, "listening", None, base, token));
                last_packet = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(4));
            continue;
        }
        let frames = queue.len() / blockalign;
        let usable = frames * blockalign;
        mono.clear();
        let bytes: Vec<u8> = queue.drain(..usable).collect();
        queue.truncate(0);
        for frame in bytes.chunks_exact(blockalign) {
            let mut sum = 0.0f32;
            for channel in 0..channels {
                let offset = channel * 4;
                if offset + 4 <= frame.len() {
                    sum += f32::from_le_bytes([frame[offset], frame[offset + 1], frame[offset + 2], frame[offset + 3]]) / channels as f32;
                }
            }
            mono.push(sum);
        }
        if let Some(feature) = analyzer.consume(&mono, rate as f64) {
            let mut value = json!({
                "op": "audioPacket", "session": session, "status": "listening",
                "rms": feature.rms, "bands": feature.bands,
            });
            value["__base"] = json!(base);
            value["__token"] = json!(token);
            dispatch(value);
            last_packet = Instant::now();
        }
    }
    let _ = client.stop_stream();
    log.append(b"[audio] capture stopped\n");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_has_no_energy() {
        let mut analyzer = Analyzer::new();
        let silence = vec![0.0f32; FFT_SIZE];
        let feature = analyzer.consume_at(&silence, 48000.0, Instant::now()).expect("full buffer emits");
        assert!(feature.rms < 1e-6);
        assert!(feature.bands.iter().all(|band| *band < 1e-4));
    }

    #[test]
    fn sine_peaks_in_its_band() {
        let mut analyzer = Analyzer::new();
        let rate = 48000.0;
        // 440 Hz lands inside the 320–640 Hz band (index 3).
        let sine: Vec<f32> = (0..FFT_SIZE).map(|n| (2.0 * std::f32::consts::PI * 440.0 * n as f32 / rate as f32).sin() * 0.5).collect();
        let feature = analyzer.consume_at(&sine, rate, Instant::now()).expect("emits");
        assert!(feature.bands[3] > 0.05, "band 3 carries the tone: {:?}", feature.bands);
        assert!(feature.bands[3] > feature.bands[5], "tone does not leak into high bands");
        assert!(feature.rms > 0.3 && feature.rms < 0.4, "rms near 0.5/sqrt(2): {}", feature.rms);
    }

    #[test]
    fn emit_is_throttled_to_45ms() {
        let mut analyzer = Analyzer::new();
        let start = Instant::now();
        let samples = vec![0.1f32; FFT_SIZE];
        assert!(analyzer.consume_at(&samples, 48000.0, start).is_some());
        assert!(analyzer.consume_at(&samples, 48000.0, start + Duration::from_millis(10)).is_none());
        assert!(analyzer.consume_at(&samples, 48000.0, start + EMIT_INTERVAL).is_some());
    }

    #[test]
    fn partial_buffers_accumulate() {
        let mut analyzer = Analyzer::new();
        let now = Instant::now();
        let half = vec![0.2f32; FFT_SIZE / 2];
        assert!(analyzer.consume_at(&half, 48000.0, now).is_none());
        assert!(analyzer.consume_at(&half, 48000.0, now).is_some());
    }
}
