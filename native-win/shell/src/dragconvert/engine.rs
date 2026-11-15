// Drag Convert conversion engine (Windows port of the DragConvertCore routing
// plus DragImageConverter/DragArchiveConverter/DragDocumentConverter): pure
// action routing shared with the wheel menu, availability detection for the
// degraded actions (pdfium / ffmpeg) and the blocking conversion routines that
// run on the job thread. Originals are always kept; outputs are staged in the
// destination folder and published with an exclusive rename under an
// auto-numbered name.

use std::collections::BTreeMap;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use image::ImageEncoder;
use pdf_writer::Finish;

use crate::pocket::store::{normalize_path, uuid_v4};
use super::store::DragPreferences;

pub const MAXIMUM_INPUTS: usize = 128;
const MAXIMUM_IMAGE_BYTES: u64 = 512 * 1024 * 1024;
const MAXIMUM_IMAGE_PIXELS: u64 = 100_000_000;
const MAXIMUM_DOCUMENT_BYTES: u64 = 512 * 1024 * 1024;
const MAXIMUM_PDF_PAGES: usize = 200;
const MAXIMUM_MEDIA_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAXIMUM_ARCHIVE_INPUT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const MAXIMUM_ARCHIVE_ENTRIES: usize = 20_000;
pub const MAXIMUM_ARCHIVE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAXIMUM_WEBP_EDGE: u32 = 16_383;
const MAXIMUM_NAME_CHARS: usize = 80;

fn issue(zh: &str, en: &str) -> String {
    format!("{zh} / {en}")
}

// ---------------------------------------------------------------------------
// File kind routing (DragFileKind parity) and wheel action lists

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileKind {
    Image,
    Pdf,
    Video,
    Audio,
    Archive,
    Other,
}

pub fn file_kind(path: &Path) -> FileKind {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "heic" | "heif" | "webp" | "tif" | "tiff" | "bmp" => FileKind::Image,
        "pdf" => FileKind::Pdf,
        "mp4" | "mov" | "m4v" => FileKind::Video,
        "mp3" | "m4a" | "wav" | "aif" | "aiff" | "caf" | "aac" | "flac" => FileKind::Audio,
        "zip" | "7z" | "rar" | "tar" | "tgz" | "tbz" | "tbz2" | "txz" | "gz" | "bz2" | "xz" => FileKind::Archive,
        _ => FileKind::Other,
    }
}

/// Action ids shown on the wheel for a selection. Mirrors DragFileKind.actions:
/// packing actions trail the kind-specific ones; mixed selections only pack or
/// extract. More than MAXIMUM_INPUTS items show no actions at all.
pub fn actions_for(kinds: &[FileKind], count: usize) -> Vec<&'static str> {
    if count == 0 || count > MAXIMUM_INPUTS || kinds.len() != count {
        return Vec::new();
    }
    let first = kinds[0];
    let mut packing = vec!["zip", if count > 1 { "zipEach" } else { "zipFast" }];
    if kinds.iter().any(|kind| *kind != first) {
        return if kinds.iter().all(|kind| matches!(kind, FileKind::Audio | FileKind::Video)) {
            packing.insert(0, "m4a");
            packing.insert(1, "wav");
            packing.insert(2, "aiff");
            packing
        } else {
            packing.push("tarGz");
            packing
        };
    }
    let mut actions: Vec<&'static str> = Vec::new();
    match first {
        FileKind::Image => {
            actions.extend(["jpg", "png", "webp", "heic", "tiff", "small", "imagesPDF"]);
        }
        FileKind::Pdf => {
            if count > 1 {
                actions.push("mergePDF");
            }
            actions.extend(["pdfPNG", "pdfJPG", "splitPDF"]);
        }
        FileKind::Video => {
            actions.extend(["mp4", "mov", "video720", "video1080", "m4a"]);
        }
        FileKind::Audio => {
            actions.extend(["m4a", "wav", "aiff"]);
        }
        FileKind::Archive => {
            return vec!["extract", "zip", "tarGz"];
        }
        FileKind::Other => {
            packing.push("tarGz");
            return packing;
        }
    }
    actions.extend(packing);
    actions
}

/// Actions that consume the whole selection in one output (the rest run once
/// per file).
pub fn combines_inputs(action: &str) -> bool {
    matches!(action, "imagesPDF" | "mergePDF" | "zip" | "zipFast" | "tarGz")
}

pub fn output_extension(action: &str) -> &'static str {
    match action {
        "small" | "pdfJPG" => "jpg",
        "pdfPNG" => "png",
        "imagesPDF" | "mergePDF" | "splitPDF" => "pdf",
        "video720" | "video1080" => "mp4",
        "zipEach" | "zipFast" => "zip",
        "tarGz" => "tar.gz",
        "jpg" | "jpeg" => "jpg",
        "png" => "png",
        "webp" => "webp",
        "tiff" => "tiff",
        "zip" => "zip",
        "mp4" => "mp4",
        "mov" => "mov",
        "m4a" => "m4a",
        "wav" => "wav",
        "aiff" => "aiff",
        _ => "out",
    }
}

// ---------------------------------------------------------------------------
// Degraded-action availability (pdfium / ffmpeg / HEIC codecs)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Availability {
    pub pdfium: bool,
    pub ffmpeg: bool,
}

pub fn action_available(action: &str, availability: &Availability) -> bool {
    match action {
        "pdfPNG" | "pdfJPG" => availability.pdfium,
        "mp4" | "mov" | "video720" | "video1080" | "m4a" | "wav" | "aiff" => availability.ffmpeg,
        // No HEIC encoder ships in the pure-Rust image stack.
        "heic" => false,
        _ => true,
    }
}

pub fn unavailable_reason(action: &str) -> String {
    match action {
        "pdfPNG" | "pdfJPG" => issue(
            "此操作需要 pdfium.dll（放在应用目录或 PATH 中），暂不可用。",
            "This action needs pdfium.dll (app folder or PATH); unavailable right now.",
        ),
        "mp4" | "mov" | "video720" | "video1080" | "m4a" | "wav" | "aiff" => issue(
            "音视频操作需要系统 PATH 中的 ffmpeg，暂不可用。",
            "Audio and video actions need ffmpeg on the system PATH; unavailable right now.",
        ),
        "heic" => issue("Windows 暂不支持导出 HEIC。", "HEIC export is not supported on Windows yet."),
        _ => issue("该文件的内容或编码不支持此操作", "Unsupported content or codec"),
    }
}

fn path_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(exe) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|p| p.to_path_buf())) {
        dirs.push(exe);
    }
    if let Some(paths) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&paths));
    }
    dirs
}

pub fn find_library(name: &str) -> Option<PathBuf> {
    path_dirs().into_iter().map(|dir| dir.join(name)).find(|candidate| candidate.is_file())
}

pub fn detect_availability() -> Availability {
    Availability {
        pdfium: find_library("pdfium.dll").is_some(),
        ffmpeg: find_library("ffmpeg.exe").is_some(),
    }
}

// ---------------------------------------------------------------------------
// Job control shared between the worker snapshot and the conversion thread

#[derive(Default)]
pub struct JobControl {
    cancelled: AtomicBool,
    progress: Mutex<f64>,
    current_file: Mutex<String>,
    completed: std::sync::atomic::AtomicUsize,
}

impl JobControl {
    pub fn new() -> JobControl {
        JobControl::default()
    }

    /// Counts a finished group (item) inside the running batch.
    pub fn step_done(&self) {
        self.completed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn completed(&self) -> usize {
        self.completed.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn set_progress(&self, value: f64) {
        *self.progress.lock().unwrap() = value.clamp(0.0, 1.0);
    }

    pub fn progress(&self) -> f64 {
        *self.progress.lock().unwrap()
    }

    pub fn set_current_file(&self, name: String) {
        *self.current_file.lock().unwrap() = name;
    }

    pub fn current_file(&self) -> String {
        self.current_file.lock().unwrap().clone()
    }

    pub fn check(&self) -> Result<(), String> {
        if self.is_cancelled() {
            Err(issue("已取消；已完成的副本保留", "Cancelled; completed copies kept"))
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Output staging (DragOutput parity): temp entry in the destination folder,
// exclusive publish under an auto-numbered name.

fn truncate_stem(stem: &str) -> String {
    stem.chars().take(MAXIMUM_NAME_CHARS).collect()
}

/// Builds the output in a hidden temp entry next to the destination and
/// publishes it with an exclusive rename. A failed build removes the temp
/// entry and never replaces an existing file.
pub fn publish_output<F>(
    directory: &Path,
    stem: &str,
    ext: &str,
    directory_output: bool,
    build: F,
) -> Result<PathBuf, String>
where
    F: FnOnce(&Path) -> Result<(), String>,
{
    std::fs::create_dir_all(directory).map_err(|e| issue("无法保存副本，请检查输出文件夹权限", &format!("Cannot save copy: {e}")))?;
    let temp = if directory_output {
        let staging = directory.join(format!(".nuphy-convert-{}", uuid_v4()));
        std::fs::create_dir_all(&staging)
            .map_err(|e| issue("无法保存副本，请检查输出文件夹权限", &format!("Cannot save copy: {e}")))?;
        staging
    } else {
        directory.join(format!(".nuphy-convert-{}.{}.tmp", uuid_v4(), ext))
    };
    let publish = |target: &Path| -> Result<(), std::io::Error> { std::fs::rename(&temp, target) };
    let cleanup = || {
        if directory_output {
            let _ = std::fs::remove_dir_all(&temp);
        } else {
            let _ = std::fs::remove_file(&temp);
        }
    };
    if let Err(message) = build(&temp) {
        cleanup();
        return Err(message);
    }
    let stem = truncate_stem(stem);
    for number in 0..10_000u32 {
        let name = if number == 0 {
            stem.clone()
        } else {
            format!("{stem}-{number}")
        };
        let target = match ext {
            "" => directory.join(&name),
            other => directory.join(format!("{name}.{other}")),
        };
        if target.exists() {
            continue;
        }
        match publish(&target) {
            Ok(()) => return Ok(target),
            Err(error) if target.exists() => {
                let _ = error;
                continue;
            }
            Err(error) => {
                cleanup();
                return Err(issue("无法保存副本，请检查输出文件夹权限", &format!("Cannot save copy: {error}")));
            }
        }
    }
    cleanup();
    Err(issue("同名副本已达编号上限", "Too many same-named copies"))
}

pub fn validate_regular_file(path: &Path, max_bytes: u64) -> Result<u64, String> {
    let meta = std::fs::metadata(path).map_err(|_| issue("无法读取文件，请先下载到本机并检查权限", "Cannot read file"))?;
    if !meta.is_file() {
        return Err(issue("无法读取文件，请先下载到本机并检查权限", "Cannot read file"));
    }
    let size = meta.len();
    if size > max_bytes {
        return Err(issue("文件超过处理限制", "File exceeds processing limits"));
    }
    Ok(size)
}

fn output_directory(input: &Path, prefs: &DragPreferences) -> PathBuf {
    match &prefs.output_directory {
        Some(directory) if !directory.is_empty() => PathBuf::from(directory),
        _ => input.parent().map(|parent| parent.to_path_buf()).unwrap_or_else(|| PathBuf::from(".")),
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".into())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Natural filename ordering (localizedStandardCompare parity: digit runs
/// compare numerically, everything else case-insensitively).
pub fn natural_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let left_chars: Vec<char> = left.to_lowercase().chars().collect();
    let right_chars: Vec<char> = right.to_lowercase().chars().collect();
    let mut left_index = 0usize;
    let mut right_index = 0usize;
    while left_index < left_chars.len() && right_index < right_chars.len() {
        let a = left_chars[left_index];
        let b = right_chars[right_index];
        if a.is_ascii_digit() && b.is_ascii_digit() {
            let mut a_digits = String::new();
            let mut b_digits = String::new();
            while left_index < left_chars.len() && left_chars[left_index].is_ascii_digit() {
                a_digits.push(left_chars[left_index]);
                left_index += 1;
            }
            while right_index < right_chars.len() && right_chars[right_index].is_ascii_digit() {
                b_digits.push(right_chars[right_index]);
                right_index += 1;
            }
            let a_number = a_digits.trim_start_matches('0');
            let b_number = b_digits.trim_start_matches('0');
            let ordering = a_number
                .len()
                .cmp(&b_number.len())
                .then_with(|| a_number.cmp(b_number))
                .then_with(|| a_digits.len().cmp(&b_digits.len()));
            if ordering != Ordering::Equal {
                return ordering;
            }
        } else {
            let ordering = a.cmp(&b);
            if ordering != Ordering::Equal {
                return ordering;
            }
            left_index += 1;
            right_index += 1;
        }
    }
    (left_chars.len() - left_index).cmp(&(right_chars.len() - right_index))
}

pub fn natural_sort(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort_by(|a, b| natural_cmp(&file_name(a), &file_name(b)));
    paths
}

// ---------------------------------------------------------------------------
// Conversion dispatch (DragConverter.convert parity)

pub fn convert(files: &[PathBuf], action: &str, prefs: &DragPreferences, control: &JobControl) -> Result<PathBuf, String> {
    if files.is_empty() || files.len() > MAXIMUM_INPUTS {
        return Err(issue("该文件的内容或编码不支持此操作", "Unsupported content or codec"));
    }
    control.check()?;
    if !action_available(action, &detect_availability()) {
        return Err(unavailable_reason(action));
    }
    match action {
        "jpg" | "png" | "webp" | "heic" | "tiff" | "small" => convert_image(&files[0], action, prefs, control),
        "imagesPDF" => convert_images_pdf(files, prefs, control),
        "zip" | "zipFast" | "zipEach" | "tarGz" => convert_pack(files, action, prefs, control),
        "extract" => convert_extract(&files[0], prefs, control),
        "mergePDF" => convert_merge_pdf(files, prefs, control),
        "splitPDF" => convert_split_pdf(&files[0], prefs, control),
        "pdfPNG" | "pdfJPG" => convert_pdf_pages(&files[0], action, prefs, control),
        "mp4" | "mov" | "video720" | "video1080" | "m4a" | "wav" | "aiff" => {
            convert_media(&files[0], action, prefs, control)
        }
        _ => Err(issue("该文件的内容或编码不支持此操作", "Unsupported content or codec")),
    }
}

// ---------------------------------------------------------------------------
// Images

fn decode_image(input: &Path) -> Result<image::DynamicImage, String> {
    let ext = input
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_default();
    if ext == "heic" || ext == "heif" {
        return Err(issue("Windows 暂不支持 HEIC 输入。", "HEIC input is not supported on Windows yet."));
    }
    validate_regular_file(input, MAXIMUM_IMAGE_BYTES)?;
    let (width, height) =
        image::image_dimensions(input).map_err(|_| issue("无法读取文件，请先下载到本机并检查权限", "Cannot read file"))?;
    if width == 0 || height == 0 {
        return Err(issue("无法读取文件，请先下载到本机并检查权限", "Cannot read file"));
    }
    if width as u64 * height as u64 > MAXIMUM_IMAGE_PIXELS {
        return Err(issue("文件超过处理限制", "File exceeds processing limits"));
    }
    image::open(input).map_err(|_| issue("无法读取文件，请先下载到本机并检查权限", "Cannot read file"))
}

/// Blends transparency onto white (JPG / HEIC behavior on the Mac).
fn flatten_white(image: &image::DynamicImage) -> image::RgbImage {
    let rgba = image.to_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    let mut pixels = Vec::with_capacity((width as usize) * (height as usize) * 3);
    for channel in rgba.pixels() {
        let [r, g, b, a] = channel.0;
        let blend = |value: u8| -> u8 { ((value as u32 * a as u32 + 255 * (255 - a as u32)) / 255) as u8 };
        pixels.push(blend(r));
        pixels.push(blend(g));
        pixels.push(blend(b));
    }
    image::RgbImage::from_raw(width, height, pixels).expect("flattened rgb buffer")
}

fn encode_image(output: &Path, image: &image::DynamicImage, action: &str, quality: f64) -> Result<(), String> {
    let file = std::fs::File::create(output).map_err(|e| issue("无法保存副本，请检查输出文件夹权限", &format!("Cannot save copy: {e}")))?;
    let mut file = std::io::BufWriter::new(file);
    match action {
        "jpg" | "small" => {
            let flat = flatten_white(image);
            let (width, height) = (flat.width(), flat.height());
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut file, (quality * 100.0).round().clamp(1.0, 100.0) as u8);
            encoder
                .write_image(flat.as_raw(), width, height, image::ExtendedColorType::Rgb8)
                .map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))
        }
        "png" => image::DynamicImage::write_to(image, &mut file, image::ImageFormat::Png)
            .map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}"))),
        "webp" => {
            // Pure-Rust WebP output is lossless; quality below 1 also takes
            // the lossless path (recorded as a known Windows degradation).
            let rgba = image.to_rgba8();
            if rgba.width() > MAXIMUM_WEBP_EDGE || rgba.height() > MAXIMUM_WEBP_EDGE {
                return Err(issue("文件超过处理限制", "File exceeds processing limits"));
            }
            image::DynamicImage::write_to(image, &mut file, image::ImageFormat::WebP)
                .map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))
        }
        "tiff" => image::DynamicImage::write_to(image, &mut file, image::ImageFormat::Tiff)
            .map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}"))),
        _ => Err(issue("该文件的内容或编码不支持此操作", "Unsupported content or codec")),
    }
}

fn convert_image(input: &Path, action: &str, prefs: &DragPreferences, control: &JobControl) -> Result<PathBuf, String> {
    control.check()?;
    let mut image = decode_image(input)?;
    if action == "small" {
        let maximum = prefs.max_dimension.max(1);
        image = image.thumbnail(maximum, maximum);
    }
    let extension = output_extension(action);
    let suffix = if action == "small" { "-small" } else { "" };
    let stem = format!("{}{}", file_stem(input), suffix);
    let directory = output_directory(input, prefs);
    let quality = prefs.quality;
    publish_output(&directory, &stem, extension, false, |output| {
        control.check()?;
        encode_image(output, &image, action, quality)?;
        control.check()
    })
}

// ---------------------------------------------------------------------------
// imagesPDF: images in filename order become one PDF with JPEG payloads

fn convert_images_pdf(files: &[PathBuf], prefs: &DragPreferences, control: &JobControl) -> Result<PathBuf, String> {
    let ordered = natural_sort(files.to_vec());
    let directory = output_directory(&ordered[0], prefs);
    let quality = prefs.quality;
    let count = ordered.len();
    let sources = ordered.clone();
    publish_output(&directory, &file_stem(&ordered[0]), "pdf", false, |output| {
        let mut pdf = pdf_writer::Pdf::new();
        let catalog_id = pdf_writer::Ref::new(1);
        let page_tree_id = pdf_writer::Ref::new(2);
        pdf.catalog(catalog_id).pages(page_tree_id);
        let mut page_ids = Vec::with_capacity(sources.len());
        let mut image_ids = Vec::with_capacity(sources.len());
        let mut content_ids = Vec::with_capacity(sources.len());
        let mut base = 3i32;
        for (index, source) in sources.iter().enumerate() {
            control.check()?;
            let image = decode_image(source)?;
            let flat = flatten_white(&image);
            let (width, height) = (flat.width(), flat.height());
            let mut encoded = Vec::new();
            let encoder =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, (quality * 100.0).round().clamp(1.0, 100.0) as u8);
            encoder
                .write_image(flat.as_raw(), width, height, image::ExtendedColorType::Rgb8)
                .map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))?;
            let page_id = pdf_writer::Ref::new(base);
            let image_id = pdf_writer::Ref::new(base + 1);
            let content_id = pdf_writer::Ref::new(base + 2);
            base += 3;
            let mut object = pdf.image_xobject(image_id, &encoded);
            object.filter(pdf_writer::Filter::DctDecode);
            object.width(width as i32);
            object.height(height as i32);
            object.color_space().device_rgb();
            object.bits_per_component(8);
            object.finish();
            let mut page = pdf.page(page_id);
            page.media_box(pdf_writer::Rect::new(0.0, 0.0, width as f32, height as f32));
            page.parent(page_tree_id);
            page.contents(content_id);
            page.resources().x_objects().pair(pdf_writer::Name(b"Im1"), image_id);
            page.finish();
            let mut content = pdf_writer::Content::new();
            content.save_state();
            content.transform([width as f32, 0.0, 0.0, height as f32, 0.0, 0.0]);
            content.x_object(pdf_writer::Name(b"Im1"));
            content.restore_state();
            pdf.stream(content_id, &content.finish());
            page_ids.push(page_id);
            image_ids.push(image_id);
            content_ids.push(content_id);
            control.set_progress((index + 1) as f64 / count as f64);
        }
        pdf.pages(page_tree_id).kids(page_ids.clone()).count(page_ids.len() as i32);
        std::fs::write(output, pdf.finish()).map_err(|e| issue("无法保存副本，请检查输出文件夹权限", &format!("Cannot save copy: {e}")))?;
        control.check()
    })
}

// ---------------------------------------------------------------------------
// Archives: manifest walk (dedupe + nesting filter + collision numbering),
// ZIP (deflate / store) and TAR.GZ packing, plus extraction.

struct ManifestEntry {
    path: PathBuf,
    name: String,
    is_dir: bool,
}

fn archive_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().replace(['\\', '/'], "_"))
        .unwrap_or_else(|| "item".into())
}

fn build_manifest(inputs: &[PathBuf], control: &JobControl) -> Result<Vec<ManifestEntry>, String> {
    let mut tops: Vec<PathBuf> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for path in inputs {
        let canonical = normalize_path(path);
        if seen.insert(canonical.clone()) {
            tops.push(canonical);
        }
    }
    // A parent and its selected child would double-store the child.
    let nested: Vec<PathBuf> = tops
        .iter()
        .filter(|path| tops.iter().any(|other| *other != **path && path.starts_with(other)))
        .cloned()
        .collect();
    tops.retain(|path| !nested.contains(path));
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut items: Vec<ManifestEntry> = Vec::new();
    let mut bytes: u64 = 0;
    let mut pending: Vec<(PathBuf, String)> = Vec::new();
    for path in &tops {
        let base = archive_name(path);
        let mut name = base.clone();
        let mut number = 1;
        let key = name.to_lowercase();
        while used.contains(&key) {
            number += 1;
            let stem = Path::new(&base).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| base.clone());
            let ext = Path::new(&base).extension().map(|e| e.to_string_lossy().into_owned());
            name = match ext {
                Some(ext) if !ext.is_empty() => format!("{stem}-{number}.{ext}"),
                _ => format!("{base}-{number}"),
            };
        }
        used.insert(name.to_lowercase());
        pending.push((path.clone(), name));
    }
    while let Some((path, name)) = pending.pop() {
        control.check()?;
        let meta = std::fs::metadata(&path).map_err(|_| issue("无法读取文件，请先下载到本机并检查权限", "Cannot read file"))?;
        if items.len() >= MAXIMUM_ARCHIVE_ENTRIES {
            return Err(issue("超过 20,000 项或 8 GB 展开大小的处理上限", "Limit: 20,000 entries or 8 GB expanded"));
        }
        let is_dir = meta.is_dir();
        items.push(ManifestEntry { path: path.clone(), name: name.clone(), is_dir });
        if is_dir {
            let mut children: Vec<PathBuf> = std::fs::read_dir(&path)
                .map_err(|_| issue("无法读取文件，请先下载到本机并检查权限", "Cannot read file"))?
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .collect();
            children.sort();
            if items.len() + pending.len() + children.len() > MAXIMUM_ARCHIVE_ENTRIES {
                return Err(issue("超过 20,000 项或 8 GB 展开大小的处理上限", "Limit: 20,000 entries or 8 GB expanded"));
            }
            for child in children {
                let name = format!("{name}/{}", archive_name(&child));
                pending.push((child, name));
            }
        } else {
            let size = meta.len();
            if size > MAXIMUM_ARCHIVE_BYTES.saturating_sub(bytes) {
                return Err(issue("超过 20,000 项或 8 GB 展开大小的处理上限", "Limit: 20,000 entries or 8 GB expanded"));
            }
            bytes += size;
        }
    }
    Ok(items)
}

fn pack_zip(items: &[ManifestEntry], output: &Path, deflate: bool) -> Result<(), String> {
    let file = std::fs::File::create(output).map_err(|e| issue("无法保存副本，请检查输出文件夹权限", &format!("Cannot save copy: {e}")))?;
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(if deflate { zip::CompressionMethod::Deflated } else { zip::CompressionMethod::Stored })
        .large_file(true);
    for entry in items {
        if entry.is_dir {
            writer
                .add_directory(format!("{}/", entry.name), options)
                .map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))?;
        } else {
            let mut source = std::fs::File::open(&entry.path)
                .map_err(|e| issue("无法读取文件，请先下载到本机并检查权限", &format!("Cannot read file: {e}")))?;
            writer
                .start_file(entry.name.as_str(), options)
                .map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))?;
            std::io::copy(&mut source, &mut writer)
                .map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))?;
        }
    }
    writer
        .finish()
        .map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))?;
    Ok(())
}

fn pack_tar_gz(items: &[ManifestEntry], output: &Path, control: &JobControl) -> Result<(), String> {
    let file = std::fs::File::create(output).map_err(|e| issue("无法保存副本，请检查输出文件夹权限", &format!("Cannot save copy: {e}")))?;
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for entry in items {
        control.check()?;
        if entry.is_dir {
            builder
                .append_dir_all(&entry.name, &entry.path)
                .map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))?;
        } else {
            builder
                .append_path_with_name(&entry.path, &entry.name)
                .map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))?;
        }
    }
    let encoder = builder
        .into_inner()
        .map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))?;
    encoder
        .finish()
        .map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))?;
    Ok(())
}

fn convert_pack(files: &[PathBuf], action: &str, prefs: &DragPreferences, control: &JobControl) -> Result<PathBuf, String> {
    if action == "zipEach" {
        // One archive per selected item; each publishes on its own.
        let directory = output_directory(&files[0], prefs);
        let mut outputs = Vec::new();
        for (index, file) in files.iter().enumerate() {
            control.check()?;
            control.set_current_file(file_name(file));
            control.set_progress(index as f64 / files.len() as f64);
            let items = build_manifest(std::slice::from_ref(file), control)?;
            let output = publish_output(&directory, &file_stem(file), "zip", false, |target| pack_zip(&items, target, false))?;
            outputs.push(output);
        }
        // The batch records the last output; earlier ones stay in the folder.
        return Ok(outputs.pop().ok_or_else(|| issue("该文件的内容或编码不支持此操作", "Unsupported content or codec"))?);
    }
    let items = build_manifest(files, control)?;
    let stem = if files.len() == 1 { file_stem(&files[0]) } else { "Archive".into() };
    let directory = output_directory(&files[0], prefs);
    let extension = output_extension(action);
    control.set_current_file(format!("{} / {}", stem, extension));
    publish_output(&directory, &stem, extension, false, |target| {
        control.check()?;
        match action {
            "zip" => pack_zip(&items, target, true),
            "zipFast" => pack_zip(&items, target, false),
            "tarGz" => pack_tar_gz(&items, target, control),
            _ => Err(issue("该文件的内容或编码不支持此操作", "Unsupported content or codec")),
        }
    })
}

/// Archive-entry path safety (DragArchiveConverter.safeComponents parity).
pub fn safe_components(name: &str) -> Result<Vec<String>, String> {
    let unsafe_message = issue("压缩包包含异常路径或重名条目，已停止解压", "Unsafe or duplicate archive paths");
    if name.is_empty()
        || name.len() > 2048
        || name.starts_with('/')
        || name.starts_with('\\')
        || name.contains('\\')
        || name.contains('\0')
    {
        return Err(unsafe_message);
    }
    if name.len() >= 2 && name.as_bytes()[1] == b':' {
        return Err(unsafe_message);
    }
    let mut components = Vec::new();
    for part in name.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.len() > 255 {
            return Err(unsafe_message);
        }
        components.push(part.to_string());
    }
    if components.len() > 64 || components.is_empty() {
        return Err(unsafe_message);
    }
    Ok(components)
}

fn count_extract_caps(count: usize, bytes: u64) -> Result<(), String> {
    if count > MAXIMUM_ARCHIVE_ENTRIES || bytes > MAXIMUM_ARCHIVE_BYTES {
        return Err(issue("超过 20,000 项或 8 GB 展开大小的处理上限", "Limit: 20,000 entries or 8 GB expanded"));
    }
    Ok(())
}

fn extract_zip(input: &Path, destination: &Path, control: &JobControl) -> Result<(), String> {
    let file = std::fs::File::open(input).map_err(|_| issue("无法读取文件，请先下载到本机并检查权限", "Cannot read file"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|_| issue("压缩包损坏或编码暂不支持", "Damaged or unsupported archive"))?;
    let mut entries = 0usize;
    let mut bytes: u64 = 0;
    for index in 0..archive.len() {
        control.check()?;
        entries += 1;
        count_extract_caps(entries, bytes)?;
        let mut entry = archive
            .by_index(index)
            .map_err(|_| issue("压缩包损坏或编码暂不支持", "Damaged or unsupported archive"))?;
        let Some(relative) = entry.enclosed_name() else {
            return Err(issue("压缩包包含异常路径或重名条目，已停止解压", "Unsafe or duplicate archive paths"));
        };
        let target = destination.join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| issue("无法保存副本，请检查输出文件夹权限", &format!("Cannot save copy: {e}")))?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| issue("无法保存副本，请检查输出文件夹权限", &format!("Cannot save copy: {e}")))?;
            }
            let mut output = std::fs::File::create(&target)
                .map_err(|e| issue("无法保存副本，请检查输出文件夹权限", &format!("Cannot save copy: {e}")))?;
            bytes += entry.size();
            count_extract_caps(entries, bytes)?;
            std::io::copy(&mut entry, &mut output)
                .map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))?;
        }
    }
    Ok(())
}

fn extract_tar<R: Read>(reader: R, destination: &Path, control: &JobControl) -> Result<(), String> {
    let mut archive = tar::Archive::new(reader);
    let mut entries = 0usize;
    let mut bytes: u64 = 0;
    for entry in archive
        .entries()
        .map_err(|_| issue("压缩包损坏或编码暂不支持", "Damaged or unsupported archive"))?
    {
        control.check()?;
        let mut entry = entry.map_err(|_| issue("压缩包损坏或编码暂不支持", "Damaged or unsupported archive"))?;
        let path = entry
            .path()
            .map_err(|_| issue("压缩包损坏或编码暂不支持", "Damaged or unsupported archive"))?
            .to_path_buf();
        let name = path.to_string_lossy().replace('\\', "/");
        let components = safe_components(&name)?;
        entries += 1;
        let header_type = entry.header().entry_type();
        if matches!(header_type, tar::EntryType::Symlink | tar::EntryType::Link) {
            return Err(issue("暂不处理符号链接、硬链接或特殊文件", "Links and special files are not supported yet"));
        }
        let target = components.iter().fold(destination.to_path_buf(), |base, part| base.join(part));
        match header_type {
            tar::EntryType::Directory => {
                std::fs::create_dir_all(&target).map_err(|e| issue("无法保存副本，请检查输出文件夹权限", &format!("Cannot save copy: {e}")))?;
            }
            _ => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| issue("无法保存副本，请检查输出文件夹权限", &format!("Cannot save copy: {e}")))?;
                }
                let mut output = std::fs::File::create(&target)
                    .map_err(|e| issue("无法保存副本，请检查输出文件夹权限", &format!("Cannot save copy: {e}")))?;
                let written = std::io::copy(&mut entry, &mut output)
                    .map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))?;
                bytes += written;
                count_extract_caps(entries, bytes)?;
            }
        }
    }
    Ok(())
}

fn decompress_stream<R: Read>(input: R, output: &Path) -> Result<(), String> {
    let mut reader = BufReader::new(input);
    let mut file = std::fs::File::create(output).map_err(|e| issue("无法保存副本，请检查输出文件夹权限", &format!("Cannot save copy: {e}")))?;
    std::io::copy(&mut reader, &mut file).map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))?;
    file.flush().map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))?;
    Ok(())
}

fn convert_extract(input: &Path, prefs: &DragPreferences, control: &JobControl) -> Result<PathBuf, String> {
    validate_regular_file(input, MAXIMUM_ARCHIVE_INPUT_BYTES)?;
    let ext = input
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_default();
    let full_name = file_name(input).to_ascii_lowercase();
    if matches!(ext.as_str(), "7z" | "rar") {
        return Err(issue("暂不支持该压缩格式，请改用 ZIP / TAR.GZ。", "This archive format is not supported yet; use ZIP / TAR.GZ."));
    }
    let directory = output_directory(input, prefs);
    let stem = file_stem(input);
    let stem = stem.strip_suffix(".tar").map(|s| s.to_string()).unwrap_or(stem);
    publish_output(&directory, &stem, "", true, |destination| {
        control.check()?;
        let is_tar = full_name.ends_with(".tar") || full_name.ends_with(".tar.gz") || full_name.ends_with(".tgz")
            || full_name.ends_with(".tar.bz2") || full_name.ends_with(".tbz") || full_name.ends_with(".tbz2")
            || full_name.ends_with(".tar.xz") || full_name.ends_with(".txz");
        match ext.as_str() {
            "zip" => extract_zip(input, destination, control),
            "tar" => extract_tar(BufReader::new(std::fs::File::open(input).map_err(|e| e.to_string())?), destination, control),
            "gz" | "bz2" | "xz" if is_tar => {
                let file = std::fs::File::open(input).map_err(|e| e.to_string())?;
                match ext.as_str() {
                    "gz" => extract_tar(flate2::read::GzDecoder::new(file), destination, control),
                    "bz2" => extract_tar(bzip2_rs::DecoderReader::new(file), destination, control),
                    _ => {
                        let mut output = Vec::new();
                        lzma_rs::xz_decompress(&mut BufReader::new(file), &mut output)
                            .map_err(|_| issue("压缩包损坏或编码暂不支持", "Damaged or unsupported archive"))?;
                        extract_tar(std::io::Cursor::new(output), destination, control)
                    }
                }
            }
            "gz" => {
                let file = std::fs::File::open(input).map_err(|e| e.to_string())?;
                let name = file_stem(input);
                decompress_stream(flate2::read::GzDecoder::new(file), &destination.join(name))
            }
            "bz2" => {
                let file = std::fs::File::open(input).map_err(|e| e.to_string())?;
                let name = file_stem(input);
                decompress_stream(bzip2_rs::DecoderReader::new(file), &destination.join(name))
            }
            "xz" => {
                let file = std::fs::File::open(input).map_err(|e| e.to_string())?;
                let name = file_stem(input);
                let mut output = Vec::new();
                lzma_rs::xz_decompress(&mut BufReader::new(file), &mut output)
                    .map_err(|_| issue("压缩包损坏或编码暂不支持", "Damaged or unsupported archive"))?;
                std::fs::write(destination.join(name), output)
                    .map_err(|e| issue("无法保存副本，请检查输出文件夹权限", &format!("Cannot save copy: {e}")))?;
                Ok(())
            }
            _ => Err(issue("暂不支持该压缩格式，请改用 ZIP / TAR.GZ。", "This archive format is not supported yet; use ZIP / TAR.GZ.")),
        }?;
        control.check()
    })
}

// ---------------------------------------------------------------------------
// PDF documents (lopdf merge/split, pdfium rendering)

fn lopdf_issue() -> String {
    issue("压缩包损坏或编码暂不支持", "Damaged or unsupported archive")
}

fn load_pdf(input: &Path) -> Result<lopdf::Document, String> {
    validate_regular_file(input, MAXIMUM_DOCUMENT_BYTES)?;
    let document = lopdf::Document::load(input).map_err(|_| issue("无法读取文件，请先下载到本机并检查权限", "Cannot read file"))?;
    let pages = document.get_pages();
    if pages.is_empty() {
        return Err(issue("PDF 没有可用页面", "PDF has no pages"));
    }
    if pages.len() > MAXIMUM_PDF_PAGES {
        return Err(issue("文件超过处理限制", "File exceeds processing limits"));
    }
    Ok(document)
}

fn inherited_page_attribute(document: &lopdf::Document, page_id: lopdf::ObjectId, key: &[u8]) -> Result<Option<lopdf::Object>, String> {
    let mut current = page_id;
    let mut visited = std::collections::HashSet::new();
    loop {
        if !visited.insert(current) {
            return Err(lopdf_issue());
        }
        let node = document.get_dictionary(current).map_err(|_| lopdf_issue())?;
        if let Ok(value) = node.get(key) {
            match document.dereference(value) {
                Ok((_, lopdf::Object::Null)) | Err(_) => {}
                Ok(_) => return Ok(Some(value.clone())),
            }
        }
        match node.get(b"Parent") {
            Err(_) | Ok(lopdf::Object::Null) => return Ok(None),
            Ok(parent) => current = parent.as_reference().map_err(|_| lopdf_issue())?,
        }
    }
}

fn materialize_page_attributes(document: &lopdf::Document, page_id: lopdf::ObjectId) -> Result<lopdf::Dictionary, String> {
    let mut page = document.get_dictionary(page_id).map_err(|_| lopdf_issue())?.clone();
    let media_box = inherited_page_attribute(document, page_id, b"MediaBox")?
        .ok_or_else(|| issue("无法读取文件，请先下载到本机并检查权限", "Cannot read file"))?;
    let crop_box = inherited_page_attribute(document, page_id, b"CropBox")?.unwrap_or_else(|| media_box.clone());
    let resources = inherited_page_attribute(document, page_id, b"Resources")?
        .unwrap_or_else(|| lopdf::Object::Dictionary(lopdf::Dictionary::new()));
    let rotation = inherited_page_attribute(document, page_id, b"Rotate")?.unwrap_or(lopdf::Object::Integer(0));
    page.set("MediaBox", media_box);
    page.set("CropBox", crop_box);
    page.set("Resources", resources);
    page.set("Rotate", rotation);
    Ok(page)
}

/// Ports lopdf's official merge example (MIT): materialize page attributes,
/// renumber every source, then rebuild one Pages tree / Catalog over all pages.
fn merge_documents(documents: Vec<lopdf::Document>) -> Result<lopdf::Document, String> {
    let mut document = lopdf::Document::with_version("1.5");
    let mut max_id = 1u32;
    let mut documents_pages: BTreeMap<lopdf::ObjectId, lopdf::Object> = BTreeMap::new();
    let mut documents_objects: BTreeMap<lopdf::ObjectId, lopdf::Object> = BTreeMap::new();
    let mut catalog_object: Option<(lopdf::ObjectId, lopdf::Object)> = None;
    let mut pages_object: Option<(lopdf::ObjectId, lopdf::Object)> = None;
    for mut source in documents {
        let page_ids: Vec<lopdf::ObjectId> = source.get_pages().into_values().collect();
        for page_id in page_ids {
            let page = materialize_page_attributes(&source, page_id)?;
            source.objects.insert(page_id, lopdf::Object::Dictionary(page));
        }
        source.renumber_objects_with(max_id);
        max_id = source.max_id + 1;
        let page_ids: Vec<lopdf::ObjectId> = source.get_pages().into_values().collect();
        for page_id in page_ids {
            let object = source.get_object(page_id).map_err(|_| lopdf_issue())?.clone();
            documents_pages.insert(page_id, object);
        }
        for (object_id, object) in source.objects {
            match object.type_name().unwrap_or(b"") {
                b"Catalog" => {
                    if catalog_object.is_none() {
                        catalog_object = Some((object_id, object));
                    }
                }
                b"Pages" => {
                    if let lopdf::Object::Dictionary(dictionary) = object {
                        let mut dictionary = dictionary;
                        if let Some((_, lopdf::Object::Dictionary(old))) = &pages_object {
                            dictionary.extend(old);
                        }
                        pages_object = Some((pages_object.map(|(id, _)| id).unwrap_or(object_id), lopdf::Object::Dictionary(dictionary)));
                    }
                }
                b"Page" | b"Outlines" | b"Outline" => {}
                _ => {
                    documents_objects.insert(object_id, object);
                }
            }
        }
    }
    let (pages_id, pages_object) = pages_object.ok_or_else(|| lopdf_issue())?;
    let (catalog_id, catalog_object) = catalog_object.ok_or_else(|| lopdf_issue())?;
    document.objects.extend(documents_objects);
    for (object_id, object) in documents_pages.iter() {
        if let lopdf::Object::Dictionary(dictionary) = object {
            let mut dictionary = dictionary.clone();
            dictionary.set("Parent", pages_id);
            document.objects.insert(*object_id, lopdf::Object::Dictionary(dictionary));
        }
    }
    if let lopdf::Object::Dictionary(dictionary) = pages_object {
        let mut dictionary = dictionary.clone();
        dictionary.remove(b"Parent");
        for key in ["MediaBox", "CropBox", "Resources", "Rotate"] {
            dictionary.remove(key.as_bytes());
        }
        dictionary.set("Count", documents_pages.len() as i64);
        dictionary.set("Kids", documents_pages.keys().map(|id| lopdf::Object::Reference(*id)).collect::<Vec<_>>());
        document.objects.insert(pages_id, lopdf::Object::Dictionary(dictionary));
    }
    if let lopdf::Object::Dictionary(dictionary) = catalog_object {
        let mut dictionary = dictionary;
        dictionary.set("Pages", pages_id);
        dictionary.remove(b"Outlines");
        document.objects.insert(catalog_id, lopdf::Object::Dictionary(dictionary));
    }
    document.trailer.set("Root", catalog_id);
    document.max_id = document.objects.len() as u32;
    document.renumber_objects();
    Ok(document)
}

fn convert_merge_pdf(files: &[PathBuf], prefs: &DragPreferences, control: &JobControl) -> Result<PathBuf, String> {
    if files.len() < 2 {
        return Err(issue("合并 PDF 需要多个文件。", "Merging PDFs needs multiple files."));
    }
    let ordered = natural_sort(files.to_vec());
    let mut total_pages = 0usize;
    let mut documents = Vec::new();
    for (index, input) in ordered.iter().enumerate() {
        control.check()?;
        control.set_current_file(file_name(input));
        control.set_progress(index as f64 / ordered.len() as f64);
        let document = load_pdf(input)?;
        total_pages += document.get_pages().len();
        if total_pages > MAXIMUM_PDF_PAGES {
            return Err(issue("文件超过处理限制", "File exceeds processing limits"));
        }
        documents.push(document);
    }
    let mut merged = merge_documents(documents)?;
    let directory = output_directory(&ordered[0], prefs);
    publish_output(&directory, &file_stem(&ordered[0]), "pdf", false, |output| {
        control.check()?;
        merged
            .save(output)
            .map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))?;
        control.check()
    })
}

fn convert_split_pdf(input: &Path, prefs: &DragPreferences, control: &JobControl) -> Result<PathBuf, String> {
    let document = load_pdf(input)?;
    let page_count = document.get_pages().len();
    let directory = output_directory(input, prefs);
    let stem = file_stem(input);
    publish_output(&directory, &stem, "", true, |destination| {
        for index in 1..=page_count as u32 {
            control.check()?;
            let mut single = document.clone();
            let others: Vec<u32> = (1..=page_count as u32).filter(|page| *page != index).collect();
            single.delete_pages(&others);
            single.prune_objects();
            single.renumber_objects();
            let output = destination.join(format!("page-{index:03}.pdf"));
            single
                .save(&output)
                .map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))?;
            control.set_progress(index as f64 / page_count as f64);
        }
        control.check()
    })
}

fn convert_pdf_pages(input: &Path, action: &str, prefs: &DragPreferences, control: &JobControl) -> Result<PathBuf, String> {
    let Some(library) = find_library("pdfium.dll") else {
        return Err(unavailable_reason(action));
    };
    validate_regular_file(input, MAXIMUM_DOCUMENT_BYTES)?;
    let bindings = pdfium_render::prelude::Pdfium::bind_to_library(&library)
        .or_else(|_| pdfium_render::prelude::Pdfium::bind_to_system_library())
        .map_err(|_| unavailable_reason(action))?;
    let pdfium = pdfium_render::prelude::Pdfium::new(bindings);
    let document = pdfium
        .load_pdf_from_file(input, None)
        .map_err(|_| issue("无法读取文件，请先下载到本机并检查权限", "Cannot read file"))?;
    let page_count = document.pages().len() as usize;
    if page_count == 0 {
        return Err(issue("PDF 没有可用页面", "PDF has no pages"));
    }
    if page_count > MAXIMUM_PDF_PAGES {
        return Err(issue("文件超过处理限制", "File exceeds processing limits"));
    }
    let directory = output_directory(input, prefs);
    let stem = file_stem(input);
    let suffix = if action == "pdfPNG" { "-png" } else { "-jpg" };
    let max_dimension = prefs.max_dimension as f32;
    let quality = prefs.quality;
    publish_output(&directory, &format!("{stem}{suffix}"), "", true, |destination| {
        for index in 0..page_count as u16 {
            control.check()?;
            let page = document.pages().get(index).map_err(|_| lopdf_issue())?;
            let width = page.width().value.max(1.0);
            let height = page.height().value.max(1.0);
            // Mac parity: 2x the page size, capped by the configured edge.
            let scale = (2.0f32).min(max_dimension / width.max(height));
            let target_width = ((width * scale).ceil() as i32).max(1);
            let target_height = ((height * scale).ceil() as i32).max(1);
            let config = pdfium_render::prelude::PdfRenderConfig::new()
                .set_target_width(target_width)
                .set_target_height(target_height);
            let bitmap = page.render_with_config(&config).map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))?;
            let rgba = bitmap.as_rgba_bytes();
            let (pixel_width, pixel_height) = (bitmap.width() as u32, bitmap.height() as u32);
            let image = image::RgbaImage::from_raw(pixel_width, pixel_height, rgba)
                .ok_or_else(|| issue("转换失败，原文件已保留", "Conversion failed"))?;
            let dynamic = image::DynamicImage::ImageRgba8(image);
            let output = destination.join(format!("page-{:03}.{}", index + 1, if action == "pdfPNG" { "png" } else { "jpg" }));
            encode_image(&output, &dynamic, if action == "pdfPNG" { "png" } else { "jpg" }, quality)?;
            control.set_progress((index + 1) as f64 / page_count as f64);
        }
        control.check()
    })
}

// ---------------------------------------------------------------------------
// Audio / video: sidecar ffmpeg from PATH or the app folder

fn ffmpeg_arguments(input: &Path, output: &Path, action: &str) -> Vec<String> {
    let input = input.to_string_lossy().into_owned();
    let output = output.to_string_lossy().into_owned();
    let base = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        input,
    ];
    match action {
        "mp4" | "mov" => {
            // Container remux first: keeps quality and stays fast.
            let mut args = base;
            args.push("-c".into());
            args.push("copy".into());
            args.push("-y".into());
            args.push(output);
            args
        }
        "video720" | "video1080" => {
            let height = if action == "video720" { 720 } else { 1080 };
            let mut args = base;
            args.extend(["-vf".into(), format!("scale=-2:{height}"), "-c:v".into(), "libx264".into(), "-preset".into(), "veryfast".into(), "-crf".into(), "23".into(), "-c:a".into(), "aac".into(), "-b:a".into(), "192k".into(), "-y".into()]);
            args.push(output);
            args
        }
        "m4a" => {
            let mut args = base;
            args.extend(["-vn".into(), "-c:a".into(), "aac".into(), "-b:a".into(), "192k".into(), "-y".into()]);
            args.push(output);
            args
        }
        "wav" => {
            let mut args = base;
            args.extend(["-vn".into(), "-ar".into(), "44100".into(), "-ac".into(), "2".into(), "-sample_fmt".into(), "s16".into(), "-y".into()]);
            args.push(output);
            args
        }
        "aiff" => {
            let mut args = base;
            args.extend(["-vn".into(), "-ar".into(), "44100".into(), "-ac".into(), "2".into(), "-c:a".into(), "pcm_s16be".into(), "-y".into()]);
            args.push(output);
            args
        }
        _ => Vec::new(),
    }
}

fn convert_media(input: &Path, action: &str, prefs: &DragPreferences, control: &JobControl) -> Result<PathBuf, String> {
    let Some(ffmpeg) = find_library("ffmpeg.exe") else {
        return Err(unavailable_reason(action));
    };
    validate_regular_file(input, MAXIMUM_MEDIA_BYTES)?;
    let directory = output_directory(input, prefs);
    let suffix = match action {
        "video720" => "-720p",
        "video1080" => "-1080p",
        _ => "",
    };
    let stem = format!("{}{}", file_stem(input), suffix);
    let extension = output_extension(action);
    control.set_current_file(file_name(input));
    publish_output(&directory, &stem, extension, false, |output| {
        control.check()?;
        let arguments = ffmpeg_arguments(input, output, action);
        let mut child = std::process::Command::new(&ffmpeg)
            .args(&arguments)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))?;
        // Poll for cancellation/timeout while the transcode runs.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1800);
        loop {
            match child.try_wait().map_err(|e| issue("转换失败，原文件已保留", &format!("Conversion failed: {e}")))? {
                Some(status) => {
                    if status.success() {
                        break;
                    }
                    cleanup_partial(output);
                    return Err(issue("转换失败，原文件已保留", "Conversion failed; original kept"));
                }
                None => {
                    if control.is_cancelled() {
                        let _ = child.kill();
                        let _ = child.wait();
                        cleanup_partial(output);
                        return Err(issue("已取消；已完成的副本保留", "Cancelled; completed copies kept"));
                    }
                    if std::time::Instant::now() > deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        cleanup_partial(output);
                        return Err(issue("转换超时，原文件已保留", "Conversion timed out"));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(120));
                }
            }
        }
        if !output.exists() || std::fs::metadata(output).map(|meta| meta.len()).unwrap_or(0) == 0 {
            cleanup_partial(output);
            return Err(issue("转换失败，原文件已保留", "Conversion failed; original kept"));
        }
        control.set_progress(1.0);
        control.check()
    })
}

fn cleanup_partial(output: &Path) {
    let _ = std::fs::remove_file(output);
}

#[cfg(test)]
mod tests;
