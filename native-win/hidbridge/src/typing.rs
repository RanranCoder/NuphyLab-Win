// Typing Sounds (模拟声浪), porting native/typing/{Model,Engine,Controller}.swift.
//
// Architecture notes for Windows:
// - A WH_KEYBOARD_LL hook (own pump thread, like hooks.rs) forwards raw key
//   events to a dedicated audio worker through a channel; the hook callback
//   only sends, it never decodes audio. Injected (synthetic) input is filtered
//   in the callback and auto-repeat is suppressed by the held-key set.
// - Samples of the selected pack are fully decoded to mono 48 kHz f32 buffers
//   once (the event path never reads files), mirroring TypingSoundBank.
// - The server owns persistence and the pause coordination; the bridge only
//   holds runtime state and receives the full preferences on every save.
// - Windows has no TCC permission gate and no Secure Input: inputMonitoring is
//   always true and the "secureInput" pause reason never occurs here.
//
// Pure logic (key groups, press dedup, patch validation, catalog validation,
// gain formulas, DSP primitives) is separated from Win32/rodio glue so the
// self-test and unit tests cover every branch without hooks or audio devices.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use windows_sys::Win32::Foundation::LPARAM;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, KBDLLHOOKSTRUCT, MSG, PostThreadMessageW, SetWindowsHookExW,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::device::diagnostic;
use crate::error::{failure, Result};

// ---------------------------------------------------------------------------
// Key groups (TypingKeyGroup port)

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum KeyGroup {
    Regular,
    Space,
    Enter,
    Backspace,
    ShiftLeft,
    ShiftRight,
    Tab,
    Caps,
}

pub const KEY_GROUPS: [KeyGroup; 8] = [
    KeyGroup::Regular,
    KeyGroup::Space,
    KeyGroup::Enter,
    KeyGroup::Backspace,
    KeyGroup::ShiftLeft,
    KeyGroup::ShiftRight,
    KeyGroup::Tab,
    KeyGroup::Caps,
];

impl KeyGroup {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyGroup::Regular => "regular",
            KeyGroup::Space => "space",
            KeyGroup::Enter => "enter",
            KeyGroup::Backspace => "backspace",
            KeyGroup::ShiftLeft => "shiftLeft",
            KeyGroup::ShiftRight => "shiftRight",
            KeyGroup::Tab => "tab",
            KeyGroup::Caps => "caps",
        }
    }

    pub fn from_name(name: &str) -> Option<KeyGroup> {
        KEY_GROUPS.iter().copied().find(|group| group.as_str() == name)
    }
}

/// VK code → sounding group. Mirrors TypingKeyGroup.key (Carbon codes): space,
/// enter (including the keypad enter, which shares VK_RETURN), backspace, tab,
/// the two physical shifts and caps lock sound distinctly; letters, digits,
/// punctuation, the numpad block and the arrow keys are regular; function
/// keys, media keys, Escape, navigation beyond arrows and the modifiers
/// (Ctrl/Alt/Win) stay silent.
pub fn key_group(vk: u16) -> Option<KeyGroup> {
    match vk {
        0x20 => Some(KeyGroup::Space),
        0x0D | 0x6C => Some(KeyGroup::Enter),
        0x08 => Some(KeyGroup::Backspace),
        0x09 => Some(KeyGroup::Tab),
        0xA0 => Some(KeyGroup::ShiftLeft),
        0xA1 => Some(KeyGroup::ShiftRight),
        0x14 => Some(KeyGroup::Caps),
        0x0C // VK_CLEAR (numpad 5; Mac Clear/keypad-equals are regular)
        | 0x25..=0x28 // arrows
        | 0x30..=0x39 // digits
        | 0x41..=0x5A // letters
        | 0x60..=0x6F // numpad block (0x6C handled above)
        | 0xBA..=0xC0 // ; = , - . / `
        | 0xDB..=0xDF // [ \ ] ' OEM_8
        | 0xE2 => Some(KeyGroup::Regular), // OEM_102 international backslash
        _ => None,
    }
}

/// Keypress dedup (TypingPressState port). A down only sounds when the code was
/// not held (this also filters Windows' auto-repeat, which the low-level hook
/// reports as repeated downs), an up only sounds when a down was observed.
#[derive(Default)]
pub struct PressState {
    held: HashSet<u16>,
}

impl PressState {
    pub fn down(&mut self, vk: u16, repeated: bool) -> Option<KeyGroup> {
        if repeated {
            return None;
        }
        let group = key_group(vk)?;
        self.held.insert(vk).then_some(group)
    }

    pub fn up(&mut self, vk: u16) -> Option<KeyGroup> {
        self.held.remove(&vk).then(|| key_group(vk)).flatten()
    }

    pub fn clear(&mut self) {
        self.held.clear();
    }
}

// ---------------------------------------------------------------------------
// Preferences (TypingSoundPreferences port)

const PREFERENCE_KEYS: [&str; 8] = ["enabled", "pack", "volume", "muted", "release", "variation", "gains", "favorites"];

#[derive(Clone, Debug, serde::Serialize)]
pub struct Preferences {
    pub enabled: bool,
    pub pack: String,
    pub volume: f64,
    pub muted: bool,
    pub release: f64,
    pub variation: bool,
    pub gains: BTreeMap<String, f64>,
    pub favorites: Vec<String>,
}

impl Default for Preferences {
    fn default() -> Self {
        Preferences {
            enabled: false,
            pack: "mahjong-crisp".into(),
            volume: 50.0,
            muted: false,
            release: 70.0,
            variation: true,
            gains: KEY_GROUPS
                .iter()
                .filter(|group| **group != KeyGroup::Regular)
                .map(|group| (group.as_str().to_string(), 100.0))
                .collect(),
            favorites: Vec::new(),
        }
    }
}

fn finite_number(value: &Value) -> Option<f64> {
    if value.is_boolean() {
        return None;
    }
    value.as_f64().filter(|number| number.is_finite())
}

impl Preferences {
    /// TypingSoundPreferences.patch: key-set validation, strict number/bool
    /// typing, bounded ranges and final pack/favorites membership checks.
    pub fn apply_patch(&self, patch: &Value, ids: &HashSet<&str>) -> Result<Preferences> {
        let invalid = || failure("INVALID_SETTINGS", "Typing sound settings are invalid.");
        let Some(object) = patch.as_object() else { return Err(invalid()) };
        for key in object.keys() {
            if !PREFERENCE_KEYS.contains(&key.as_str()) {
                return Err(invalid());
            }
        }
        let mut next = self.clone();
        for (key, value) in object {
            match key.as_str() {
                "volume" | "release" => {
                    let number = finite_number(value).ok_or_else(invalid)?;
                    if !(0.0..=100.0).contains(&number) {
                        return Err(invalid());
                    }
                    if key == "volume" {
                        next.volume = number;
                    } else {
                        next.release = number;
                    }
                }
                "enabled" | "muted" | "variation" => {
                    let flag = value.as_bool().ok_or_else(invalid)?;
                    match key.as_str() {
                        "enabled" => next.enabled = flag,
                        "muted" => next.muted = flag,
                        _ => next.variation = flag,
                    }
                }
                "gains" => {
                    let Some(map) = value.as_object() else { return Err(invalid()) };
                    for (group, value) in map {
                        if !next.gains.contains_key(group) {
                            return Err(invalid());
                        }
                        let number = finite_number(value).ok_or_else(invalid)?;
                        if !(0.0..=150.0).contains(&number) {
                            return Err(invalid());
                        }
                        next.gains.insert(group.clone(), number);
                    }
                }
                "pack" => {
                    let Some(pack) = value.as_str() else { return Err(invalid()) };
                    next.pack = pack.to_string();
                }
                "favorites" => {
                    let Some(list) = value.as_array() else { return Err(invalid()) };
                    let mut favorites = Vec::with_capacity(list.len());
                    for item in list {
                        let Some(pack) = item.as_str() else { return Err(invalid()) };
                        favorites.push(pack.to_string());
                    }
                    next.favorites = favorites;
                }
                _ => return Err(invalid()),
            }
        }
        if !ids.contains(next.pack.as_str())
            || next.favorites.len() > ids.len()
            || next.favorites.iter().collect::<HashSet<_>>().len() != next.favorites.len()
            || next.favorites.iter().any(|pack| !ids.contains(pack.as_str()))
        {
            return Err(invalid());
        }
        Ok(next)
    }

    /// masterGain: muted ? 0 : pow(volume/100, 1.7) * 0.65.
    pub fn master_gain(&self) -> f64 {
        if self.muted {
            0.0
        } else {
            (self.volume / 100.0).powf(1.7) * 0.65
        }
    }

    /// gain(group, release): (regular ? 1 : gains[group]/100) * (release ? release/100 : 1).
    pub fn gain(&self, group: KeyGroup, release: bool) -> f64 {
        let base = if group == KeyGroup::Regular {
            1.0
        } else {
            self.gains.get(group.as_str()).copied().unwrap_or(100.0) / 100.0
        };
        base * if release { self.release / 100.0 } else { 1.0 }
    }
}

// ---------------------------------------------------------------------------
// Catalog (TypingSoundPack / TypingSoundCatalog port)

#[derive(Clone, Debug)]
pub struct SoundPack {
    pub id: String,
    pub designed: Option<bool>,
    pub variants: Vec<i64>,
    pub samples: Option<BTreeMap<String, BTreeMap<String, Vec<String>>>>,
}

#[derive(Clone, Debug)]
pub struct Catalog {
    pub packs: Vec<SoundPack>,
    pub assets: Vec<String>,
}

#[derive(serde::Deserialize)]
struct CatalogFile {
    packs: Vec<SoundPackFile>,
    assets: Vec<String>,
}

#[derive(serde::Deserialize)]
struct SoundPackFile {
    id: String,
    #[serde(default)]
    designed: Option<bool>,
    #[serde(default)]
    variants: Vec<i64>,
    #[serde(default)]
    samples: Option<BTreeMap<String, BTreeMap<String, Vec<String>>>>,
}

/// Sample path list for a group/phase. Designed packs carry an explicit
/// `samples` table; the retro packs fall back to the GENERIC_R{variant} mp3
/// stems (press) and the dedicated stem per large key.
impl SoundPack {
    pub fn paths(&self, group: KeyGroup, release: bool) -> Vec<String> {
        let phase = if release { "release" } else { "press" };
        if let Some(samples) = &self.samples {
            return samples
                .get(group.as_str())
                .and_then(|phases| phases.get(phase))
                .cloned()
                .unwrap_or_default();
        }
        let stem = match group {
            KeyGroup::Space => "SPACE",
            KeyGroup::Enter => "ENTER",
            KeyGroup::Backspace | KeyGroup::ShiftLeft | KeyGroup::ShiftRight => "BACKSPACE",
            _ => "GENERIC",
        };
        if stem == "GENERIC" && !release {
            return self.variants.iter().map(|variant| format!("press/GENERIC_R{variant}.mp3")).collect();
        }
        vec![format!("{phase}/{stem}.mp3")]
    }
}

fn safe_pack_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn safe_sample_path(path: &str) -> bool {
    // Relative, forward-slash separated, no traversal: the loader joins
    // audio/<pack>/<path> and must never escape the pack directory.
    !path.is_empty()
        && !path.contains("..")
        && !path.contains('\\')
        && !path.contains(':')
        && !path.starts_with('/')
        && path.split('/').all(|segment| !segment.is_empty())
}

/// TypingSoundCatalog.validate: 18 unique packs, unique asset list, restricted
/// pack ids and every group/phase resolved to whitelisted sample paths.
pub fn validate_catalog(catalog: &Catalog) -> Result<()> {
    let invalid = || failure("INVALID_CATALOG", "The typing sound catalog is invalid.");
    if catalog.packs.len() != 18
        || catalog.packs.iter().map(|pack| pack.id.as_str()).collect::<HashSet<_>>().len() != catalog.packs.len()
        || catalog.assets.iter().collect::<HashSet<_>>().len() != catalog.assets.len()
    {
        return Err(invalid());
    }
    let allowed: HashSet<&str> = catalog.assets.iter().map(String::as_str).collect();
    for pack in &catalog.packs {
        if !safe_pack_id(&pack.id) {
            return Err(invalid());
        }
        for group in KEY_GROUPS {
            for release in [false, true] {
                let paths = pack.paths(group, release);
                if paths.is_empty() {
                    return Err(invalid());
                }
                for path in &paths {
                    if !safe_sample_path(path) || !allowed.contains(&format!("audio/{}/{}", pack.id, path).as_str()) {
                        return Err(invalid());
                    }
                }
            }
        }
    }
    Ok(())
}

/// Loads and validates catalog.json. Returns the typed catalog plus the public
/// JSON for the status snapshot (the raw object minus the "assets" list).
pub fn load_catalog(root: &Path) -> Result<(Catalog, Value)> {
    let text = std::fs::read_to_string(root.join("catalog.json"))
        .map_err(|_| failure("INVALID_CATALOG", "The typing sound catalog could not be read."))?;
    let raw: Value = serde_json::from_str(&text).map_err(|_| failure("INVALID_CATALOG", "The typing sound catalog is not valid JSON."))?;
    let file: CatalogFile = serde_json::from_value(raw.clone())
        .map_err(|_| failure("INVALID_CATALOG", "The typing sound catalog has unknown fields."))?;
    let catalog = Catalog {
        packs: file
            .packs
            .into_iter()
            .map(|pack| SoundPack { id: pack.id, designed: pack.designed, variants: pack.variants, samples: pack.samples })
            .collect(),
        assets: file.assets,
    };
    validate_catalog(&catalog)?;
    let mut public = raw;
    if let Some(object) = public.as_object_mut() {
        object.remove("assets");
    }
    Ok((catalog, public))
}

// ---------------------------------------------------------------------------
// DSP primitives (TypingSoundBank port)

fn biquad(samples: &[f32], hz: f64, peak: bool) -> Vec<f32> {
    let w = 2.0 * std::f64::consts::PI * hz / 48_000.0;
    let c = w.cos();
    let a = w.sin() / (2.0 * if peak { 1.7 } else { 0.5_f64.sqrt() });
    let big_a = 10f64.powf(3.0 / 40.0);
    let (b0, b1, b2, a0, a1, a2) = if peak {
        (1.0 + a * big_a, -2.0 * c, 1.0 - a * big_a, 1.0 + a / big_a, -2.0 * c, 1.0 - a / big_a)
    } else {
        ((1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0, 1.0 + a, -2.0 * c, 1.0 - a)
    };
    let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
    samples
        .iter()
        .map(|sample| {
            let value = *sample as f64;
            let y = (b0 * value + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2) / a0;
            x2 = x1;
            x1 = value;
            y2 = y1;
            y1 = y;
            y as f32
        })
        .collect()
}

/// Linear-interpolation playback-rate change (TypingSoundBank's rate factor):
/// output length is the truncated input length divided by the factor.
fn change_rate(samples: &[f32], rate: f64) -> Vec<f32> {
    if samples.is_empty() || (rate - 1.0).abs() < 1e-9 {
        return samples.to_vec();
    }
    let output_length = ((samples.len() as f64) / rate) as usize;
    let mut output = Vec::with_capacity(output_length);
    for index in 0..output_length {
        let position = index as f64 * rate;
        let left = (position as usize).min(samples.len() - 1);
        let right = (left + 1).min(samples.len() - 1);
        let fraction = (position - (position as usize) as f64) as f32;
        output.push(samples[left] + (samples[right] - samples[left]) * fraction);
    }
    output
}

/// Strips leading silence but keeps 48 samples of pre-attack air.
fn trim_leading(samples: &[f32]) -> Vec<f32> {
    let first = samples.iter().position(|value| value.abs() >= 0.001).unwrap_or(0);
    let cut = first.saturating_sub(48).min(samples.len());
    samples[cut..].to_vec()
}

/// Fades the tail so a cut-off release never clicks.
fn apply_fade(samples: &mut [f32], count: usize) {
    let count = count.min(samples.len());
    let length = samples.len();
    for index in 0..count {
        let position = length - count + index;
        samples[position] *= (count - 1 - index) as f32 / (count - 1).max(1) as f32;
    }
}

// ---------------------------------------------------------------------------
// Sound bank: decoded, normalized, pre-baked samples (TypingSoundBank port)

const MAX_FRAMES_PER_FILE: usize = 2_000_000;
const MAX_FILE_BYTES: usize = 16_000_000;

fn audio_error() -> crate::error::BridgeError {
    failure("AUDIO", "Typing sound samples could not be decoded.")
}

pub struct SoundBank {
    pub id: String,
    /// key = (group, release) → buffers; every buffer is mono 48 kHz f32 with
    /// normalization ×2 headroom already baked in (playback applies 0.5×).
    samples: BTreeMap<(KeyGroup, bool), Vec<Arc<[f32]>>>,
}

impl SoundBank {
    fn bank_key(group: KeyGroup, release: bool) -> String {
        format!("{}/{}", group.as_str(), if release { "release" } else { "press" })
    }

    pub fn buffers(&self, group: KeyGroup, release: bool) -> &[Arc<[f32]>] {
        static EMPTY: [Arc<[f32]>; 0] = [];
        self.samples.get(&(group, release)).map(Vec::as_slice).unwrap_or(&EMPTY)
    }

    /// Decodes every referenced sample once; synthesized large-key variants for
    /// non-designed packs mirror the Swift filter/resample treatment.
    pub fn load(pack: &SoundPack, root: &Path) -> Result<SoundBank> {
        let mut decoded: BTreeMap<String, Vec<f32>> = BTreeMap::new();
        let mut peak: f32 = 0.0;
        for group in KEY_GROUPS {
            for release in [false, true] {
                for path in pack.paths(group, release) {
                    if decoded.contains_key(&path) {
                        continue;
                    }
                    let file = root.join("audio").join(&pack.id).join(&path);
                    let bytes = std::fs::read(&file).map_err(|_| audio_error())?;
                    if bytes.len() > MAX_FILE_BYTES {
                        return Err(audio_error());
                    }
                    let mono = Self::decode_mono_48k(&bytes)?;
                    let trimmed = trim_leading(&mono);
                    if trimmed.is_empty() {
                        return Err(audio_error());
                    }
                    peak = peak.max(trimmed.iter().fold(0.0f32, |best, value| best.max(value.abs())));
                    decoded.insert(path, trimmed);
                }
            }
        }
        if peak <= 0.0 {
            return Err(audio_error());
        }
        let normalization = if pack.designed == Some(true) {
            1.0
        } else {
            (0.65 / (peak.max(0.01) as f64)).min(8.0) as f32
        };
        let mut samples = BTreeMap::new();
        for group in KEY_GROUPS {
            for release in [false, true] {
                let mut buffers = Vec::new();
                for path in pack.paths(group, release) {
                    let mut values = decoded[&path].clone();
                    if pack.designed != Some(true) && matches!(group, KeyGroup::ShiftLeft | KeyGroup::ShiftRight | KeyGroup::Tab | KeyGroup::Caps) {
                        let hz = match group {
                            KeyGroup::ShiftLeft => 620.0,
                            KeyGroup::ShiftRight => 720.0,
                            KeyGroup::Tab => 1100.0,
                            _ => 880.0,
                        };
                        values = biquad(&values, hz, true);
                        values = biquad(&values, if matches!(group, KeyGroup::ShiftLeft | KeyGroup::ShiftRight) { 7000.0 } else { 11000.0 }, false);
                        let rate = match group {
                            KeyGroup::ShiftLeft => 0.96,
                            KeyGroup::ShiftRight => 0.98,
                            KeyGroup::Tab => 1.02,
                            _ => 0.99,
                        };
                        values = change_rate(&values, rate);
                    }
                    apply_fade(&mut values, 144);
                    for value in values.iter_mut() {
                        *value *= normalization * 2.0;
                    }
                    buffers.push(Arc::from(values.into_boxed_slice()));
                }
                samples.insert((group, release), buffers);
            }
        }
        Ok(SoundBank { id: pack.id.clone(), samples })
    }

    /// Any rodio-supported file → mono 48 kHz f32. Channel mixdown averages the
    /// frame; other sample rates stretch linearly to 48 kHz. rodio sniffs the
    /// content and normalizes every decoder to i16, so the wav/mp3 features
    /// cover both catalog pack shapes.
    fn decode_mono_48k(bytes: &[u8]) -> Result<Vec<f32>> {
        use rodio::{Sample, Source};
        let cursor = std::io::Cursor::new(bytes.to_vec());
        let decoder = rodio::Decoder::new(cursor).map_err(|_| audio_error())?;
        let channels = decoder.channels().max(1) as usize;
        let rate = decoder.sample_rate().max(1);
        let raw: Vec<f32> = decoder.take(MAX_FRAMES_PER_FILE * channels).map(|sample| sample.to_f32()).collect();
        if raw.is_empty() {
            return Err(audio_error());
        }
        let frames = raw.len() / channels;
        let mut mono = Vec::with_capacity(frames);
        for frame in 0..frames {
            let start = frame * channels;
            let sum: f32 = raw[start..start + channels].iter().sum();
            mono.push(sum / channels as f32);
        }
        if rate == 48_000 {
            Ok(mono)
        } else {
            Ok(change_rate(&mono, rate as f64 / 48_000.0))
        }
    }
}

// ---------------------------------------------------------------------------
// Audio output (rodio) — up to 32 concurrent voices over one output stream.
//
// The cpal stream is !Send on windows-gnu, so the OutputStream lives on the
// audio worker thread itself: the runtime only asks the worker to ensure an
// output exists (EnsureOutput + reply channel) and then sends play commands.

/// Zero-copy source over the pre-baked buffer.
struct ArcSource {
    data: Arc<[f32]>,
    position: usize,
}

impl Iterator for ArcSource {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        let value = self.data.get(self.position).copied();
        if value.is_some() {
            self.position += 1;
        }
        value
    }
}

impl rodio::Source for ArcSource {
    fn current_frame_len(&self) -> Option<usize> {
        Some(self.data.len() - self.position.min(self.data.len()))
    }
    fn channels(&self) -> u16 {
        1
    }
    fn sample_rate(&self) -> u32 {
        48_000
    }
    fn total_duration(&self) -> Option<Duration> {
        Some(Duration::from_secs_f64(self.data.len() as f64 / 48_000.0))
    }
}

// Small deterministic-enough generator: std has no RNG and a dependency for
// ±3 % jitter and variant shuffling is not warranted.
struct SmallRng(u64);

impl SmallRng {
    fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            ^ ((std::process::id() as u64) << 32);
        SmallRng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u64() % bound as u64) as usize
        }
    }
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

// ---------------------------------------------------------------------------
// Audio worker: serial queue owning voices, dedup state and preview timeline.

enum AudioMessage {
    /// Raw low-level hook event; dedup happens on the worker.
    Hook { vk: u16, down: bool, at: Instant },
    /// Preview playback request (already schedule-gated by the token).
    Play { group: KeyGroup, release: bool },
    /// Ask the worker to create the shared output stream (first arm).
    EnsureOutput { reply: Sender<std::result::Result<(), String>> },
    Sync { bank: Option<Arc<SoundBank>>, prefs: Arc<Preferences>, active: bool },
    Preview { bank: Arc<SoundBank>, prefs: Arc<Preferences>, groups: Vec<KeyGroup>, token: u64 },
    PreviewDone { token: u64 },
    StopVoices,
}

struct AudioWorker {
    stream: Option<rodio::OutputStream>,
    out: Option<Arc<rodio::OutputStreamHandle>>,
    voices: Vec<rodio::Sink>,
    bank: Option<Arc<SoundBank>>,
    prefs: Arc<Preferences>,
    active: bool,
    previewing: bool,
    preview_token: u64,
    presses: PressState,
    previous: HashMap<String, usize>,
    rng: SmallRng,
}

impl AudioWorker {
    fn run(rx: Receiver<AudioMessage>, tx: Sender<AudioMessage>, preview_generation: Arc<AtomicU64>) {
        let mut worker = AudioWorker {
            stream: None,
            out: None,
            voices: Vec::new(),
            bank: None,
            prefs: Arc::new(Preferences::default()),
            active: false,
            previewing: false,
            preview_token: 0,
            presses: PressState::default(),
            previous: HashMap::new(),
            rng: SmallRng::new(),
        };
        while let Ok(message) = rx.recv() {
            match message {
                AudioMessage::Hook { vk, down, at } => {
                    // Mac receive() guard: previews, pauses and missing audio
                    // drop presses entirely; a stalled queue never replays old
                    // typing as a burst (80 ms staleness window).
                    if worker.previewing || !worker.active || worker.bank.is_none() {
                        worker.presses.clear();
                        continue;
                    }
                    if at.elapsed() > Duration::from_millis(80) {
                        worker.presses.clear();
                        continue;
                    }
                    let group = if down { worker.presses.down(vk, false) } else { worker.presses.up(vk) };
                    if let Some(group) = group {
                        worker.play(group, !down);
                    }
                }
                AudioMessage::Play { group, release } => {
                    worker.play(group, release);
                }
                AudioMessage::EnsureOutput { reply } => {
                    if worker.out.is_none() {
                        match rodio::OutputStream::try_default() {
                            Ok((stream, handle)) => {
                                // The stream must stay alive for playback; the
                                // worker thread owns it for the whole session.
                                worker.stream = Some(stream);
                                worker.out = Some(Arc::new(handle));
                                let _ = reply.send(Ok(()));
                            }
                            Err(error) => {
                                let _ = reply.send(Err(error.to_string()));
                            }
                        }
                    } else {
                        let _ = reply.send(Ok(()));
                    }
                }
                AudioMessage::Sync { bank, prefs, active } => {
                    worker.bank = bank;
                    worker.prefs = prefs;
                    worker.active = active;
                    if !active {
                        worker.presses.clear();
                    }
                }
                AudioMessage::Preview { bank, prefs, groups, token } => {
                    worker.stop_voices();
                    if worker.out.is_none() {
                        continue;
                    }
                    worker.previewing = true;
                    worker.preview_token = token;
                    worker.bank = Some(bank);
                    worker.prefs = prefs;
                    AudioWorker::schedule_preview(groups, token, Arc::clone(&preview_generation), tx.clone());
                }
                AudioMessage::PreviewDone { token } => {
                    if worker.preview_token == token {
                        worker.previewing = false;
                    }
                }
                AudioMessage::StopVoices => {
                    worker.stop_voices();
                }
            }
        }
    }

    fn stop_voices(&mut self) {
        for voice in self.voices.drain(..) {
            voice.stop();
        }
        self.presses.clear();
    }

    /// TypingSoundEngine.preview: 12 ms lead-in, 200 ms per group, the release
    /// 75 ms after its press; the timeline aborts when the generation moves.
    fn schedule_preview(groups: Vec<KeyGroup>, token: u64, preview_generation: Arc<AtomicU64>, tx: Sender<AudioMessage>) {
        std::thread::spawn(move || {
            let start = Instant::now();
            for (index, group) in groups.iter().enumerate() {
                for release in [false, true] {
                    let offset = Duration::from_millis(12 + index as u64 * 200 + u64::from(release) * 75);
                    // duration_since saturates to zero once the deadline passed.
                    std::thread::sleep((start + offset).duration_since(Instant::now()));
                    if preview_generation.load(Ordering::SeqCst) != token {
                        return;
                    }
                    let _ = tx.send(AudioMessage::Play { group: *group, release });
                }
            }
            let tail = Duration::from_millis(groups.len() as u64 * 200 + 600);
            std::thread::sleep((start + tail).duration_since(Instant::now()));
            let _ = tx.send(AudioMessage::PreviewDone { token });
        });
    }

    /// TypingSoundEngine.play: variant rotation avoiding the previous sample,
    /// ±3 % natural-variation jitter, per-voice volume = master × group × 0.5
    /// (the extra 2× headroom is baked into the float samples).
    fn play(&mut self, group: KeyGroup, release: bool) {
        let master = self.prefs.master_gain() as f32;
        if master <= 0.0 {
            return;
        }
        let (Some(out), Some(bank)) = (self.out.as_ref(), self.bank.as_ref()) else { return };
        let buffers = bank.buffers(group, release);
        if buffers.is_empty() {
            return;
        }
        let key = format!("{}{}", bank.id, SoundBank::bank_key(group, release));
        let last = self.previous.get(&key).copied().unwrap_or(usize::MAX);
        let index = if self.prefs.variation {
            let choices: Vec<usize> = (0..buffers.len()).filter(|candidate| *candidate != last).collect();
            let chosen = if choices.is_empty() { 0 } else { self.rng.below(choices.len()) };
            choices.into_iter().nth(chosen).unwrap_or(0)
        } else {
            buffers.len() / 2
        };
        self.previous.insert(key, index);
        let jitter = if self.prefs.variation { 0.97 + self.rng.unit() * 0.06 } else { 1.0 };
        let volume = master * self.prefs.gain(group, release) as f32 * 0.5 * jitter as f32;
        let Ok(sink) = rodio::Sink::try_new(out) else { return };
        sink.set_volume(volume);
        sink.append(ArcSource { data: Arc::clone(&buffers[index]), position: 0 });
        self.voices.retain(|voice| !voice.empty());
        while self.voices.len() >= 32 {
            if let Some(oldest) = self.voices.first() {
                oldest.stop();
            }
            self.voices.remove(0);
        }
        self.voices.push(sink);
    }
}

// ---------------------------------------------------------------------------
// WH_KEYBOARD_LL hook (TypingSoundController's CGEventTap port)

const LLKHF_INJECTED: u32 = 0x10;

thread_local! {
    static TYPING_EVENTS: std::cell::RefCell<Option<Sender<AudioMessage>>> = const { std::cell::RefCell::new(None) };
}

struct TypingHook {
    thread_id: u32,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TypingHook {
    /// Installs the hook on a dedicated pump thread; the install result is
    /// awaited so failures surface synchronously (Mac: CGEvent.tapCreate nil).
    fn spawn(events: Sender<AudioMessage>) -> Option<TypingHook> {
        let (ready_tx, ready_rx) = channel::<Option<u32>>();
        let handle = std::thread::spawn(move || unsafe {
            TYPING_EVENTS.with(|cell| *cell.borrow_mut() = Some(events));
            let thread_id = GetCurrentThreadId();
            let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(typing_hook_callback), std::ptr::null_mut(), 0);
            if hook.is_null() {
                let _ = ready_tx.send(None);
                diagnostic("[typing] SetWindowsHookExW failed");
                return;
            }
            let _ = ready_tx.send(Some(thread_id));
            let mut message: MSG = std::mem::zeroed();
            // Returns 0 on WM_QUIT; negative on failure — both end the pump.
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {}
            UnhookWindowsHookEx(hook);
        });
        match ready_rx.recv() {
            Ok(Some(thread_id)) => Some(TypingHook { thread_id, handle: Some(handle) }),
            _ => {
                let _ = handle.join();
                None
            }
        }
    }

    fn stop(&mut self) {
        unsafe {
            PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Listen-only: synthetic input never sounds and the event is always passed on.
unsafe extern "system" fn typing_hook_callback(code: i32, wparam: usize, lparam: LPARAM) -> isize {
    if code < 0 {
        return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
    }
    let info = &*(lparam as *const KBDLLHOOKSTRUCT);
    let down = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
    let up = matches!(wparam as u32, WM_KEYUP | WM_SYSKEYUP);
    if (down || up) && info.flags & LLKHF_INJECTED == 0 {
        TYPING_EVENTS.with(|cell| {
            if let Ok(borrowed) = cell.try_borrow() {
                if let Some(events) = borrowed.as_ref() {
                    let _ = events.send(AudioMessage::Hook { vk: info.vkCode as u16, down, at: Instant::now() });
                }
            }
        });
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

// ---------------------------------------------------------------------------
// Runtime (TypingSoundController port): catalog + preferences + hook + audio.

const PREVIEW_GROUPS: [KeyGroup; 13] = [
    KeyGroup::Regular,
    KeyGroup::Regular,
    KeyGroup::Regular,
    KeyGroup::Space,
    KeyGroup::Regular,
    KeyGroup::Regular,
    KeyGroup::Regular,
    KeyGroup::Regular,
    KeyGroup::Space,
    KeyGroup::Regular,
    KeyGroup::Regular,
    KeyGroup::Enter,
    KeyGroup::Backspace,
];

struct AudioLink {
    tx: Sender<AudioMessage>,
    preview_generation: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

struct TypingRuntime {
    catalog: Option<Catalog>,
    catalog_json: Value,
    preferences: Preferences,
    issue: String,
    suspended: HashSet<String>,
    bank: Option<Arc<SoundBank>>,
    hook: Option<TypingHook>,
    previewing: bool,
    audio: Option<AudioLink>,
    bank_cache: Vec<(String, Arc<SoundBank>)>,
    assets_root: Option<PathBuf>,
}

static TYPING: Mutex<Option<TypingRuntime>> = Mutex::new(None);

fn with_runtime<T>(operation: impl FnOnce(&mut TypingRuntime) -> Result<T>) -> Result<T> {
    let mut guard = TYPING.lock().unwrap();
    if guard.is_none() {
        let assets = std::env::var_os("AIR_STUDIO_TYPING_ASSETS")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty());
        *guard = Some(TypingRuntime::new(assets));
    }
    operation(guard.as_mut().expect("typing runtime"))
}

impl TypingRuntime {
    fn new(assets_root: Option<PathBuf>) -> TypingRuntime {
        let (mut catalog, mut catalog_json, mut issue) = (None, json!({}), String::new());
        if let Some(root) = &assets_root {
            match load_catalog(root) {
                Ok((loaded, public)) => {
                    catalog = Some(loaded);
                    catalog_json = public;
                }
                Err(_) => issue = "configuration".into(),
            }
        } else {
            issue = "configuration".into();
        }
        let (tx, rx) = channel::<AudioMessage>();
        let preview_generation = Arc::new(AtomicU64::new(0));
        let worker_generation = Arc::clone(&preview_generation);
        let worker_tx = tx.clone();
        let handle = std::thread::spawn(move || AudioWorker::run(rx, worker_tx, worker_generation));
        TypingRuntime {
            catalog,
            catalog_json,
            preferences: Preferences::default(),
            issue,
            suspended: HashSet::new(),
            bank: None,
            hook: None,
            previewing: false,
            audio: Some(AudioLink { tx, preview_generation, handle: Some(handle) }),
            bank_cache: Vec::new(),
            assets_root,
        }
    }

    fn send_audio(&self, message: AudioMessage) {
        if let Some(link) = &self.audio {
            let _ = link.tx.send(message);
        }
    }

    fn pack_ids(&self) -> HashSet<&str> {
        self.catalog
            .as_ref()
            .map(|catalog| catalog.packs.iter().map(|pack| pack.id.as_str()).collect())
            .unwrap_or_default()
    }

    fn load_bank(&mut self, pack_id: &str) -> Result<Arc<SoundBank>> {
        if let Some(position) = self.bank_cache.iter().position(|(id, _)| id == pack_id) {
            let (_, bank) = self.bank_cache.remove(position);
            self.bank_cache.insert(0, (pack_id.to_string(), Arc::clone(&bank)));
            return Ok(bank);
        }
        let pack = self
            .catalog
            .as_ref()
            .and_then(|catalog| catalog.packs.iter().find(|pack| pack.id == pack_id))
            .ok_or_else(|| failure("UNAVAILABLE", "Typing sound pack not found."))?
            .clone();
        let root = self.assets_root.clone().ok_or_else(audio_error)?;
        let bank = Arc::new(SoundBank::load(&pack, &root)?);
        self.bank_cache.insert(0, (pack_id.to_string(), Arc::clone(&bank)));
        while self.bank_cache.len() > 3 {
            self.bank_cache.pop();
        }
        Ok(bank)
    }

    /// TypingSoundController.interrupt: stop voices and invalidate previews.
    /// The hook keeps running across pack changes; disable uninstalls it in
    /// reconcile().
    fn interrupt(&mut self) {
        self.previewing = false;
        if let Some(link) = &self.audio {
            link.preview_generation.fetch_add(1, Ordering::SeqCst);
        }
        self.send_audio(AudioMessage::StopVoices);
    }

    /// TypingSoundController.reconcile: arm (or disarm) hook, bank and audio
    /// worker for the current preferences and pause set. Synchronous because
    /// the JSONL loop is serial anyway; decoding one pack is well under a
    /// second.
    fn reconcile(&mut self) {
        if !self.preferences.enabled {
            if let Some(mut hook) = self.hook.take() {
                hook.stop();
            }
            self.bank = None;
            self.send_audio(AudioMessage::Sync { bank: None, prefs: Arc::new(self.preferences.clone()), active: false });
            self.send_audio(AudioMessage::StopVoices);
            return;
        }
        if !self.suspended.is_empty() {
            // Paused: the hook may keep running but nothing may sound.
            self.send_audio(AudioMessage::StopVoices);
            self.send_audio(AudioMessage::Sync { bank: self.bank.clone(), prefs: Arc::new(self.preferences.clone()), active: false });
            return;
        }
        if self.bank.as_ref().map(|bank| bank.id.as_str()) != Some(self.preferences.pack.as_str()) {
            let pack = self.preferences.pack.clone();
            match self.load_bank(&pack) {
                Ok(bank) => self.bank = Some(bank),
                Err(_) => {
                    self.issue = "audio".into();
                    return;
                }
            }
        }
        match self.ensure_output() {
            Ok(()) => {}
            Err(_) => {
                self.issue = "audio".into();
                return;
            }
        }
        if self.hook.is_none() {
            let events = self.audio.as_ref().map(|link| link.tx.clone());
            if let Some(events) = events {
                self.hook = TypingHook::spawn(events);
            }
        }
        let active = self.hook.is_some();
        self.send_audio(AudioMessage::Sync { bank: self.bank.clone(), prefs: Arc::new(self.preferences.clone()), active });
    }

    /// Asks the worker to create its output stream and waits for the reply so
    /// failures surface synchronously (Mac: engine.prepare completion).
    fn ensure_output(&mut self) -> Result<()> {
        let Some(link) = &self.audio else { return Err(audio_error()) };
        let (reply_tx, reply_rx) = channel::<std::result::Result<(), String>>();
        if link.tx.send(AudioMessage::EnsureOutput { reply: reply_tx }).is_err() {
            return Err(audio_error());
        }
        match reply_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) | Err(_) => Err(audio_error()),
        }
    }

    fn state(&self) -> &'static str {
        if !self.issue.is_empty() {
            "error"
        } else if !self.preferences.enabled {
            "off"
        } else if !self.suspended.is_empty() {
            "paused"
        } else if self.bank.is_none() {
            "preparing"
        } else if self.hook.is_none() {
            "paused"
        } else {
            "ready"
        }
    }

    fn reason(&self) -> String {
        if !self.issue.is_empty() {
            return self.issue.clone();
        }
        for known in ["session", "dictation", "recording"] {
            if self.suspended.contains(known) {
                return known.to_string();
            }
        }
        if let Some(other) = self.suspended.iter().next() {
            return other.clone();
        }
        if self.hook.is_none() {
            return "listener".into();
        }
        String::new()
    }

    fn snapshot(&self) -> Value {
        json!({
            "preferences": self.preferences,
            "state": self.state(),
            "reason": self.reason(),
            "inputMonitoring": true,
            "previewing": self.previewing,
            "catalog": self.catalog_json,
            "issue": self.issue,
        })
    }

    fn stop_preview(&mut self) {
        if let Some(link) = &self.audio {
            link.preview_generation.fetch_add(1, Ordering::SeqCst);
        }
        self.previewing = false;
        self.send_audio(AudioMessage::StopVoices);
    }
}

// ---------------------------------------------------------------------------
// JSONL ops

pub fn status() -> Result<Value> {
    with_runtime(|runtime| Ok(runtime.snapshot()))
}

pub fn save(patch: Option<&Value>) -> Result<Value> {
    with_runtime(|runtime| {
        let invalid = || failure("INVALID_SETTINGS", "Typing sound settings are invalid.");
        let Some(patch) = patch else { return Err(invalid()) };
        let next = {
            let ids = runtime.pack_ids();
            if ids.is_empty() {
                return Err(invalid());
            }
            runtime.preferences.apply_patch(patch, &ids)?
        };
        let reset = next.enabled != runtime.preferences.enabled || next.pack != runtime.preferences.pack;
        runtime.preferences = next;
        runtime.issue.clear();
        if reset {
            runtime.interrupt();
        }
        runtime.reconcile();
        Ok(runtime.snapshot())
    })
}

pub fn preview(pack: Option<&Value>, group: Option<&Value>) -> Result<Value> {
    with_runtime(|runtime| {
        let unavailable = || failure("UNAVAILABLE", "Typing sound preview is unavailable.");
        let invalid = || failure("INVALID_SETTINGS", "Typing sound preview group is invalid.");
        let Some(pack_id) = pack.and_then(Value::as_str).filter(|id| !id.is_empty()) else {
            return Err(unavailable());
        };
        let groups: Vec<KeyGroup> = match group {
            None => PREVIEW_GROUPS.to_vec(),
            Some(raw) => {
                let name = raw.as_str().ok_or_else(invalid)?;
                vec![KeyGroup::from_name(name).ok_or_else(invalid)?]
            }
        };
        let bank = runtime.load_bank(pack_id)?;
        runtime.ensure_output()?;
        let (tx, generation) = {
            let Some(link) = &runtime.audio else { return Err(audio_error()) };
            (link.tx.clone(), Arc::clone(&link.preview_generation))
        };
        runtime.stop_preview();
        let token = generation.fetch_add(1, Ordering::SeqCst) + 1;
        runtime.previewing = true;
        let _ = tx.send(AudioMessage::Preview { bank, prefs: Arc::new(runtime.preferences.clone()), groups: groups.clone(), token });
        // The status flag clears on the same schedule the worker's timeline
        // completes on (Mac: engine.preview completion callback).
        let generation = Arc::clone(&generation);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(groups.len() as u64 * 200 + 600));
            if generation.load(Ordering::SeqCst) != token {
                return;
            }
            if let Ok(mut guard) = TYPING.lock() {
                if let Some(runtime) = guard.as_mut() {
                    if runtime.audio.as_ref().map(|link| link.preview_generation.load(Ordering::SeqCst)) == Some(token) {
                        runtime.previewing = false;
                    }
                }
            }
        });
        Ok(runtime.snapshot())
    })
}

pub fn stop_preview() -> Result<Value> {
    with_runtime(|runtime| {
        runtime.stop_preview();
        Ok(runtime.snapshot())
    })
}

pub fn set_paused(reason: Option<&Value>, active: bool) -> Result<Value> {
    with_runtime(|runtime| {
        let name = reason.and_then(Value::as_str).unwrap_or_default().to_string();
        if !name.is_empty() {
            if active {
                runtime.suspended.insert(name);
            } else {
                runtime.suspended.remove(&name);
            }
        } else if !active {
            runtime.suspended.clear();
        }
        runtime.reconcile();
        Ok(runtime.snapshot())
    })
}

/// Bridge shutdown: unhook, stop voices and drop the worker. Called from the
/// same shutdown path as hooks::stop_all().
pub fn shutdown() {
    if let Ok(mut guard) = TYPING.lock() {
        if let Some(mut runtime) = guard.take() {
            if let Some(mut hook) = runtime.hook.take() {
                hook.stop();
            }
            runtime.send_audio(AudioMessage::StopVoices);
            // Dropping the sender ends the worker loop once the queue drains;
            // join only when it already finished to keep shutdown snappy.
            if let Some(link) = runtime.audio.take() {
                if let Some(handle) = link.handle {
                    if handle.is_finished() {
                        let _ = handle.join();
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::BridgeError;

    fn pack_ids(count: usize) -> HashSet<&'static str> {
        static NAMES: [&str; 18] = [
            "mahjong-crisp", "mahjong-porcelain", "thock-deep", "cream-milk", "cream-butter", "cream-pudding",
            "wood-bamboo", "wood-walnut", "wood-ebony", "soft-velvet", "soft-cushion", "soft-light",
            "holypanda", "mxbrown", "topre", "boxnavy", "bluealps", "buckling",
        ];
        NAMES[..count].iter().copied().collect()
    }

    #[test]
    fn key_group_mapping_follows_mac_semantics() {
        assert_eq!(key_group(0x20), Some(KeyGroup::Space));
        assert_eq!(key_group(0x0D), Some(KeyGroup::Enter), "Return");
        assert_eq!(key_group(0x6C), Some(KeyGroup::Enter), "keypad enter shares Return");
        assert_eq!(key_group(0x08), Some(KeyGroup::Backspace));
        assert_eq!(key_group(0x09), Some(KeyGroup::Tab));
        assert_eq!(key_group(0xA0), Some(KeyGroup::ShiftLeft));
        assert_eq!(key_group(0xA1), Some(KeyGroup::ShiftRight));
        assert_eq!(key_group(0x14), Some(KeyGroup::Caps));
        assert_eq!(key_group(0x41), Some(KeyGroup::Regular), "A");
        assert_eq!(key_group(0x31), Some(KeyGroup::Regular), "1");
        assert_eq!(key_group(0xBA), Some(KeyGroup::Regular), "semicolon");
        assert_eq!(key_group(0x25), Some(KeyGroup::Regular), "Left arrow sounds like Mac 123");
        assert_eq!(key_group(0x65), Some(KeyGroup::Regular), "numpad 5");
        // Silent keys: F keys, modifiers, Escape, navigation beyond arrows, media.
        for silent in [0x1Bu16, 0x11, 0x12, 0x10, 0x5B, 0x5C, 0x70, 0x87, 0x2E, 0x24, 0x90, 0xB5] {
            assert_eq!(key_group(silent), None, "VK {silent:#04x} must stay silent");
        }
        assert_eq!(KeyGroup::from_name("shiftLeft"), Some(KeyGroup::ShiftLeft));
        assert_eq!(KeyGroup::from_name("regular"), Some(KeyGroup::Regular));
        assert_eq!(KeyGroup::from_name("wheel"), None);
    }

    #[test]
    fn press_state_dedups_held_keys_and_filters_repeat() {
        let mut presses = PressState::default();
        assert_eq!(presses.down(0x41, false), Some(KeyGroup::Regular), "first insert sounds");
        assert_eq!(presses.down(0x41, false), None, "held key down again is auto-repeat");
        assert_eq!(presses.down(0x41, true), None, "explicit repeat flag never sounds");
        assert_eq!(presses.up(0x41), Some(KeyGroup::Regular), "release after press sounds");
        assert_eq!(presses.up(0x41), None, "stray release is silent");
        assert_eq!(presses.down(0x11, false), None, "silent groups never sound");
        assert_eq!(presses.up(0x11), None, "and never enter the held set");
        assert_eq!(presses.down(0x20, false), Some(KeyGroup::Space));
        presses.clear();
        assert_eq!(presses.up(0x20), None, "clear drops every held key");
    }

    #[test]
    fn patch_validation_enforces_mac_boundaries() {
        let prefs = Preferences::default();
        let ids = pack_ids(18);
        // Numbers: bounds and types.
        assert!(prefs.apply_patch(&json!({"volume": 0}), &ids).is_ok());
        assert!(prefs.apply_patch(&json!({"volume": 100}), &ids).is_ok());
        assert!(prefs.apply_patch(&json!({"release": 100.5}), &ids).is_err());
        assert!(prefs.apply_patch(&json!({"volume": -1}), &ids).is_err());
        assert!(prefs.apply_patch(&json!({"volume": true}), &ids).is_err());
        assert!(prefs.apply_patch(&json!({"volume": "50"}), &ids).is_err());
        // Booleans must be booleans.
        assert!(prefs.apply_patch(&json!({"enabled": 1}), &ids).is_err());
        assert!(prefs.apply_patch(&json!({"muted": false, "variation": true}), &ids).is_ok());
        // Gains: 0..150 and only the seven large groups.
        assert!(prefs.apply_patch(&json!({"gains": {"caps": 150}}), &ids).is_ok());
        assert!(prefs.apply_patch(&json!({"gains": {"space": 150.5}}), &ids).is_err());
        assert!(prefs.apply_patch(&json!({"gains": {"regular": 100}}), &ids).is_err());
        assert!(prefs.apply_patch(&json!({"gains": {"wheel": 100}}), &ids).is_err());
        // Unknown top-level keys and malformed containers.
        assert!(prefs.apply_patch(&json!({"volume2": 50}), &ids).is_err());
        assert!(prefs.apply_patch(&json!([]), &ids).is_err());
        assert!(prefs.apply_patch(&json!({"favorites": ["mahjong-crisp"]}), &ids).is_ok());
        assert!(prefs.apply_patch(&json!({"favorites": ["mahjong-crisp", "mahjong-crisp"]}), &ids).is_err());
        assert!(prefs.apply_patch(&json!({"favorites": ["nope"]}), &ids).is_err());
        // Pack membership: default pack and the 18-pack catalog.
        assert!(prefs.apply_patch(&json!({"pack": "wood-bamboo"}), &ids).is_ok());
        assert!(prefs.apply_patch(&json!({"pack": "unknown"}), &ids).is_err());
        let missing = pack_ids(17);
        assert!(prefs.apply_patch(&json!({"pack": "buckling"}), &missing).is_err(), "buckling is outside the first 17 ids");
        // The error code maps to the UI's "settings" issue label.
        let error: BridgeError = prefs.apply_patch(&json!({"volume": 999}), &ids).unwrap_err();
        assert_eq!(error.code, "INVALID_SETTINGS");
    }

    #[test]
    fn gain_formulas_match_swift() {
        let mut prefs = Preferences::default();
        prefs.volume = 50.0;
        prefs.release = 70.0;
        assert!((prefs.master_gain() - (0.5f64.powf(1.7) * 0.65)).abs() < 1e-9, "pow(volume/100,1.7)*0.65");
        assert!((prefs.gain(KeyGroup::Regular, false) - 1.0).abs() < 1e-9);
        assert!((prefs.gain(KeyGroup::Regular, true) - 0.7).abs() < 1e-9);
        assert!((prefs.gain(KeyGroup::Space, false) - 1.0).abs() < 1e-9, "default large-key gain 100");
        prefs.gains.insert("space".into(), 150.0);
        assert!((prefs.gain(KeyGroup::Space, false) - 1.5).abs() < 1e-9);
        assert!((prefs.gain(KeyGroup::Space, true) - 1.05).abs() < 1e-9, "150% × release 70%");
        prefs.muted = true;
        assert_eq!(prefs.master_gain(), 0.0);
        prefs.muted = false;
        prefs.volume = 0.0;
        assert_eq!(prefs.master_gain(), 0.0);
        assert!((prefs.gain(KeyGroup::Caps, true) - 0.7).abs() < 1e-9, "gain is independent of master");
    }

    fn synthetic_catalog() -> Value {
        let mut packs = Vec::new();
        let mut assets = Vec::new();
        for index in 0..18 {
            let id = format!("pack-{index}");
            packs.push(json!({
                "id": id,
                "designed": true,
                "variants": [0],
                "samples": {
                    "regular": {"press": ["press/regular_0.wav"], "release": ["release/regular_0.wav"]},
                    "space": {"press": ["press/space_0.wav"], "release": ["release/space_0.wav"]},
                    "enter": {"press": ["press/enter_0.wav"], "release": ["release/enter_0.wav"]},
                    "backspace": {"press": ["press/backspace_0.wav"], "release": ["release/backspace_0.wav"]},
                    "shiftLeft": {"press": ["press/shiftLeft_0.wav"], "release": ["release/shiftLeft_0.wav"]},
                    "shiftRight": {"press": ["press/shiftRight_0.wav"], "release": ["release/shiftRight_0.wav"]},
                    "tab": {"press": ["press/tab_0.wav"], "release": ["release/tab_0.wav"]},
                    "caps": {"press": ["press/caps_0.wav"], "release": ["release/caps_0.wav"]}
                }
            }));
            for group in ["regular", "space", "enter", "backspace", "shiftLeft", "shiftRight", "tab", "caps"] {
                for phase in ["press", "release"] {
                    assets.push(format!("audio/{id}/{phase}/{group}_0.wav"));
                }
            }
        }
        json!({"packs": packs, "assets": assets})
    }

    fn parse_catalog(value: &Value) -> Result<Catalog> {
        let file: CatalogFile = serde_json::from_value(value.clone()).map_err(|_| failure("INVALID_CATALOG", "bad fixture"))?;
        let catalog = Catalog {
            packs: file
                .packs
                .into_iter()
                .map(|pack| SoundPack { id: pack.id, designed: pack.designed, variants: pack.variants, samples: pack.samples })
                .collect(),
            assets: file.assets,
        };
        validate_catalog(&catalog)?;
        Ok(catalog)
    }

    #[test]
    fn catalog_validation_rejects_broken_catalogs() {
        let value = synthetic_catalog();
        let catalog = parse_catalog(&value).expect("synthetic catalog validates");
        assert_eq!(catalog.packs.len(), 18);
        // 17 packs are rejected outright.
        let mut short = value.clone();
        short["packs"].as_array_mut().unwrap().pop();
        assert!(parse_catalog(&short).is_err());
        // Duplicate ids.
        let mut duplicated = value.clone();
        duplicated["packs"][17]["id"] = json!("pack-0");
        assert!(parse_catalog(&duplicated).is_err());
        // Path traversal and absolute paths.
        let mut traversal = value.clone();
        traversal["packs"][0]["samples"]["regular"]["press"][0] = json!("press/../secret.wav");
        traversal["assets"].as_array_mut().unwrap().push(json!("audio/pack-0/press/../secret.wav"));
        assert!(parse_catalog(&traversal).is_err());
        let mut absolute = value.clone();
        absolute["packs"][1]["samples"]["space"]["press"][0] = json!("/etc/space.wav");
        absolute["assets"].as_array_mut().unwrap().push(json!("audio/pack-1//etc/space.wav"));
        assert!(parse_catalog(&absolute).is_err());
        // Every referenced path must be whitelisted in the asset list.
        let mut unlisted = value.clone();
        unlisted["assets"].as_array_mut().unwrap().pop();
        assert!(parse_catalog(&unlisted).is_err());
        // Upper-case ids are outside the whitelist.
        let mut upper = value.clone();
        upper["packs"][2]["id"] = json!("Pack-2");
        assert!(parse_catalog(&upper).is_err());
        // A group without any sample path fails closed.
        let mut incomplete = value.clone();
        incomplete["packs"][3]["samples"]["caps"] = json!({});
        assert!(parse_catalog(&incomplete).is_err());
    }

    #[test]
    fn pack_paths_cover_both_catalog_shapes() {
        let value = synthetic_catalog();
        let catalog = parse_catalog(&value).expect("catalog");
        let designed = &catalog.packs[0];
        assert_eq!(designed.paths(KeyGroup::Regular, false), vec!["press/regular_0.wav"]);
        assert_eq!(designed.paths(KeyGroup::Caps, true), vec!["release/caps_0.wav"]);
        // Retro packs (no samples table) fall back to the GENERIC stems.
        let retro = SoundPack { id: "holypanda".into(), designed: None, variants: vec![0, 1, 2, 3, 4], samples: None };
        assert_eq!(
            retro.paths(KeyGroup::Regular, false),
            vec!["press/GENERIC_R0.mp3", "press/GENERIC_R1.mp3", "press/GENERIC_R2.mp3", "press/GENERIC_R3.mp3", "press/GENERIC_R4.mp3"]
        );
        assert_eq!(retro.paths(KeyGroup::Regular, true), vec!["release/GENERIC.mp3"]);
        assert_eq!(retro.paths(KeyGroup::Space, false), vec!["press/SPACE.mp3"]);
        assert_eq!(retro.paths(KeyGroup::Backspace, true), vec!["release/BACKSPACE.mp3"]);
        assert_eq!(retro.paths(KeyGroup::ShiftLeft, true), vec!["release/BACKSPACE.mp3"]);
    }

    #[test]
    fn dsp_primitives_keep_tail_clean_and_resample_lengths() {
        // Trim keeps 48 samples of pre-attack air.
        let mut samples = vec![0.0f32; 100];
        samples[50..].fill(0.5);
        let trimmed = trim_leading(&samples);
        assert_eq!(trimmed.len(), 98, "first loud index 50 minus 48 pre-samples");
        assert!(trimmed.iter().take(46).all(|value| value.abs() < 0.001));
        // Fade zeroes the final sample and tapers before it.
        let mut fading = vec![1.0f32; 10];
        apply_fade(&mut fading, 4);
        assert_eq!(fading[9], 0.0, "final sample multiplied by zero");
        assert!(fading[8] > 0.0 && fading[8] < fading[7] && fading[7] < 1.0);
        // A short buffer fades over its whole length without panicking.
        let mut tiny = vec![1.0f32; 3];
        apply_fade(&mut tiny, 144);
        assert_eq!(tiny[2], 0.0);
        // Rate change truncates to floor(count / rate).
        assert_eq!(change_rate(&vec![1.0f32; 10], 0.5).len(), 20);
        assert_eq!(change_rate(&vec![1.0f32; 10], 2.0).len(), 5);
        assert_eq!(change_rate(&[1.0f32], 0.96), vec![1.0f32]);
    }

    #[test]
    fn catalog_public_json_strips_assets() {
        let dir = std::env::temp_dir().join(format!("typing-catalog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("catalog.json"), serde_json::to_vec(&synthetic_catalog()).unwrap()).expect("catalog fixture");
        let (catalog, public) = load_catalog(&dir).expect("load");
        assert_eq!(catalog.packs.len(), 18);
        assert!(public.get("assets").is_none(), "the asset list never reaches the UI");
        assert_eq!(public.get("packs").and_then(Value::as_array).map(Vec::len), Some(18));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bank_load_decodes_wav_without_an_audio_device() {
        // The bank loader is pure decoding: no output stream is created. The
        // fixture is WAV content inside the retro pack's GENERIC_R stem name to
        // prove rodio sniffs content, not extension.
        let dir = std::env::temp_dir().join(format!("typing-bank-{}", std::process::id()));
        let pack_dir = dir.join("audio").join("unit-pack");
        // The retro shape references press/{GENERIC_R0,SPACE,ENTER,BACKSPACE}.mp3
        // and release/{GENERIC,SPACE,ENTER,BACKSPACE}.mp3; write wav content in
        // every stem to prove rodio sniffs content, not extension.
        let spec = hound::WavSpec { channels: 1, sample_rate: 48_000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        for (phase, stems) in [
            ("press", vec!["GENERIC_R0.mp3", "SPACE.mp3", "ENTER.mp3", "BACKSPACE.mp3"]),
            ("release", vec!["GENERIC.mp3", "SPACE.mp3", "ENTER.mp3", "BACKSPACE.mp3"]),
        ] {
            std::fs::create_dir_all(pack_dir.join(phase)).expect("pack dir");
            for stem in stems {
                let mut writer = hound::WavWriter::create(pack_dir.join(phase).join(stem), spec).expect("writer");
                for frame in 0..9_600u32 {
                    // Quiet lead-in below the trim threshold, then a clear tone.
                    let lead = if frame < 48 { 0.0005 } else { 0.5 };
                    let value = (frame as f32 * 0.02).sin() * lead;
                    writer.write_sample((value * i16::MAX as f32) as i16).expect("sample");
                }
                drop(writer);
            }
        }
        let pack = SoundPack { id: "unit-pack".into(), designed: None, variants: vec![0], samples: None };
        let bank = SoundBank::load(&pack, &dir).expect("bank loads");
        assert_eq!(bank.buffers(KeyGroup::Regular, false).len(), 1);
        assert!(!bank.buffers(KeyGroup::Regular, false)[0].is_empty());
        // Non-designed packs synthesize the shift variant through the filter chain.
        assert_eq!(bank.buffers(KeyGroup::ShiftLeft, false).len(), 1);
        assert_eq!(bank.buffers(KeyGroup::Space, true).len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
