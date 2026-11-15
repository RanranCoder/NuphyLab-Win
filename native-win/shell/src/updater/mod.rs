// In-app updater (Windows port of the Sparkle flow): polls the signed appcast
// feed, compares versions, downloads the Inno Setup installer into a private
// directory, verifies SHA-256 + Ed25519 and hands the launch over to the
// graceful-quit chain (server restores the keyboard first, then the shell
// exits and the installer runs outside the shell's kill-on-close job object).
//
// Ownership follows the selectionLab pattern: one worker thread answers the
// `appUpdate` bridge ops and reports state changes back through UserEvent so
// the WebView (which only runs on the event-loop thread) can be pushed fresh
// status.

pub mod crypto;
pub mod feed;
#[cfg(test)]
mod tests;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tao::event_loop::EventLoopProxy;

use crate::worker::WorkerLog;
use crate::UserEvent;
use feed::UpdateItem;

/// Production feed: the latest published release asset on GitHub. Override
/// with AIR_STUDIO_UPDATE_FEED (http(s) or file:/// for local testing); the
/// values "off"/"none"/"disabled" turn the updater off entirely.
pub const DEFAULT_FEED_URL: &str = "https://github.com/bohu8264/NuphyLab-Win/releases/latest/download/appcast-win.xml";
/// First automatic check after launch (single silent pass; manual checks
/// always stay available). AIR_STUDIO_NO_UPDATE_CHECK=1 disables it.
pub const AUTO_CHECK_DELAY: Duration = Duration::from_secs(600);

const FEED_TIMEOUT: Duration = Duration::from_secs(10);
const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Hard ceiling per download: the declared length plus slack, so a hostile or
/// broken server cannot grow the file forever.
const DOWNLOAD_SLACK_BYTES: u64 = 16 * 1024 * 1024;

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Resolves the feed endpoint for this run: None means the updater is off.
pub fn resolve_feed_url(override_value: Option<&str>) -> Option<String> {
    let configured = override_value
        .map(str::to_owned)
        .or_else(|| std::env::var("AIR_STUDIO_UPDATE_FEED").ok().filter(|value| !value.trim().is_empty()))
        .unwrap_or_else(|| DEFAULT_FEED_URL.to_string());
    if matches!(configured.trim(), "off" | "none" | "disabled" | "0") {
        return None;
    }
    Some(configured)
}

fn auto_check_enabled() -> bool {
    std::env::var("AIR_STUDIO_NO_UPDATE_CHECK").ok().as_deref() != Some("1")
}

/// Updater phases surfaced to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Checking,
    Available,
    Downloading,
    Verified,
    Installing,
    Error,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Checking => "checking",
            Phase::Available => "available",
            Phase::Downloading => "downloading",
            Phase::Verified => "verified",
            Phase::Installing => "installing",
            Phase::Error => "error",
        }
    }
}

pub struct UpdaterCore {
    pub data_dir: PathBuf,
    pub phase: Phase,
    /// Last check found nothing newer than the running build.
    pub up_to_date: bool,
    /// The newest remote item once a check found one.
    pub remote: Option<UpdateItem>,
    /// Download progress in percent (downloading only).
    pub progress: u8,
    /// Bilingual failure reason for the error phase.
    pub error: Option<(String, String)>,
    /// Verified installer path (phase = verified).
    pub setup_path: Option<PathBuf>,
    /// Feed override supplied by the bridge check op (QA affordance).
    pub feed_override: Option<String>,
}

impl UpdaterCore {
    fn open(data_dir: &Path) -> UpdaterCore {
        UpdaterCore {
            data_dir: data_dir.to_path_buf(),
            phase: Phase::Idle,
            up_to_date: false,
            remote: None,
            progress: 0,
            error: None,
            setup_path: None,
            feed_override: None,
        }
    }

    fn reset(&mut self, phase: Phase) {
        self.phase = phase;
        self.error = None;
        if phase == Phase::Idle {
            self.remote = None;
            self.setup_path = None;
            self.progress = 0;
        }
    }

    fn fail(&mut self, zh: &str, en: &str) {
        self.phase = Phase::Error;
        self.error = Some((zh.to_string(), en.to_string()));
        self.progress = 0;
    }

    fn snapshot(&self) -> Value {
        let downloading = self.phase == Phase::Downloading;
        json!({
            "phase": self.phase.as_str(),
            "upToDate": self.up_to_date,
            "currentVersion": current_version(),
            "version": self.remote.as_ref().map(|item| item.version.clone()),
            "progress": downloading.then_some(self.progress),
            "error": self.error.as_ref().map(|(zh, en)| json!({"zh": zh, "en": en})),
            "notes": self.remote.as_ref().map(|item| json!({
                "version": item.notes.version,
                "zh": item.notes.zh,
                "en": item.notes.en,
            })),
            "autoCheck": auto_check_enabled(),
        })
    }
}

/// Messages the updater worker understands.
pub enum Op {
    /// Bridge ops from the `appUpdate` page handler.
    Ipc { id: i64, op: String, values: Value },
    /// The launch-delayed silent check (10 minutes after Ready).
    AutoCheck,
}

#[derive(Clone)]
pub struct UpdaterHandle {
    sender: Sender<Op>,
}

pub type SharedCore = Arc<Mutex<UpdaterCore>>;

impl UpdaterHandle {
    pub fn send(&self, op: Op) {
        let _ = self.sender.send(op);
    }
}

/// Frozen status snapshot for the UI push path (UserEvent::UpdateChanged).
pub fn status_snapshot(core: &SharedCore) -> Value {
    core.lock().unwrap().snapshot()
}

/// Spawns the updater worker.
pub fn spawn(data_dir: &Path, log: Arc<WorkerLog>, proxy: EventLoopProxy<UserEvent>) -> (UpdaterHandle, SharedCore) {
    let core: SharedCore = Arc::new(Mutex::new(UpdaterCore::open(data_dir)));
    let (sender, receiver) = channel::<Op>();
    let handle = UpdaterHandle { sender };
    let worker_core = core.clone();
    std::thread::Builder::new()
        .name("updater-worker".into())
        .spawn(move || worker_loop(worker_core, receiver, log, proxy))
        .expect("updater worker thread");
    (handle, core)
}

fn worker_loop(core: Arc<Mutex<UpdaterCore>>, receiver: Receiver<Op>, log: Arc<WorkerLog>, proxy: EventLoopProxy<UserEvent>) {
    while let Ok(op) = receiver.recv() {
        match op {
            Op::Ipc { id, op, values } => handle_ipc(&core, id, &op, &values, &log, &proxy),
            Op::AutoCheck => {
                // Silent check: only meaningful from a clean idle state, and a
                // failure must not surface as an updater error — log only.
                if !auto_check_enabled() || core.lock().unwrap().phase != Phase::Idle {
                    continue;
                }
                let outcome = {
                    let mut guard = core.lock().unwrap();
                    perform_check(&mut guard, None)
                };
                match outcome {
                    Ok(_) => log.append(b"[updater] automatic check finished\n"),
                    Err(message) => log.append(format!("[updater] automatic check failed: {message}\n").as_bytes()),
                }
                let _ = proxy.send_event(UserEvent::UpdateChanged);
            }
        }
    }
}

fn handle_ipc(core: &Arc<Mutex<UpdaterCore>>, id: i64, op: &str, values: &Value, log: &Arc<WorkerLog>, proxy: &EventLoopProxy<UserEvent>) {
    match op {
        "status" => {
            let payload = core.lock().unwrap().snapshot().to_string();
            reply(proxy, id, true, Some(payload), None);
        }
        "check" => {
            let feed_override = values
                .get("feed")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            {
                let mut guard = core.lock().unwrap();
                guard.feed_override = feed_override.clone();
                guard.reset(Phase::Checking);
            }
            let _ = proxy.send_event(UserEvent::UpdateChanged);
            let outcome = {
                let mut guard = core.lock().unwrap();
                perform_check(&mut guard, feed_override.as_deref())
            };
            if let Err(message) = outcome {
                let (zh, en) = if message.starts_with("feed requires a newer OS") {
                    ("此更新需要更新版本的 Windows。", "This update requires a newer version of Windows.")
                } else {
                    ("暂时无法检查更新，请稍后重试。", "Could not check for updates right now. Try again shortly.")
                };
                core.lock().unwrap().fail(zh, en);
                log.append(format!("[updater] check failed: {message}\n").as_bytes());
            }
            let payload = core.lock().unwrap().snapshot().to_string();
            reply(proxy, id, true, Some(payload), None);
            let _ = proxy.send_event(UserEvent::UpdateChanged);
        }
        "install" => {
            let phase = core.lock().unwrap().phase;
            match phase {
                Phase::Available => {
                    // Long download: run it on its own thread so status ops and
                    // the worker loop keep answering while it streams.
                    let payload = core.lock().unwrap().snapshot().to_string();
                    reply(proxy, id, true, Some(payload), None);
                    let core = core.clone();
                    let proxy = proxy.clone();
                    std::thread::spawn(move || {
                        let mut guard = core.lock().unwrap();
                        guard.phase = Phase::Downloading;
                        guard.progress = 0;
                        drop(guard);
                        let _ = proxy.send_event(UserEvent::UpdateChanged);
                        let result = download_and_verify(&core, &proxy);
                        match result {
                            Ok(()) => {
                                let path = core.lock().unwrap().setup_path.clone();
                                if let Some(setup) = path {
                                    let _ = proxy.send_event(UserEvent::UpdateInstallReady {
                                        setup: setup.to_string_lossy().into_owned(),
                                    });
                                    return;
                                }
                                core.lock().unwrap().fail(
                                    "安装包校验通过，但启动安装失败。",
                                    "The installer was verified, but launching it failed.",
                                );
                            }
                            Err((zh, en)) => core.lock().unwrap().fail(&zh, &en),
                        }
                        let _ = proxy.send_event(UserEvent::UpdateChanged);
                    });
                }
                Phase::Verified => {
                    let path = core.lock().unwrap().setup_path.clone();
                    let Some(path) = path else {
                        reply(proxy, id, false, None, Some("更新状态异常，请重新检查更新。".into()));
                        return;
                    };
                    core.lock().unwrap().phase = Phase::Installing;
                    let payload = core.lock().unwrap().snapshot().to_string();
                    reply(proxy, id, true, Some(payload), None);
                    let _ = proxy.send_event(UserEvent::UpdateInstallReady {
                        setup: path.to_string_lossy().into_owned(),
                    });
                }
                _ => {
                    reply(proxy, id, false, None, Some("请先检查更新，确认有新版本后再安装。".into()));
                }
            }
            let _ = proxy.send_event(UserEvent::UpdateChanged);
        }
        _ => {
            reply(proxy, id, false, None, Some("未知的更新操作。".into()));
        }
    }
}

fn reply(proxy: &EventLoopProxy<UserEvent>, id: i64, ok: bool, payload: Option<String>, error: Option<String>) {
    let _ = proxy.send_event(UserEvent::BridgeReply { id, ok, payload, error });
}

// ---------------------------------------------------------------------------
// Check (feed fetch + version comparison)

/// Runs one feed check against `core`. Returns the item when a newer version
/// exists, None when the remote is not ahead, Err when the feed cannot be
/// fetched or parsed. Only mutates the core on decided success paths.
pub fn perform_check(core: &mut UpdaterCore, feed_override: Option<&str>) -> Result<Option<UpdateItem>, String> {
    let Some(feed_url) = resolve_feed_url(feed_override) else {
        return Err("updates are disabled".to_string());
    };
    if !feed::is_supported_feed_url(&feed_url) {
        return Err(format!("unsupported feed url scheme: {feed_url}"));
    }
    let body = fetch_bytes(&feed_url, FEED_TIMEOUT)?;
    let items = feed::parse_appcast(&String::from_utf8_lossy(&body))?;
    let Some(item) = items.into_iter().next() else { return Err("feed has no items".to_string()) };
    if !feed::is_os_supported(os_version(), &item.minimum_system) {
        return Err(format!("feed requires a newer OS: {}", item.minimum_system));
    }
    let local = feed::parse_version(current_version());
    let remote = feed::parse_version(&item.version);
    if remote.is_empty() {
        return Err(format!("feed version unparseable: {}", item.version));
    }
    if feed::compare_versions(&remote, &local) != std::cmp::Ordering::Greater {
        core.reset(Phase::Idle);
        core.up_to_date = true;
        return Ok(None);
    }
    core.reset(Phase::Available);
    core.up_to_date = false;
    core.remote = Some(item.clone());
    Ok(Some(item))
}

/// Fetches a small resource (the feed) fully. file:/// URLs read straight from
/// disk for local testing; http(s) goes through ureq (which follows
/// redirects).
pub fn fetch_bytes(url: &str, timeout: Duration) -> Result<Vec<u8>, String> {
    if let Some(path) = file_url_path(url) {
        return std::fs::read(path).map_err(|error| format!("cannot read feed file: {error}"));
    }
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let response = agent.get(url).call().map_err(|error| error.to_string())?;
    let mut reader = response.into_reader();
    let mut body = Vec::new();
    reader.read_to_end(&mut body).map_err(|error| error.to_string())?;
    Ok(body)
}

/// Converts file:///D:/… (or file://localhost/D:/…) into a filesystem path.
pub fn file_url_path(url: &str) -> Option<PathBuf> {
    if !url.to_ascii_lowercase().starts_with("file://") {
        return None;
    }
    let rest = &url["file://".len()..];
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let rest = rest.strip_prefix('/')?;
    let path = rest.split(['?', '#']).next().unwrap_or(rest);
    Some(PathBuf::from(percent_decode(path).replace('/', "\\")))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 3 <= bytes.len() {
            match std::str::from_utf8(&bytes[index + 1..index + 3])
                .ok()
                .and_then(|value| u8::from_str_radix(value, 16).ok())
            {
                Some(byte) => {
                    out.push(byte);
                    index += 3;
                }
                None => {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Current Windows version as (major, minor, build) via RtlGetVersion, which
/// reports the truth regardless of the exe manifest. Returns (0,0,0) when the
/// API cannot be resolved (nothing depends on the zero value; minimum-system
/// checks then simply require the feed minimum to be 0).
pub fn os_version() -> (u64, u64, u64) {
    #[repr(C)]
    struct OsVersionInfoW {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        service_pack: [u16; 128],
    }
    type RtlGetVersion = unsafe extern "system" fn(*mut OsVersionInfoW) -> i32;
    unsafe {
        let ntdll = windows_sys::Win32::System::LibraryLoader::GetModuleHandleA(b"ntdll.dll\0".as_ptr());
        if ntdll.is_null() {
            return (0, 0, 0);
        }
        let address = windows_sys::Win32::System::LibraryLoader::GetProcAddress(ntdll, b"RtlGetVersion\0".as_ptr());
        let Some(address) = address else { return (0, 0, 0) };
        let routine: RtlGetVersion = std::mem::transmute(address);
        let mut info: OsVersionInfoW = std::mem::zeroed();
        info.size = std::mem::size_of::<OsVersionInfoW>() as u32;
        if routine(&mut info) != 0 {
            return (0, 0, 0);
        }
        (info.major as u64, info.minor as u64, info.build as u64)
    }
}

// ---------------------------------------------------------------------------
// Download + verify

fn download_target_dir(data_dir: &Path) -> PathBuf {
    let root = data_dir.join("updater");
    let _ = std::fs::create_dir_all(&root);
    root
}

fn download_and_verify(core: &Arc<Mutex<UpdaterCore>>, proxy: &EventLoopProxy<UserEvent>) -> Result<(), (String, String)> {
    let (item, data_dir) = {
        let guard = core.lock().unwrap();
        match guard.remote.clone() {
            Some(item) => (item, guard.data_dir.clone()),
            None => return Err(("没有待下载的更新。".into(), "No update is pending download.".into())),
        }
    };
    run_download(&item, &data_dir, &mut |percent| {
        let mut changed = false;
        {
            let mut guard = core.lock().unwrap();
            if guard.phase == Phase::Downloading && guard.progress != percent {
                guard.progress = percent;
                changed = true;
            }
        }
        if changed {
            let _ = proxy.send_event(UserEvent::UpdateChanged);
        }
    })?;
    let path = download_target_dir(&data_dir).join(format!("setup-{}.exe", item.version));
    let mut guard = core.lock().unwrap();
    guard.setup_path = Some(path);
    guard.phase = Phase::Verified;
    Ok(())
}

/// Streams the installer to `<data_dir>/updater/setup-<version>.exe.part`,
/// reports progress percent through `on_progress`, then verifies SHA-256 and
/// the Ed25519 signature. On success the `.part` file is renamed to its final
/// name; failures return a bilingual reason and never leave partial files.
pub fn run_download(item: &UpdateItem, data_dir: &Path, on_progress: &mut dyn FnMut(u8)) -> Result<(), (String, String)> {
    if !item.url.to_ascii_lowercase().starts_with("http://") && !item.url.to_ascii_lowercase().starts_with("https://") {
        return Err(("下载地址不受支持。".into(), "The download URL scheme is not supported.".into()));
    }
    if item.sha256.len() != 64 || !item.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err((
            "更新源缺少有效的 SHA-256 摘要。".into(),
            "The feed does not carry a valid SHA-256 digest.".into(),
        ));
    }
    let directory = download_target_dir(data_dir);
    let target = directory.join(format!("setup-{}.exe", item.version));
    let partial = directory.join(format!("setup-{}.exe.part", item.version));
    let _ = std::fs::remove_file(&partial);

    let agent = ureq::AgentBuilder::new().timeout_connect(DOWNLOAD_CONNECT_TIMEOUT).build();
    let response = agent.get(&item.url).call().map_err(|error| {
        (
            format!("下载安装包失败：{error}"),
            format!("Downloading the installer failed: {error}"),
        )
    })?;
    let declared_length = item
        .length
        .max(response.header("Content-Length").and_then(|value| value.parse::<u64>().ok()).unwrap_or(0));
    let ceiling = if declared_length > 0 { declared_length.saturating_add(DOWNLOAD_SLACK_BYTES) } else { u64::MAX };
    let mut file = std::fs::File::create(&partial)
        .map_err(|error| (format!("无法写入下载缓存：{error}"), format!("Cannot write the download cache: {error}")))?;
    let mut reader = response.into_reader();
    let mut buffer = [0u8; 64 * 1024];
    let mut received: u64 = 0;
    let mut last_percent: i32 = -1;
    loop {
        let read = reader.read(&mut buffer).map_err(|error| {
            let _ = std::fs::remove_file(&partial);
            (format!("下载中断：{error}"), format!("The download was interrupted: {error}"))
        })?;
        if read == 0 {
            break;
        }
        received += read as u64;
        if received > ceiling {
            drop(file);
            let _ = std::fs::remove_file(&partial);
            return Err((
                "下载内容超出更新源声明的体积，已中止。".into(),
                "The download exceeds the size declared in the feed and was aborted.".into(),
            ));
        }
        if std::io::Write::write_all(&mut file, &buffer[..read]).is_err() {
            let _ = std::fs::remove_file(&partial);
            return Err((
                "写入下载缓存失败。".into(),
                "Writing the download cache failed.".into(),
            ));
        }
        if declared_length > 0 {
            let percent = ((received as f64 / declared_length as f64) * 100.0).floor().clamp(0.0, 99.0) as i32;
            if percent > last_percent {
                last_percent = percent;
                on_progress(percent as u8);
            }
        }
    }
    drop(file);
    if declared_length > 0 && received != declared_length {
        let _ = std::fs::remove_file(&partial);
        return Err((
            format!("下载不完整（{received} / {declared_length} 字节），已中止。"),
            format!("The download is incomplete ({received} / {declared_length} bytes) and was aborted."),
        ));
    }
    let bytes = std::fs::read(&partial)
        .map_err(|error| (format!("无法读取下载缓存：{error}"), format!("Cannot read the download cache: {error}")))?;
    let digest = crypto::sha256_hex(&bytes);
    if !digest.eq_ignore_ascii_case(&item.sha256) {
        let _ = std::fs::remove_file(&partial);
        return Err((
            "安装包与更新源记录的 SHA-256 不一致，已拒绝安装。".into(),
            "The installer does not match the SHA-256 recorded in the feed. Install refused.".into(),
        ));
    }
    if let Err(reason) = crypto::verify_signature(&crypto::public_key_for_verification(), &item.signature, &bytes) {
        let _ = std::fs::remove_file(&partial);
        return Err((
            format!("安装包签名校验失败，已拒绝安装。{reason}"),
            format!("The installer signature check failed. Install refused. {reason}"),
        ));
    }
    std::fs::rename(&partial, &target).map_err(|error| {
        let _ = std::fs::remove_file(&partial);
        (
            format!("无法保存安装包：{error}"),
            format!("Cannot store the installer: {error}"),
        )
    })?;
    on_progress(100);
    Ok(())
}

/// The exact Inno Setup invocation used for the silent update install.
/// Exposed for tests and the QA dry run.
pub fn installer_command_line(setup: &Path) -> String {
    format!(
        "\"{}\" /VERYSILENT /SUPPRESSMSGBOXES /RESTARTAPPLICATIONS /CLOSEAPPLICATIONS",
        setup.display()
    )
}

/// Launches the verified installer outside the shell's kill-on-close job
/// object (the shell exits right after; the installer must outlive it). The
/// shell job object sets BREAKAWAY_OK to permit this.
pub fn launch_installer(setup: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    std::process::Command::new(setup)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/RESTARTAPPLICATIONS", "/CLOSEAPPLICATIONS"])
        .creation_flags(CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// QA (AIR_STUDIO_UPDATE_QA=1): exercises check → download → verify against a
// local fixture server (which 302-redirects the installer), plus the tampered
// signature / tampered installer / version-replay negative cases, dry-runs
// the installer command line and writes a report next to the logs.

pub fn run_update_qa(data_dir: &Path) -> i32 {
    let mut lines = Vec::new();
    let mut code = 0;
    let mut check = |ok: bool, label: &str, detail: String| {
        lines.push(format!("[{}] {} {}", if ok { "PASS" } else { "FAIL" }, label, detail));
        if !ok {
            code = 1;
        }
    };

    let public_key = crypto::public_key_for_verification();
    let key_length = crypto::decode_base64(&public_key).map(|bytes| bytes.len()).unwrap_or(0);
    check(key_length == 32, "public-key", format!("length={key_length}"));

    let good_feed = std::env::var("AIR_STUDIO_UPDATE_FEED").unwrap_or_default();
    check(!good_feed.is_empty(), "feed-env", good_feed.clone());

    // 1. Positive path: check → available.
    let mut core = UpdaterCore::open(data_dir);
    let checked = perform_check(&mut core, Some(&good_feed));
    if let Ok(Some(item)) = &checked {
        check(core.phase == Phase::Available, "check-available", format!("version={} notes-zh={}", item.version, !item.notes.zh.is_empty()));
    } else {
        check(false, "check-available", format!("{:?}", checked.as_ref().err()));
    }

    // 2. Download + verify (the fixture server redirects the installer).
    if let Ok(Some(item)) = &checked {
        match run_download(item, data_dir, &mut |_| {}) {
            Ok(()) => {
                let expected = download_target_dir(data_dir).join(format!("setup-{}.exe", item.version));
                check(expected.is_file(), "download-verify", expected.display().to_string());
                // 3. Installer command line dry run (never really installed).
                let command = installer_command_line(&expected);
                let flags_ok = ["/VERYSILENT", "/SUPPRESSMSGBOXES", "/RESTARTAPPLICATIONS", "/CLOSEAPPLICATIONS"]
                    .iter()
                    .all(|flag| command.contains(flag))
                    && command.contains(&format!("\"{}\"", expected.display()));
                check(flags_ok, "install-command", command);
            }
            Err((zh, en)) => check(false, "download-verify", format!("{zh} / {en}")),
        }
    }

    // 4. Tampered signature → refused (SHA passes, signature does not).
    if let Ok(url) = std::env::var("AIR_STUDIO_UPDATE_QA_TAMPERED_SIG") {
        let mut core = UpdaterCore::open(data_dir);
        match perform_check(&mut core, Some(&url)).ok().flatten() {
            Some(item) => match run_download(&item, data_dir, &mut |_| {}) {
                Err((zh, _)) => check(zh.contains("签名"), "tampered-signature", zh),
                Ok(()) => check(false, "tampered-signature", "tampered signature was accepted".into()),
            },
            None => check(false, "tampered-signature", "check unexpectedly failed".into()),
        }
    }

    // 5. Tampered installer bytes → SHA-256 refused.
    if let Ok(url) = std::env::var("AIR_STUDIO_UPDATE_QA_TAMPERED_SETUP") {
        let mut core = UpdaterCore::open(data_dir);
        match perform_check(&mut core, Some(&url)).ok().flatten() {
            Some(item) => match run_download(&item, data_dir, &mut |_| {}) {
                Err((zh, _)) => check(zh.contains("SHA-256"), "tampered-setup", zh),
                Ok(()) => check(false, "tampered-setup", "tampered installer was accepted".into()),
            },
            None => check(false, "tampered-setup", "check unexpectedly failed".into()),
        }
    }

    // 6. Older version → treated as up to date (replay refused).
    if let Ok(url) = std::env::var("AIR_STUDIO_UPDATE_QA_OLDER") {
        let mut core = UpdaterCore::open(data_dir);
        match perform_check(&mut core, Some(&url)) {
            Ok(None) => check(core.up_to_date && core.phase == Phase::Idle, "older-version", "ignored as up-to-date".into()),
            Ok(Some(item)) => check(false, "older-version", format!("older {} was offered", item.version)),
            Err(message) => check(false, "older-version", message),
        }
    }

    let report = data_dir.join("updater-qa.txt");
    let _ = std::fs::create_dir_all(data_dir);
    let body = format!("NuPhyLab updater QA\n{}\n", lines.join("\n"));
    let _ = std::fs::write(&report, &body);
    println!("{body}");
    code
}
