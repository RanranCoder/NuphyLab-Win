// Win32 clipboard capture and write for Keyboard Pocket, plus the injected
// Ctrl+C / Ctrl+V strokes that drive the store/retrieve flows. Formats:
// CF_HDROP (files), "HTML Format" (rich text), CF_UNICODETEXT, CF_DIB + "PNG"
// (images). SendInput sets the shell marker so hooks recognize our own strokes.

use std::io::Cursor;
use std::path::PathBuf;
use std::time::Duration;

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
    OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP};
use windows_sys::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
};

use crate::win::wide;

#[allow(dead_code)]
pub const CF_TEXT: u32 = 1;
pub const CF_DIB: u32 = 8;
pub const CF_UNICODETEXT: u32 = 13;
pub const CF_HDROP: u32 = 15;
pub const CF_HTML_NAME: &str = "HTML Format";
pub const CF_PNG_NAME: &str = "PNG";
pub const OWN_EVENT_MARKER: usize = 0x4E504B54; // "NPKT", Mac ownEventMarker parity

fn clipboard_format(name: &str) -> u32 {
    let wide = wide(name);
    unsafe { RegisterClipboardFormatW(wide.as_ptr()) }
}

/// Runs `body` with the clipboard open (retries while another app holds it).
fn with_clipboard<T>(body: impl FnOnce() -> Option<T>) -> Option<T> {
    for _ in 0..15 {
        let opened = unsafe { OpenClipboard(std::ptr::null_mut()) };
        if opened != 0 {
            let result = body();
            unsafe { CloseClipboard() };
            return result;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

fn is_available(format: u32) -> bool {
    unsafe { IsClipboardFormatAvailable(format) != 0 }
}

/// A captured clipboard payload. Conversion into a stored item happens on the
/// caller side (store owns asset persistence).
#[derive(Debug)]
pub struct Captured {
    pub kind: &'static str,
    pub text: Option<String>,
    pub image_png: Option<Vec<u8>>,
    pub rich_html: Option<Vec<u8>>,
    pub files: Option<Vec<PathBuf>>,
}

/// Reads the current clipboard. Priority mirrors PocketStore.capture: files,
/// rich text, plain text / link, image. Unsupported => Err.
pub fn capture() -> Result<Captured, String> {
    with_clipboard(|| {
        if is_available(CF_HDROP) {
            if let Some(paths) = read_hdrop() {
                if !paths.is_empty() {
                    return Some(Captured {
                        kind: "files",
                        text: None,
                        image_png: None,
                        rich_html: None,
                        files: Some(paths),
                    });
                }
            }
        }
        let html_format = clipboard_format(CF_HTML_NAME);
        let png_format = clipboard_format(CF_PNG_NAME);
        let plain = read_unicode_text();
        if is_available(html_format) {
            if let Some(fragment) = read_html_fragment(html_format) {
                return Some(Captured {
                    kind: "richText",
                    text: plain.clone().filter(|t| !t.is_empty()),
                    image_png: None,
                    rich_html: Some(fragment),
                    files: None,
                });
            }
        }
        if let Some(text) = plain.filter(|t| !t.is_empty()) {
            let kind = if is_supported_link(text.trim()) { "link" } else { "text" };
            return Some(Captured {
                kind,
                text: Some(text),
                image_png: None,
                rich_html: None,
                files: None,
            });
        }
        if is_available(png_format) {
            if let Some(data) = read_raw(png_format) {
                return Some(Captured {
                    kind: "image",
                    text: None,
                    image_png: Some(data),
                    rich_html: None,
                    files: None,
                });
            }
        }
        if is_available(CF_DIB) {
            if let Some(dib) = read_raw(CF_DIB) {
                if let Ok(png) = dib_to_png(&dib) {
                    return Some(Captured {
                        kind: "image",
                        text: None,
                        image_png: Some(png),
                        rich_html: None,
                        files: None,
                    });
                }
            }
        }
        None
    })
    .ok_or_else(|| {
        "剪贴板中没有可保存的文字、图片、链接或文件。 / The clipboard has no supported text, image, link or files to store.".to_string()
    })
}

fn read_unicode_text() -> Option<String> {
    if !is_available(CF_UNICODETEXT) {
        return None;
    }
    let handle = unsafe { GetClipboardData(CF_UNICODETEXT) };
    if handle.is_null() {
        return None;
    }
    let locked = unsafe { GlobalLock(handle as _) } as *const u16;
    if locked.is_null() {
        return None;
    }
    let mut length = 0usize;
    unsafe {
        while *locked.add(length) != 0 && length < 8 * 1024 * 1024 {
            length += 1;
        }
        let slice = std::slice::from_raw_parts(locked, length);
        let text = String::from_utf16_lossy(slice);
        GlobalUnlock(handle as _);
        Some(text)
    }
}

fn read_raw(format: u32) -> Option<Vec<u8>> {
    let handle = unsafe { GetClipboardData(format) };
    if handle.is_null() {
        return None;
    }
    let locked = unsafe { GlobalLock(handle as _) } as *const u8;
    if locked.is_null() {
        return None;
    }
    // GlobalSize gives the allocation bounds; clipboard writers may allocate
    // more than the logical payload, so bounds-check conservatively.
    let size = unsafe { GlobalSize(handle as _) };
    let size = size.min(64 * 1024 * 1024);
    let data = unsafe { std::slice::from_raw_parts(locked, size) }.to_vec();
    unsafe { GlobalUnlock(handle as _) };
    Some(data)
}

fn read_hdrop() -> Option<Vec<PathBuf>> {
    let handle = unsafe { GetClipboardData(CF_HDROP) };
    if handle.is_null() {
        return None;
    }
    let hdrop = handle as HDROP;
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, std::ptr::null_mut(), 0) };
    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        let length = unsafe { DragQueryFileW(hdrop, index, std::ptr::null_mut(), 0) };
        let mut buffer = vec![0u16; length as usize + 1];
        let written = unsafe { DragQueryFileW(hdrop, index, buffer.as_mut_ptr(), buffer.len() as u32) };
        buffer.truncate(written as usize);
        paths.push(PathBuf::from(String::from_utf16_lossy(&buffer)));
    }
    Some(paths)
}

/// Extracts the fragment between StartFragment/EndFragment byte offsets of a
/// Windows "HTML Format" payload. Malformed headers fall back to None.
fn read_html_fragment(format: u32) -> Option<Vec<u8>> {
    let data = read_raw(format)?;
    let text = String::from_utf8_lossy(&data);
    let mut start = None;
    let mut end = None;
    for line in text.lines() {
        if let Some(value) = line.trim().strip_prefix("StartFragment:") {
            start = value.trim().parse::<usize>().ok();
        }
        if let Some(value) = line.trim().strip_prefix("EndFragment:") {
            end = value.trim().parse::<usize>().ok();
        }
    }
    let (start, end) = (start?, end?);
    if start >= end || end > data.len() {
        return None;
    }
    Some(data[start..end].to_vec())
}

pub fn is_supported_link(value: &str) -> bool {
    if value.chars().any(char::is_whitespace) {
        return false;
    }
    let Ok(url) = url::Url::parse(value) else { return false };
    match url.scheme() {
        "http" | "https" => url.host_str().is_some_and(|host| !host.is_empty()),
        "mailto" => !url.path().is_empty(),
        _ => false,
    }
}

/// Pocket title heuristic: first non-empty line, trimmed to 80 chars.
pub fn title_for_text(value: &str) -> String {
    let first = value
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("文字");
    first.trim().chars().take(80).collect()
}

// ---------------------------------------------------------------------------
// Clipboard write

#[derive(Debug)]
pub enum WritePayload {
    Text { text: String },
    RichText { text: String, html_fragment: String },
    Image { png: Vec<u8> },
    Files { paths: Vec<PathBuf> },
}

/// Replaces the clipboard content. `plain_text` strips rich representations
/// from textual payloads only (Mac plainFor parity).
pub fn write_clipboard(payload: &WritePayload, plain_text: bool) -> Result<(), String> {
    let failure = "无法写入剪贴板。 / Could not write to the clipboard.".to_string();
    let mut prepared: Vec<(u32, Vec<u8>)> = Vec::new();
    match payload {
        WritePayload::Text { text } => {
            prepared.push((CF_UNICODETEXT, utf16_payload(text)));
        }
        WritePayload::RichText { text, html_fragment } => {
            prepared.push((CF_UNICODETEXT, utf16_payload(text)));
            if !plain_text {
                prepared.push((clipboard_format(CF_HTML_NAME), build_html_format(html_fragment)));
            }
        }
        WritePayload::Image { png } => {
            prepared.push((clipboard_format(CF_PNG_NAME), png.clone()));
            prepared.push((CF_DIB, png_to_dib(png)?));
        }
        WritePayload::Files { paths } => {
            if paths.is_empty() {
                return Err("文件引用为空。 / The file reference is empty.".into());
            }
            prepared.push((CF_HDROP, hdrop_payload(paths)));
        }
    }
    let opened = unsafe { OpenClipboard(std::ptr::null_mut()) };
    if opened == 0 {
        return Err(failure);
    }
    let result = (|| {
        unsafe { EmptyClipboard() };
        for (format, data) in &prepared {
            if set_data(*format, data).is_none() {
                return Err(failure.clone());
            }
        }
        Ok(())
    })();
    unsafe { CloseClipboard() };
    result
}

fn utf16_payload(text: &str) -> Vec<u8> {
    let mut units: Vec<u16> = text.encode_utf16().collect();
    units.push(0);
    let mut bytes = Vec::with_capacity(units.len() * 2);
    for unit in units {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// DROPFILES header + double-null-terminated wide path list.
fn hdrop_payload(paths: &[PathBuf]) -> Vec<u8> {
    let mut buffer = Vec::new();
    // DROPFILES: pFiles offset (20), POINT (8), fNC (4), fWide (4).
    buffer.extend_from_slice(&20u32.to_le_bytes());
    buffer.extend_from_slice(&0i32.to_le_bytes());
    buffer.extend_from_slice(&0i32.to_le_bytes());
    buffer.extend_from_slice(&0u32.to_le_bytes());
    buffer.extend_from_slice(&1u32.to_le_bytes()); // fWide: Unicode
    for path in paths {
        for unit in path.to_string_lossy().encode_utf16() {
            buffer.extend_from_slice(&unit.to_le_bytes());
        }
        buffer.extend_from_slice(&0u16.to_le_bytes());
    }
    buffer.extend_from_slice(&0u16.to_le_bytes()); // final terminator
    buffer
}

/// Rebuilds a full "HTML Format" document around a stored fragment, with the
/// fixed-width offset header Windows consumers expect.
fn build_html_format(fragment: &str) -> Vec<u8> {
    let header_template = format!(
        "Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n",
        0, 0, 0, 0
    );
    let header_len = header_template.len();
    let prefix = "<html><body>\r\n<!--StartFragment-->";
    let suffix = "<!--EndFragment-->\r\n</body>\r\n</html>";
    let start_html = header_len;
    let start_fragment = start_html + prefix.len();
    let end_fragment = start_fragment + fragment.len();
    let end_html = end_fragment + suffix.len();
    let mut out = String::with_capacity(end_html);
    out.push_str(&format!(
        "Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n",
        start_html, end_html, start_fragment, end_fragment
    ));
    out.push_str(prefix);
    out.push_str(fragment);
    out.push_str(suffix);
    out.into_bytes()
}

/// Allocates a moveable HGLOBAL and hands it to the clipboard. On success the
/// system owns the memory; on failure it is freed here.
fn set_data(format: u32, data: &[u8]) -> Option<HANDLE> {
    unsafe {
        let handle = GlobalAlloc(GMEM_MOVEABLE, data.len().max(1));
        if handle.is_null() {
            return None;
        }
        let locked = GlobalLock(handle) as *mut u8;
        if locked.is_null() {
            GlobalFree(handle as _);
            return None;
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), locked, data.len());
        GlobalUnlock(handle);
        let placed = SetClipboardData(format, handle as HANDLE);
        if placed.is_null() {
            GlobalFree(handle as _);
            return None;
        }
        Some(placed)
    }
}

// ---------------------------------------------------------------------------
// Image conversion (PNG <-> CF_DIB)

/// Converts stored PNG bytes into a 32bpp BI_RGB CF_DIB payload.
pub fn png_to_dib(png: &[u8]) -> Result<Vec<u8>, String> {
    let failure = "图片无法转换为剪贴板格式。 / The image could not be converted to a clipboard format.".to_string();
    let mut decoder = png::Decoder::new(Cursor::new(png));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|_| failure.clone())?;
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buffer).map_err(|_| failure.clone())?;
    let (width, height) = (frame.width as usize, frame.height as usize);
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 || (width as u64) * (height as u64) > 80_000_000 {
        return Err("图片尺寸过大或无法读取。 / The image is too large or unreadable.".into());
    }
    let rgba = match frame.color_type {
        png::ColorType::Rgba => buffer[..(width * height * 4)].to_vec(),
        png::ColorType::Rgb => {
            let mut rgba = Vec::with_capacity(width * height * 4);
            for pixel in buffer[..(width * height * 3)].chunks_exact(3) {
                rgba.extend_from_slice(pixel);
                rgba.push(255);
            }
            rgba
        }
        png::ColorType::Grayscale => {
            let mut rgba = Vec::with_capacity(width * height * 4);
            for pixel in &buffer[..width * height] {
                rgba.extend_from_slice(&[*pixel, *pixel, *pixel, 255]);
            }
            rgba
        }
        png::ColorType::GrayscaleAlpha => {
            let mut rgba = Vec::with_capacity(width * height * 4);
            for pixel in buffer[..(width * height * 2)].chunks_exact(2) {
                rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
            rgba
        }
        _ => return Err(failure),
    };
    // BITMAPINFOHEADER + bottom-up BGRA rows.
    let mut dib = Vec::with_capacity(40 + width * height * 4);
    dib.extend_from_slice(&40u32.to_le_bytes()); // biSize
    dib.extend_from_slice(&(width as i32).to_le_bytes()); // biWidth
    dib.extend_from_slice(&(height as i32).to_le_bytes()); // biHeight (bottom-up)
    dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    dib.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    dib.extend_from_slice(&((width * height * 4) as u32).to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&0u32.to_le_bytes()); // biXPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes()); // biYPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
    for row in (0..height).rev() {
        for column in 0..width {
            let index = row * width * 4 + column * 4;
            dib.push(rgba[index + 2]); // B
            dib.push(rgba[index + 1]); // G
            dib.push(rgba[index]); // R
            dib.push(rgba[index + 3]); // A
        }
    }
    Ok(dib)
}

/// Converts a clipboard CF_DIB payload into PNG bytes. Supports uncompressed
/// 16/24/32bpp BI_RGB and BI_BITFIELDS data (the formats clipboard writers
/// actually emit).
pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>, String> {
    let failure = "图片格式无法读取。 / The image format could not be read.".to_string();
    if dib.len() < 40 {
        return Err(failure);
    }
    let read_u32 = |offset: usize| u32::from_le_bytes([dib[offset], dib[offset + 1], dib[offset + 2], dib[offset + 3]]);
    let read_i32 = |offset: usize| i32::from_le_bytes([dib[offset], dib[offset + 1], dib[offset + 2], dib[offset + 3]]);
    let header_size = read_u32(0) as usize;
    let width = read_i32(4);
    let raw_height = read_i32(8);
    let bpp = u16::from_le_bytes([dib[14], dib[15]]) as usize;
    let compression = read_u32(16);
    if width <= 0 || raw_height == 0 || width > 16_384 || raw_height.unsigned_abs() > 16_384 {
        return Err("图片尺寸过大或无法读取。 / The image is too large or unreadable.".into());
    }
    if compression != 0 && compression != 3 {
        return Err(failure); // RLE/JPEG/PNG-in-DIB unsupported
    }
    if header_size < 40 || bpp < 16 {
        return Err(failure);
    }
    let height = raw_height.unsigned_abs() as usize;
    let top_down = raw_height < 0;
    let mut masks = [0u32, 0x0000F800, 0x000007E0, 0x0000001F]; // defaults: 565
    let mut pixel_offset = header_size;
    if compression == 3 {
        if dib.len() < header_size + 12 {
            return Err(failure);
        }
        masks[1] = read_u32(header_size);
        masks[2] = read_u32(header_size + 4);
        masks[3] = read_u32(header_size + 8);
        pixel_offset = header_size + 12;
    }
    let stride = ((width as usize * bpp + 31) / 32) * 4;
    if dib.len() < pixel_offset + stride * height {
        // Some writers omit biSizeImage accounting; require full data anyway.
        return Err(failure);
    }
    let extract = |pixel: u32, mask: u32| -> u8 {
        if mask == 0 {
            return 0;
        }
        let shift = mask.trailing_zeros();
        let maximum = mask >> shift;
        let value = ((pixel & mask) >> shift) as f64 / maximum.max(1) as f64;
        (value * 255.0).round() as u8
    };
    let mut rgba = vec![0u8; width as usize * height * 4];
    for row in 0..height {
        let source_row = if top_down { row } else { height - 1 - row };
        let row_start = pixel_offset + source_row * stride;
        for column in 0..width as usize {
            let (r, g, b, a) = match bpp {
                32 => {
                    let index = row_start + column * 4;
                    let pixel = u32::from_le_bytes([dib[index], dib[index + 1], dib[index + 2], dib[index + 3]]);
                    if compression == 3 {
                        (extract(pixel, masks[1]), extract(pixel, masks[2]), extract(pixel, masks[3]), 255)
                    } else {
                        // BI_RGB 32bpp is BGRA; writers that emit zero alpha are
                        // treated as opaque so pasted images stay visible.
                        let alpha = dib[index + 3];
                        (dib[index + 2], dib[index + 1], dib[index], if alpha == 0 { 255 } else { alpha })
                    }
                }
                24 => {
                    let index = row_start + column * 3;
                    (dib[index + 2], dib[index + 1], dib[index], 255u8)
                }
                16 => {
                    let index = row_start + column * 2;
                    let pixel = u16::from_le_bytes([dib[index], dib[index + 1]]) as u32;
                    // 555 masks when BI_RGB; the defaults already say 565, so
                    // apply the standard 555 fallback for compression == 0.
                    let (rm, gm, bm) = if compression == 0 { (0x7C00u32, 0x03E0u32, 0x001Fu32) } else { (masks[1], masks[2], masks[3]) };
                    (extract(pixel, rm), extract(pixel, gm), extract(pixel, bm), 255u8)
                }
                _ => return Err(failure),
            };
            let target = (row * width as usize + column) * 4;
            rgba[target] = r;
            rgba[target + 1] = g;
            rgba[target + 2] = b;
            rgba[target + 3] = a;
        }
    }
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, width as u32, height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|_| failure.clone())?;
        writer.write_image_data(&rgba).map_err(|_| failure.clone())?;
    }
    Ok(encoded)
}

// ---------------------------------------------------------------------------
// Foreground window helpers (conflict-policy inputs) and injected strokes

pub fn foreground_window() -> isize {
    unsafe { GetForegroundWindow() as isize }
}

/// Focused window of the foreground GUI thread (AX focused element analogue).
pub fn foreground_focus_hwnd() -> isize {
    let hwnd = foreground_window();
    if hwnd == 0 {
        return 0;
    }
    let thread = unsafe { GetWindowThreadProcessId(hwnd as _, std::ptr::null_mut()) };
    let mut info: GUITHREADINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
    if unsafe { GetGUIThreadInfo(thread, &mut info) } != 0 && info.hwndFocus != std::ptr::null_mut() {
        info.hwndFocus as isize
    } else {
        hwnd
    }
}

/// Injects Ctrl+C or Ctrl+V into the foreground app. The events carry the
/// LLKHF_INJECTED flag plus the shell marker, so hooks skip them.
pub fn send_copy_paste(paste: bool) -> bool {
    let key = if paste { 0x56u16 } else { 0x43u16 }; // V or C
    let stroke = |vk: u16, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0_uninit(
            KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: OWN_EVENT_MARKER,
            },
        ),
    };
    let inputs = [stroke(0x11, false), stroke(key, false), stroke(key, true), stroke(0x11, true)];
    let sent = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    sent == inputs.len() as u32
}

// windows-sys unions cannot be constructed directly in stable position
// syntax; build through a zeroed wrapper.
#[allow(non_snake_case)]
fn INPUT_0_uninit(kbd: KEYBDINPUT) -> windows_sys::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
    let mut anonymous: windows_sys::Win32::UI::Input::KeyboardAndMouse::INPUT_0 = unsafe { std::mem::zeroed() };
    anonymous.ki = kbd;
    anonymous
}

/// True while any modifier or the given VKs are physically held.
pub fn keys_still_held(vks: &[u16]) -> bool {
    let pressed = |vk: u16| unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 };
    [0x10, 0xA0, 0xA1, 0x11, 0xA2, 0xA3, 0x12, 0xA4, 0xA5, 0x5B, 0x5C]
        .iter()
        .any(|vk| pressed(*vk))
        || vks.iter().any(|vk| pressed(*vk))
}

pub fn sequence_number() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}
