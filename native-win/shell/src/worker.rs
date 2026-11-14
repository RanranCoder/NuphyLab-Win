// Node worker orchestration, ported from native/AppHost.swift startWorker()
// (spawn + ready handshake + stderr rotation log + termination handling).
use std::io::{BufRead, Read};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tao::event_loop::EventLoopProxy;

use crate::win::{self, JobObject};
use crate::UserEvent;

const LOG_ROTATE_BYTES: u64 = 1_048_576;
const LOG_CHUNK_LIMIT: usize = 65_536;

/// Rotating stderr log. Mirrors appendWorkerLog in AppHost.swift: rotate at
/// 1 MiB to `worker.log.previous`, truncate oversized chunks to their tail,
/// flush every write so a crash cannot lose the diagnostic tail.
pub struct WorkerLog {
    path: PathBuf,
}

impl WorkerLog {
    pub fn new(path: PathBuf) -> Self {
        WorkerLog { path }
    }

    pub fn append(&self, bytes: &[u8]) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(meta) = std::fs::metadata(&self.path) {
            if meta.len() > LOG_ROTATE_BYTES {
                let previous = self.path.with_extension("log.previous");
                let _ = std::fs::remove_file(&previous);
                let _ = std::fs::rename(&self.path, &previous);
            }
        }
        let write = |bytes: &[u8]| {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
            if bytes.len() > LOG_CHUNK_LIMIT {
                file.write_all(b"[stderr chunk truncated]\n")?;
                file.write_all(&bytes[bytes.len() - LOG_CHUNK_LIMIT..])?;
            } else if !bytes.is_empty() {
                file.write_all(bytes)?;
            }
            file.flush()?;
            let _ = file.sync_data();
            Ok::<(), std::io::Error>(())
        };
        // A logging failure must not mask the worker's own exit status.
        let _ = write(bytes);
    }
}

/// The node server child process. Dropping `stdin` is the graceful shutdown
/// signal: the server watches stdin EOF when AIR_STUDIO_STDIN_EXIT=1 and runs
/// its restore chain (the Windows equivalent of the macOS SIGTERM path).
pub struct Worker {
    child: Child,
    stdin: Option<ChildStdin>,
    _job: JobObject,
}

impl Worker {
    /// true when the graceful signal could be delivered (stdin pipe closed).
    pub fn request_graceful_exit(&mut self) -> bool {
        match self.stdin.take() {
            Some(_) => true,
            None => false,
        }
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

fn iso_now() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let seconds_of_day = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60
    )
}

fn previous_port(data_dir: &std::path::Path) -> Option<u16> {
    let runtime = std::fs::read_to_string(data_dir.join("runtime.json")).ok()?;
    let value: Value = serde_json::from_str(&runtime).ok()?;
    let port = value.get("port")?.as_u64()?;
    (port > 0 && port <= u16::MAX as u64).then_some(port as u16)
}

pub fn spawn(
    root: &std::path::Path,
    data_dir: &std::path::Path,
    log: Arc<WorkerLog>,
    proxy: EventLoopProxy<UserEvent>,
) -> Result<Worker, String> {
    let bundled = root.join("runtime").join("node.exe");
    let program = if bundled.is_file() {
        bundled.into_os_string()
    } else {
        // Development layout: repo root with node on PATH.
        log.append(format!("[{}] Using node.exe from PATH (no runtime/node.exe under {})\n", iso_now(), root.display()).as_bytes());
        std::ffi::OsString::from("node.exe")
    };
    let mut command = Command::new(&program);
    command
        .arg(root.join("server").join("main.mjs"))
        .current_dir(root)
        .env("AIR_STUDIO_HID", root.join("bin").join("HIDBridge.exe"))
        .env("AIR_STUDIO_UI", root.join("ui").join("dist"))
        .env("AIR_STUDIO_DATA", data_dir)
        .env("AIR_STUDIO_STDIN_EXIT", "1")
        // parent-lifetime: a hard host kill (no SIGTERM, no stdin EOF) still
        // releases the USB handle, held shortcuts and the RGB session.
        .env("AIR_STUDIO_PARENT_PID", std::process::id().to_string())
        .env_remove("AIR_STUDIO_PORT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Reuse the previous web origin so an app update preserves the unsaved canvas.
    if let Some(port) = previous_port(data_dir) {
        command.env("AIR_STUDIO_PREFERRED_PORT", port.to_string());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|error| {
        log.append(format!("Worker launch failed: {}\n", error).as_bytes());
        error.to_string()
    })?;
    let pid = child.id();
    log.append(format!("[{}] Worker started: pid={}, root={}\n", iso_now(), pid, root.display()).as_bytes());
    let job = JobObject::kill_on_close().map_err(|error| {
        let _ = child.kill();
        log.append(format!("Job object failed: {}\n", error).as_bytes());
        error.to_string()
    })?;
    if let Err(error) = job.assign(&child) {
        let _ = child.kill();
        log.append(format!("Job assignment failed: {}\n", error).as_bytes());
        return Err(error.to_string());
    }
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    if let Some(stderr) = stderr {
        let log = log.clone();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stderr);
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => log.append(&chunk[..n]),
                }
            }
        });
    }
    if let Some(stdout) = stdout {
        let proxy = proxy.clone();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let trimmed: &[u8] = line.strip_suffix(b"\n").unwrap_or(&line);
                        if trimmed.is_empty() {
                            continue;
                        }
                        let Ok(message) = serde_json::from_slice::<Value>(trimmed) else { continue };
                        if message.get("event").and_then(Value::as_str) == Some("ready") {
                            if let Some(url) = message.get("url").and_then(Value::as_str) {
                                let _ = proxy.send_event(UserEvent::Ready { url: url.to_string() });
                            }
                        }
                    }
                }
            }
        });
    }
    {
        let log = log.clone();
        let proxy = proxy.clone();
        std::thread::spawn(move || {
            let code = win::wait_process_exit(pid);
            log.append(
                format!(
                    "\n[{}] Worker exited: status={}\n",
                    iso_now(),
                    code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".to_string())
                )
                .as_bytes(),
            );
            let _ = proxy.send_event(UserEvent::WorkerExited { code });
        });
    }
    Ok(Worker { child, stdin, _job: job })
}

/// Shared handle so the quit path can signal the worker from the event loop.
pub type WorkerSlot = Arc<Mutex<Option<Worker>>>;
