// Drag Convert controller (Windows port of DragConvertController.swift):
// owns settings/history via DragStore, dispatches the dragConvert bridge ops
// on a dedicated worker thread, configures the trigger hook + wheel menu and
// runs conversion jobs on background threads. Every op answers with the full
// Status object (or a rejection carrying the issue string), like the Mac
// bridge; progress, cancel and the last 20 batches surface on the page.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tao::event_loop::EventLoopProxy;

use crate::win;
use crate::worker::WorkerLog;
use crate::UserEvent;

use self::engine::JobControl;
use self::store::{Batch, DragPreferences, DragStore, FileResult};
use self::trigger::SharedDragRuntime;

pub mod engine;
pub mod menu;
pub mod store;
pub mod trigger;

pub type SharedCore = Arc<Mutex<DragCore>>;

/// Bilingual issue text ("中文 / English"); the UI splits on " / ".
fn issue(zh: &str, en: &str) -> String {
    format!("{zh} / {en}")
}

pub struct DragCore {
    pub settings: DragPreferences,
    pub load_issue: String,
    pub history: Vec<Batch>,
    pub outputs: BTreeMap<String, Vec<PathBuf>>,
    pub job: Option<Arc<JobControl>>,
    pub current_action: String,
    pub total: usize,
    pub availability: engine::Availability,
}

impl DragCore {
    fn open(store: &DragStore) -> DragCore {
        let mut load_issue = String::new();
        let settings = match store.load_settings() {
            Ok(settings) => match settings.validate() {
                Ok(()) => settings,
                Err(_) => {
                    load_issue = issue(
                        "无法读取转换设置，已暂停。重新保存设置即可恢复。",
                        "Settings could not be read; re-save to recover.",
                    );
                    DragPreferences::default()
                }
            },
            Err(_) => {
                load_issue = issue(
                    "无法读取转换设置，已暂停。重新保存设置即可恢复。",
                    "Settings could not be read; re-save to recover.",
                );
                DragPreferences::default()
            }
        };
        let (history, outputs) = store.load_history();
        DragCore {
            settings,
            load_issue,
            history,
            outputs,
            job: None,
            current_action: String::new(),
            total: 0,
            availability: engine::detect_availability(),
        }
    }

    fn current_issue(&self) -> String {
        if !self.load_issue.is_empty() {
            return self.load_issue.clone();
        }
        if !self.settings.enabled {
            return String::new();
        }
        let mut missing = Vec::new();
        if !self.availability.pdfium {
            missing.push(issue("PDF 转图片 / 拆图需要 pdfium.dll", "PDF to image / page rendering needs pdfium.dll"));
        }
        if !self.availability.ffmpeg {
            missing.push(issue("音视频转换需要系统 PATH 中的 ffmpeg", "Audio and video conversion needs ffmpeg on the system PATH"));
        }
        missing.join("；")
    }
}

/// Messages the dragconvert worker understands.
pub enum Op {
    Ipc { id: i64, op: String, values: Value },
    /// Wheel committed a drop over a sector.
    Drop { action: String, files: Vec<PathBuf> },
    /// A conversion job finished (sent by the job thread).
    JobDone { batch: Batch, outputs: Vec<PathBuf> },
}

#[derive(Clone)]
pub struct DragHandle {
    sender: Sender<Op>,
}

impl DragHandle {
    pub fn send(&self, op: Op) {
        let _ = self.sender.send(op);
    }
}

/// Starts the dragconvert worker and the wheel-menu thread. Returns the op
/// handle plus the shared core and hook runtime (DragHook consumes both).
pub fn spawn(data_dir: &Path, log: Arc<WorkerLog>, proxy: EventLoopProxy<UserEvent>, english: bool) -> (DragHandle, SharedCore, SharedDragRuntime) {
    let store = DragStore::open(data_dir);
    let core = Arc::new(Mutex::new(DragCore::open(&store)));
    let runtime = Arc::new(Mutex::new(trigger::DragHookRuntime::new()));
    let english_flag: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(english)));
    let _ = ENGLISH_FLAG.set(english_flag);
    set_proxy(proxy);
    let (sender, receiver) = channel::<Op>();
    let availability = core.lock().unwrap().availability;
    // Wheel window thread: registers the OLE drop target for the hook to wake.
    menu::spawn_menu(
        english_flag,
        availability,
        runtime.clone(),
        sender.clone(),
        runtime.lock().unwrap().menu_thread_id.clone(),
    );
    let worker_core = core.clone();
    let worker_store = DragStore::open(data_dir);
    let worker_runtime = runtime.clone();
    let worker_sender = sender.clone();
    std::thread::Builder::new()
        .name("dragconvert-worker".into())
        .spawn(move || worker_loop(worker_core, worker_store, worker_runtime, receiver, worker_sender, log))
        .expect("dragconvert worker thread");
    (DragHandle { sender }, core, runtime)
}

/// Mirrors the current UI language into the wheel menu labels.
static ENGLISH_FLAG: std::sync::OnceLock<&'static AtomicBool> = std::sync::OnceLock::new();

pub fn set_english(english: bool) {
    if let Some(flag) = ENGLISH_FLAG.get() {
        flag.store(english, std::sync::atomic::Ordering::Relaxed);
    }
}

fn worker_loop(
    core: SharedCore,
    store: DragStore,
    runtime: SharedDragRuntime,
    receiver: Receiver<Op>,
    sender: Sender<Op>,
    log: Arc<WorkerLog>,
) {
    while let Ok(op) = receiver.recv() {
        match op {
            Op::Ipc { id, op, values } => {
                let mut guard = core.lock().unwrap();
                handle_ipc(&mut guard, &store, &runtime, id, &op, &values);
            }
            Op::Drop { action, files } => {
                let mut guard = core.lock().unwrap();
                start_job(&mut guard, sender.clone(), &log, action, files);
            }
            Op::JobDone { batch, outputs } => {
                let mut guard = core.lock().unwrap();
                finish_job(&mut guard, &store, batch, outputs);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge op dispatch (DragConvertController.handle parity)

fn handle_ipc(core: &mut DragCore, store: &DragStore, runtime: &SharedDragRuntime, id: i64, op: &str, values: &Value) {
    match op {
        "status" | "refresh" => reply(core, id, true, None),
        "inputPermission" => {
            // Windows needs no input-monitoring grant for the trigger key.
            reply(core, id, true, None)
        }
        "cancel" => {
            if let Some(job) = &core.job {
                job.cancel();
            }
            reply(core, id, true, None)
        }
        "settings" => apply_settings(core, store, runtime, values, id),
        "folder" => choose_output_folder(core, store, id),
        "sourceFolder" => {
            let mut next = core.settings.clone();
            next.output_directory = None;
            match next.validate().and_then(|_| store.save_settings(&next)) {
                Ok(()) => {
                    core.settings = next;
                    core.load_issue.clear();
                    reply(core, id, true, None)
                }
                Err(message) => reply(core, id, false, Some(message)),
            }
        }
        "reveal" => {
            let Some(batch_id) = values.get("id").and_then(Value::as_str) else {
                reply(core, id, false, Some(issue("没有可以查看的文件", "No converted files")));
                return;
            };
            reveal_batch(core, id, batch_id);
        }
        _ => reply(core, id, false, Some(issue("未知的拖拽转换操作", "Unknown drag conversion operation"))),
    }
}

fn apply_settings(core: &mut DragCore, store: &DragStore, runtime: &SharedDragRuntime, values: &Value, id: i64) {
    let patch = values.get("preferences").cloned().unwrap_or_else(|| json!({}));
    let mut next = core.settings.clone();
    next.apply_patch(&patch);
    // Output locations can only come from the native folder picker.
    next.output_directory = core.settings.output_directory.clone();
    if let Err(message) = next.validate() {
        reply(core, id, false, Some(message));
        return;
    }
    let previous = core.settings.clone();
    core.settings = next;
    if let Err(message) = store.save_settings(&core.settings) {
        core.settings = previous;
        reply(core, id, false, Some(message));
        return;
    }
    core.load_issue.clear();
    // The hook and wheel pick up the new trigger immediately.
    if let Ok(mut hook) = runtime.lock() {
        hook.vk = trigger::trigger_vk(&core.settings.trigger);
        hook.enabled = core.settings.enabled;
    }
    reply(core, id, true, None);
}

fn choose_output_folder(core: &mut DragCore, store: &DragStore, id: i64) {
    let Some(directory) = browse_for_folder() else {
        reply(core, id, true, None); // cancelled picker: report current status
        return;
    };
    let mut next = core.settings.clone();
    next.output_directory = Some(directory.to_string_lossy().into_owned());
    match next.validate().and_then(|_| store.save_settings(&next)) {
        Ok(()) => {
            core.settings = next;
            core.load_issue.clear();
            reply(core, id, true, None)
        }
        Err(message) => reply(core, id, false, Some(message)),
    }
}

fn reveal_batch(core: &mut DragCore, id: i64, batch_id: &str) {
    let paths = core.outputs.get(batch_id).cloned().unwrap_or_default();
    let existing: Vec<&PathBuf> = paths.iter().filter(|path| path.exists()).collect();
    if existing.is_empty() {
        reply(core, id, false, Some(issue("文件已移动或删除。", "Files have been moved or deleted.")));
        return;
    }
    win::show_in_explorer(&existing[0].to_string_lossy());
    reply(core, id, true, Some("true".into()));
}

// ---------------------------------------------------------------------------
// Conversion jobs

fn start_job(core: &mut DragCore, sender: Sender<Op>, log: &Arc<WorkerLog>, action: String, files: Vec<PathBuf>) {
    if !core.settings.enabled || core.job.is_some() || files.is_empty() || files.len() > engine::MAXIMUM_INPUTS {
        return;
    }
    let kinds: Vec<engine::FileKind> = files.iter().map(|path| engine::file_kind(path)).collect();
    if !engine::actions_for(&kinds, files.len()).contains(&action.as_str()) {
        log.append(format!("[dragconvert] rejected drop: {action} not valid for selection\n").as_bytes());
        return;
    }
    let control = Arc::new(JobControl::new());
    let preferences = core.settings.clone();
    core.job = Some(control.clone());
    core.current_action = action.clone();
    core.total = files.len();
    let input_count = files.len();
    let groups: Vec<Vec<PathBuf>> = if engine::combines_inputs(&action) {
        vec![files]
    } else {
        files.into_iter().map(|file| vec![file]).collect()
    };
    std::thread::Builder::new()
        .name("dragconvert-job".into())
        .spawn(move || {
            let mut batch = Batch::new(&action, input_count);
            let mut outputs = Vec::new();
            for group in &groups {
                if control.is_cancelled() {
                    break;
                }
                control.set_progress(0.0);
                control.set_current_file(if group.len() > 1 {
                    format!("{group_count} files", group_count = group.len())
                } else {
                    group[0]
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default()
                });
                match engine::convert(group, &action, &preferences, &control) {
                    Ok(output) => {
                        batch.succeeded += 1;
                        batch.files.push(FileResult {
                            input: display_name(&group[0]),
                            output: display_name(&output),
                            error: String::new(),
                        });
                        outputs.push(output);
                    }
                    Err(message) => {
                        batch.failed += 1;
                        batch.files.push(FileResult {
                            input: display_name(&group[0]),
                            output: String::new(),
                            error: message,
                        });
                    }
                }
                control.step_done();
                control.set_progress(1.0);
            }
            if control.is_cancelled() {
                batch.cancelled = Some(true);
            }
            let _ = sender.send(Op::JobDone { batch, outputs });
        })
        .ok();
}

fn finish_job(core: &mut DragCore, store: &DragStore, batch: Batch, outputs: Vec<PathBuf>) {
    if !outputs.is_empty() {
        core.outputs.insert(batch.id.clone(), outputs);
    }
    Batch::push_history(&mut core.history, batch);
    // Drop the output map entries of batches that fell out of the history.
    core.outputs.retain(|id, _| core.history.iter().any(|batch| batch.id == *id));
    core.job = None;
    core.current_action.clear();
    core.total = 0;
    if let Err(message) = store.save_history(&core.history, &core.outputs) {
        core.load_issue = message;
    }
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// Snapshot (Status parity for DragConvertLab.tsx)

pub fn snapshot(core: &DragCore) -> Value {
    let preferences = core.settings.preferences_value();
    let history: Vec<Value> = core
        .history
        .iter()
        .map(|batch| serde_json::to_value(batch).unwrap_or(Value::Null))
        .collect();
    let (busy, completed, total, progress, current_file, cancelling) = match &core.job {
        Some(job) => (true, job.completed(), core.total, job.progress(), job.current_file(), job.is_cancelled()),
        None => (false, 0, 0, 0.0, String::new(), false),
    };
    json!({
        "preferences": preferences,
        "ready": core.settings.enabled,
        "busy": busy,
        "completed": completed,
        "total": total,
        "history": history,
        "issue": core.current_issue(),
        "progress": progress,
        "currentAction": core.current_action,
        "currentFile": current_file,
        "cancelling": cancelling,
        "triggerLabels": trigger::trigger_labels(),
        "inputPermissionNeeded": false,
    })
}

fn reply(core: &DragCore, id: i64, ok: bool, override_error: Option<String>) {
    let payload = if ok { Some(snapshot(core).to_string()) } else { None };
    send_reply(id, ok, payload, override_error);
}

// Reply plumbing mirrors the pocket bridge: the worker thread sends
// UserEvent::BridgeReply through the event-loop proxy captured at spawn.
static PROXY: std::sync::OnceLock<Option<EventLoopProxy<UserEvent>>> = std::sync::OnceLock::new();

fn send_reply(id: i64, ok: bool, payload: Option<String>, error: Option<String>) {
    if let Some(Some(proxy)) = PROXY.get() {
        let _ = proxy.send_event(UserEvent::BridgeReply { id, ok, payload, error });
    }
}

pub fn set_proxy(proxy: EventLoopProxy<UserEvent>) {
    let _ = PROXY.set(Some(proxy));
}

// ---------------------------------------------------------------------------
// Folder picker (folder op)

fn browse_for_folder() -> Option<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{SHBrowseForFolderW, SHGetPathFromIDListW, BROWSEINFOW};
    let title: Vec<u16> = "选择转换副本的保存位置\0".encode_utf16().collect();
    let mut info: BROWSEINFOW = unsafe { std::mem::zeroed() };
    info.lpszTitle = title.as_ptr();
    info.ulFlags = 0x0040 | 0x0010; // BIF_NEWDIALOGSTYLE | BIF_RETURNONLYFSDIRS
    let pidl = unsafe { SHBrowseForFolderW(&mut info) };
    if pidl.is_null() {
        return None;
    }
    let mut buffer = [0u16; 260];
    let ok = unsafe { SHGetPathFromIDListW(pidl, buffer.as_mut_ptr()) };
    unsafe { CoTaskMemFree(pidl as *const core::ffi::c_void) };
    if ok == 0 {
        return None;
    }
    let end = buffer.iter().position(|unit| *unit == 0).unwrap_or(buffer.len());
    let path = String::from_utf16_lossy(&buffer[..end]);
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

// ---------------------------------------------------------------------------
// Storage QA entry (AIR_STUDIO_DRAG_QA=1): real image/zip/tar/mergePDF round
// trips in an isolated directory, report written next to the logs.

pub fn run_conversion_qa(data_dir: &Path) -> i32 {
    use self::engine as eng;
    use std::io::Read;
    let root = data_dir.join("drag-convert-qa");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).ok();
    let mut lines = Vec::new();
    let mut code = 0;
    let mut report = |name: &str, result: &Result<PathBuf, String>, verify: &dyn Fn(&Path) -> Result<(), String>| {
        match result {
            Ok(output) => match verify(output) {
                Ok(()) => lines.push(format!("{name}: ok ({})", output.display())),
                Err(message) => {
                    lines.push(format!("{name}: FAILED {message}"));
                    code = 1;
                }
            },
            Err(message) => {
                lines.push(format!("{name}: FAILED {message}"));
                code = 1;
            }
        }
    };
    let text = |output: &Path, name: &str| -> Result<String, String> { std::fs::read_to_string(output.join(name)).map_err(|e| e.to_string()) };

    // Sample images: an opaque PNG and a transparent PNG (white flatten).
    let opaque = root.join("opaque.png");
    let mut opaque_image = image::RgbaImage::new(24, 18);
    for pixel in opaque_image.pixels_mut() {
        *pixel = image::Rgba([200, 40, 40, 255]);
    }
    opaque_image.save(&opaque).expect("qa png");
    let transparent = root.join("transparent.png");
    let mut alpha_image = image::RgbaImage::new(20, 20);
    for pixel in alpha_image.pixels_mut() {
        *pixel = image::Rgba([10, 200, 90, 128]);
    }
    alpha_image.save(&transparent).expect("qa alpha png");

    let preferences = DragPreferences::default();
    let control = JobControl::new();
    let jpg = eng::convert(&[opaque.clone()], "jpg", &preferences, &control);
    report("png->jpg", &jpg, &|output: &Path| {
        image::open(output).map_err(|error| error.to_string())?;
        Ok(())
    });
    let small_preferences = DragPreferences { max_dimension: 8, ..DragPreferences::default() };
    let small = eng::convert(&[opaque.clone()], "small", &small_preferences, &control);
    report("png->small", &small, &|output: &Path| {
        let image = image::open(output).map_err(|error| error.to_string())?;
        if image.width() != 8 || image.height() > 6 {
            return Err(format!("expected 8px long edge, got {}x{}", image.width(), image.height()));
        }
        Ok(())
    });
    let webp = eng::convert(&[transparent.clone()], "webp", &preferences, &control);
    report("png->webp", &webp, &|output: &Path| {
        image::open(output).map_err(|error| error.to_string())?;
        Ok(())
    });

    // ZIP round trip.
    let payload = root.join("资料.txt");
    std::fs::write(&payload, "拖拽转换 / drag convert").unwrap();
    let zipped = eng::convert(&[payload.clone()], "zip", &preferences, &control);
    report("zip", &zipped, &|output: &Path| {
        let file = std::fs::File::open(output).map_err(|e| e.to_string())?;
        let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
        let mut entry = archive.by_name("资料.txt").map_err(|e| e.to_string())?;
        let mut text = String::new();
        entry.read_to_string(&mut text).map_err(|e| e.to_string())?;
        if text != "拖拽转换 / drag convert" {
            return Err("zip content mismatch".into());
        }
        Ok(())
    });
    let extracted = zipped.clone().and_then(|archive| eng::convert(&[archive], "extract", &preferences, &control));
    report("extract", &extracted, &|output: &Path| {
        if text(output, "资料.txt")? != "拖拽转换 / drag convert" {
            return Err("extract content mismatch".into());
        }
        Ok(())
    });

    // TAR.GZ round trip.
    let tarred = eng::convert(&[payload.clone(), opaque.clone()], "tarGz", &preferences, &control);
    report("tarGz", &tarred, &|output: &Path| {
        let file = std::fs::File::open(output).map_err(|e| e.to_string())?;
        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        let names: Vec<String> = archive
            .entries()
            .map_err(|e| e.to_string())?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path().map(|path| path.to_string_lossy().into_owned()).unwrap_or_default())
            .collect();
        if !names.contains(&"资料.txt".to_string()) {
            return Err(format!("tar missing entry: {names:?}"));
        }
        Ok(())
    });

    // mergePDF round trip: two generated PDFs merge into a two-page PDF.
    let pdf_a = root.join("a.pdf");
    let pdf_b = root.join("b.pdf");
    write_sample_pdf(&pdf_a);
    write_sample_pdf(&pdf_b);
    let merged = eng::convert(&[pdf_a, pdf_b], "mergePDF", &preferences, &control);
    report("mergePDF", &merged, &|output: &Path| {
        let document = lopdf::Document::load(output).map_err(|error| error.to_string())?;
        let pages = document.get_pages();
        if pages.len() != 2 {
            return Err(format!("expected 2 pages, got {}", pages.len()));
        }
        Ok(())
    });

    // Naming rule: a second identical conversion lands at "<stem>-1".
    let second = eng::convert(&[opaque.clone()], "jpg", &preferences, &control);
    match (&jpg, &second) {
        (Ok(first), Ok(other)) => {
            let expected = format!("{}-1.jpg", first.with_extension("").to_string_lossy());
            if other.to_string_lossy() != expected {
                lines.push(format!("unique naming: FAILED expected {expected}, got {}", other.display()));
                code = 1;
            } else {
                lines.push("unique naming: ok".into());
            }
        }
        _ => lines.push("unique naming: FAILED (a jpg conversion failed)".to_string()),
    }

    let availability = eng::detect_availability();
    lines.push(format!("availability: pdfium={} ffmpeg={}", availability.pdfium, availability.ffmpeg));
    lines.push(format!("history cap: {}", store::MAXIMUM_HISTORY));
    let report_text = format!("{}\n", lines.join("\n"));
    let _ = std::fs::create_dir_all(data_dir);
    let _ = std::fs::write(data_dir.join("drag-convert-qa.txt"), &report_text);
    print!("{report_text}");
    code
}

fn write_sample_pdf(path: &Path) {
    use pdf_writer::Finish;
    let mut pdf = pdf_writer::Pdf::new();
    let catalog_id = pdf_writer::Ref::new(1);
    let page_tree_id = pdf_writer::Ref::new(2);
    let page_id = pdf_writer::Ref::new(3);
    let content_id = pdf_writer::Ref::new(4);
    pdf.catalog(catalog_id).pages(page_tree_id);
    pdf.pages(page_tree_id).kids([page_id]).count(1);
    let mut page = pdf.page(page_id);
    page.media_box(pdf_writer::Rect::new(0.0, 0.0, 200.0, 100.0));
    page.parent(page_tree_id);
    page.contents(content_id);
    page.finish();
    let content = pdf_writer::Content::new();
    pdf.stream(content_id, &content.finish());
    std::fs::write(path, pdf.finish()).expect("sample pdf");
}
