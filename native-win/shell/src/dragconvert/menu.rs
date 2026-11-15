// Radial Drag Convert wheel (Windows port of DragConvertPanel): a topmost,
// never-activating popup drawn with plain GDI. The window is an OLE drop
// target (RegisterDragDrop): DragEnter peeks at CF_HDROP to rebuild the
// sector set from the real file kinds, and a Drop over a sector commits the
// conversion. Center and sector gaps are neutral no-drop zones; Esc, trigger
// release, mouse release and the 10 s timeout all close the menu.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, PolyPolygon,
    RoundRect, ScreenToClient, SelectObject, SetBkMode, SetTextColor, SRCCOPY, TRANSPARENT, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, PS_SOLID,
};
use windows_sys::Win32::System::Com::{FORMATETC, STGMEDIUM, DVASPECT_CONTENT, TYMED_HGLOBAL};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Ole::{
    OleInitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop, CF_HDROP, DROPEFFECT_COPY,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::Shell::DragQueryFileW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW, KillTimer, RegisterClassW,
    SetTimer, SetWindowPos, ShowWindow, TranslateMessage, DestroyWindow, CS_HREDRAW, CS_VREDRAW, HWND_TOPMOST,
    SWP_NOACTIVATE, SW_HIDE, SW_SHOWNOACTIVATE, WM_ERASEBKGND, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows_sys::core::GUID;

use super::engine::{self, Availability, FileKind};
use super::trigger::SharedDragRuntime;
use super::Op;

/// Wheel geometry (DragWheelGeometry parity): 252×252, annulus 34–112 px,
/// 0.045 rad gutters so ambiguous releases land on nothing.
pub const SIDE: i32 = 252;
pub const INNER_RADIUS: f64 = 34.0;
pub const OUTER_RADIUS: f64 = 112.0;
pub const LABEL_RADIUS: f64 = 77.0;
pub const GAP: f64 = 0.045;
pub const MAXIMUM_SECTORS: usize = 9;
pub const CENTER: (f64, f64) = (126.0, 126.0);
const TIMEOUT_SECS: u64 = 10;
const ACTIVATION_DISTANCE: f64 = 7.0;

/// Thread messages the hook posts to the wheel window's thread (WM_APP + n).
pub const WM_DRAG_SHOW: u32 = 0x8000 + 1;
pub const WM_DRAG_HIDE: u32 = 0x8000 + 2;
pub const WM_DRAG_SHUTDOWN: u32 = 0x8000 + 3;

/// Polar angle of a client point: 0 at twelve o'clock, growing clockwise.
pub fn sector_angle(x: f64, y: f64) -> f64 {
    let (cx, cy) = CENTER;
    let (dx, dy) = (x - cx, y - cy);
    // atan2(sin, cos) with the SVG's clockwise-from-north convention.
    let mut angle = dx.atan2(-dy);
    if angle < 0.0 {
        angle += std::f64::consts::TAU;
    }
    angle % std::f64::consts::TAU
}

/// Sector hit test: inside the annulus and inside sector i's angular span
/// (gutters excluded). Center and gaps return None = cancel.
pub fn sector_hit(x: f64, y: f64, count: usize) -> Option<usize> {
    if !(3..=MAXIMUM_SECTORS).contains(&count) {
        return None;
    }
    let (cx, cy) = CENTER;
    let distance = (x - cx).hypot(y - cy);
    if !(INNER_RADIUS..=OUTER_RADIUS).contains(&distance) {
        return None;
    }
    let angle = sector_angle(x, y);
    let span = std::f64::consts::TAU / count as f64;
    let half = span / 2.0;
    let sector = (((angle + half) / span).floor() as isize).rem_euclid(count as isize) as usize;
    let mid = sector as f64 * span;
    if angle >= mid - half + GAP && angle <= mid + half - GAP {
        Some(sector)
    } else {
        None
    }
}

pub fn label_point(index: usize, count: usize) -> (f64, f64) {
    let angle = index as f64 * std::f64::consts::TAU / count as f64;
    let (cx, cy) = CENTER;
    (cx + angle.sin() * LABEL_RADIUS, cy - angle.cos() * LABEL_RADIUS)
}

/// Clamped window top-left for the cursor position (8 px screen margin).
pub fn frame_at(cursor: (i32, i32), work: RECT) -> (i32, i32) {
    let x = (cursor.0 - SIDE / 2).clamp(work.left + 8, (work.right - SIDE - 8).max(work.left + 8));
    let y = (cursor.1 - SIDE / 2).clamp(work.top + 8, (work.bottom - SIDE - 8).max(work.top + 8));
    (x, y)
}

/// Wheel action titles match the settings preview (DragConvertLab.tsx).
pub fn action_title(action: &str, english: bool) -> &'static str {
    match action {
        "jpg" | "jpeg" => "JPG",
        "png" => "PNG",
        "webp" => "WebP",
        "heic" => "HEIC",
        "tiff" => "TIFF",
        "small" => if english { "Small JPG" } else { "小图 JPG" },
        "imagesPDF" => if english { "To PDF" } else { "合成 PDF" },
        "zip" => if english { "Pack ZIP" } else { "打包 ZIP" },
        "zipEach" => if english { "Each ZIP" } else { "分别 ZIP" },
        "zipFast" => if english { "Store ZIP" } else { "快速 ZIP" },
        "tarGz" => "TAR.GZ",
        "extract" => if english { "Extract" } else { "解压" },
        "mergePDF" => if english { "Merge PDFs" } else { "合并 PDF" },
        "pdfPNG" => "PNG",
        "pdfJPG" => "JPG",
        "splitPDF" => if english { "Split" } else { "拆分页面" },
        "mp4" => "MP4",
        "mov" => "MOV",
        "video720" => "720p",
        "video1080" => "1080p",
        "m4a" => "M4A",
        "wav" => "WAV",
        "aiff" => "AIFF",
        _ => "FILE",
    }
}

// ---------------------------------------------------------------------------
// Wheel session state (only touched from the menu thread)

struct Session {
    visible: bool,
    committed: bool,
    files: Vec<PathBuf>,
    actions: Vec<&'static str>,
    unavailable: Vec<bool>,
    hover: Option<usize>,
    activation: (f64, f64),
    moved: bool,
    shown_at: Instant,
}

impl Session {
    fn new() -> Session {
        Session {
            visible: false,
            committed: false,
            files: Vec::new(),
            actions: Vec::new(),
            unavailable: Vec::new(),
            hover: None,
            activation: CENTER,
            moved: false,
            shown_at: Instant::now(),
        }
    }
}

static SESSION: OnceLock<Mutex<Session>> = OnceLock::new();

fn session() -> &'static Mutex<Session> {
    SESSION.get_or_init(|| Mutex::new(Session::new()))
}

fn lock_session() -> MutexGuard<'static, Session> {
    // A poisoned lock must never wedge the wheel; the session is rebuildable.
    session().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct MenuEnvironment {
    hwnd: HWND,
    english: &'static AtomicBool,
    runtime: SharedDragRuntime,
    events: Sender<Op>,
}

thread_local! {
    static ENVIRONMENT: std::cell::RefCell<Option<MenuEnvironment>> = const { std::cell::RefCell::new(None) };
}

fn with_environment<T>(callback: impl FnOnce(&MenuEnvironment) -> T) -> Option<T> {
    ENVIRONMENT.with(|cell| {
        let Ok(borrow) = cell.try_borrow() else { return None };
        borrow.as_ref().map(callback)
    })
}

fn english() -> bool {
    with_environment(|env| env.english.load(Ordering::Relaxed)).unwrap_or(false)
}

/// Spawns the dedicated wheel-menu thread: its own Win32 window, its own OLE
/// STA apartment (required for RegisterDragDrop) and its own message pump.
pub fn spawn_menu(
    english: &'static AtomicBool,
    availability: Availability,
    runtime: SharedDragRuntime,
    events: Sender<Op>,
    thread_id_slot: std::sync::Arc<std::sync::atomic::AtomicU32>,
) {
    std::thread::Builder::new()
        .name("dragconvert-menu".into())
        .spawn(move || unsafe {
            // STA apartment: required before RegisterDragDrop on this thread.
            OleInitialize(std::ptr::null());
            let instance = GetModuleHandleW(std::ptr::null());
            let class_name: Vec<u16> = "NuPhyLabDragMenu\0".encode_utf16().collect();
            let class = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class_name.as_ptr(),
            };
            RegisterClassW(&class);
            let title: Vec<u16> = "NuPhyLab Drag Convert\0".encode_utf16().collect();
            let hwnd = CreateWindowExW(
                (WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) as u32,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                -SIDE * 2,
                -SIDE * 2,
                SIDE,
                SIDE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return;
            }
            register_drop_target(hwnd, availability);
            ENVIRONMENT.with(|cell| {
                *cell.borrow_mut() = Some(MenuEnvironment { hwnd, english, runtime, events });
            });
            thread_id_slot.store(windows_sys::Win32::System::Threading::GetCurrentThreadId(), Ordering::SeqCst);
            let mut message: windows_sys::Win32::UI::WindowsAndMessaging::MSG = std::mem::zeroed();
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                match message.message {
                    WM_DRAG_SHOW => show_wheel(hwnd),
                    WM_DRAG_HIDE => hide_wheel(hwnd),
                    WM_DRAG_SHUTDOWN => break,
                    _ => {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
            }
            RevokeDragDrop(hwnd);
            DestroyWindow(hwnd);
        })
        .expect("dragconvert menu thread");
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut paint);
            paint_wheel(hdc);
            EndPaint(hwnd, &paint);
            0
        }
        WM_ERASEBKGND => 1,
        WM_TIMER => {
            timer_tick(hwnd);
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe fn show_wheel(hwnd: HWND) {
    let mut cursor = POINT { x: 0, y: 0 };
    if GetCursorPos(&mut cursor) == 0 {
        return;
    }
    let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(monitor, &mut info) == 0 {
        return;
    }
    let (left, top) = frame_at((cursor.x, cursor.y), info.rcWork);
    {
        let mut guard = lock_session();
        guard.files.clear();
        guard.actions.clear();
        guard.unavailable.clear();
        guard.hover = None;
        // Activation point in client coordinates; a small intentional move is
        // required before a sector can select (screen-edge clamping parity).
        guard.activation = ((cursor.x - left) as f64, (cursor.y - top) as f64);
        guard.moved = false;
        guard.committed = false;
        guard.visible = true;
        guard.shown_at = Instant::now();
    }
    SetWindowPos(hwnd, HWND_TOPMOST, left, top, SIDE, SIDE, SWP_NOACTIVATE);
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    SetTimer(hwnd, 1, 80, None);
    InvalidateRect(hwnd, std::ptr::null(), 1);
}

unsafe fn hide_wheel(hwnd: HWND) {
    KillTimer(hwnd, 1);
    ShowWindow(hwnd, SW_HIDE);
    let mut guard = lock_session();
    guard.visible = false;
    guard.hover = None;
}

unsafe fn timer_tick(hwnd: HWND) {
    let (visible, committed, shown_at) = {
        let guard = lock_session();
        (guard.visible, guard.committed, guard.shown_at)
    };
    if !visible {
        return;
    }
    if committed {
        hide_wheel(hwnd);
        return;
    }
    // Trigger released, drag ended, or the menu sat idle for 10 seconds.
    let trigger_held = with_environment(|env| env.runtime.lock().ok().and_then(|runtime| runtime.vk).map(super::trigger::vk_held))
        .flatten()
        .unwrap_or(true);
    let mouse_held = (GetAsyncKeyState(0x01) as u16 & 0x8000 != 0) || (GetAsyncKeyState(0x02) as u16 & 0x8000 != 0);
    if !trigger_held || !mouse_held || shown_at.elapsed().as_secs() >= TIMEOUT_SECS {
        hide_wheel(hwnd);
    }
}

// ---------------------------------------------------------------------------
// Painting

fn rgb(value: u32) -> u32 {
    // 0xRRGGBB input, COLORREF 0x00BBGGRR output.
    let r = (value >> 16) & 0xFF;
    let g = (value >> 8) & 0xFF;
    let b = value & 0xFF;
    r | (g << 8) | (b << 16)
}

const COLOR_BACKGROUND: u32 = 0x141414;
const COLOR_IDLE: u32 = 0x292929;
const COLOR_IDLE_DIM: u32 = 0x1F1F1F;
const COLOR_SELECTED: u32 = 0xE6E6E6;
const COLOR_BORDER: u32 = 0x636363;
const COLOR_IDLE_BORDER: u32 = 0x5A5A5A;
const COLOR_IDLE_BORDER_DIM: u32 = 0x3A3A3A;
const COLOR_SELECTED_BORDER: u32 = 0xFFFFFF;
const COLOR_TEXT: u32 = 0xEDEDED;
const COLOR_TEXT_DIM: u32 = 0x7A7A7A;
const COLOR_SELECTED_TEXT: u32 = 0x111111;
const COLOR_DETAIL: u32 = 0xA0A0A0;

fn sector_polygon(index: usize, count: usize) -> Vec<POINT> {
    let span = std::f64::consts::TAU / count as f64;
    let half = span / 2.0;
    let mid = index as f64 * span;
    let start = mid - half + GAP;
    let end = mid + half - GAP;
    let steps = 36;
    let point = |angle: f64, radius: f64| -> POINT {
        let x = CENTER.0 + angle.sin() * radius;
        let y = CENTER.1 - angle.cos() * radius;
        POINT { x: x.round() as i32, y: y.round() as i32 }
    };
    let mut polygon = Vec::with_capacity((steps + 1) * 2);
    for step in 0..=steps {
        polygon.push(point(start + (end - start) * step as f64 / steps as f64, OUTER_RADIUS));
    }
    for step in (0..=steps).rev() {
        polygon.push(point(start + (end - start) * step as f64 / steps as f64, INNER_RADIUS));
    }
    polygon
}

unsafe fn draw_centered(hdc: windows_sys::Win32::Graphics::Gdi::HDC, text: &str, x: i32, y: i32) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let mut rect = RECT { left: x - 60, top: y - 10, right: x + 60, bottom: y + 10 };
    DrawTextW(hdc, wide.as_ptr(), (wide.len() as i32) - 1, &mut rect, 0x25); // DT_CENTER|DT_VCENTER|DT_SINGLELINE
}

unsafe fn create_wheel_font(size: i32) -> HGDIOBJ {
    let face: Vec<u16> = "Microsoft YaHei UI\0".encode_utf16().collect();
    let font = CreateFontW(-size, 0, 0, 0, 500, 0, 0, 0, 0, 0, 0, 0, 0, face.as_ptr());
    if font.is_null() {
        std::ptr::null_mut()
    } else {
        font
    }
}

unsafe fn paint_wheel(hdc: windows_sys::Win32::Graphics::Gdi::HDC) {
    let (actions, unavailable, hover, files) = {
        let guard = lock_session();
        (guard.actions.clone(), guard.unavailable.clone(), guard.hover, guard.files.len())
    };
    let english = english();
    let memory = CreateCompatibleDC(hdc);
    let bitmap = CreateCompatibleBitmap(hdc, SIDE, SIDE);
    let old_bitmap = SelectObject(memory, bitmap);
    let background = CreateSolidBrush(rgb(COLOR_BACKGROUND));
    let border_pen = CreatePen(PS_SOLID, 1, rgb(COLOR_BORDER));
    let old_brush = SelectObject(memory, background);
    let old_pen = SelectObject(memory, border_pen);
    RoundRect(memory, 0, 0, SIDE + 1, SIDE + 1, 36, 36);

    let count = actions.len();
    if (3..=MAXIMUM_SECTORS).contains(&count) {
        for (index, _action) in actions.iter().enumerate() {
            let selected = hover == Some(index);
            let dimmed = unavailable.get(index).copied().unwrap_or(false);
            let fill = if selected {
                COLOR_SELECTED
            } else if dimmed {
                COLOR_IDLE_DIM
            } else {
                COLOR_IDLE
            };
            let stroke = if selected {
                COLOR_SELECTED_BORDER
            } else if dimmed {
                COLOR_IDLE_BORDER_DIM
            } else {
                COLOR_IDLE_BORDER
            };
            let brush = CreateSolidBrush(rgb(fill));
            let pen = CreatePen(PS_SOLID, 1, rgb(stroke));
            SelectObject(memory, brush);
            SelectObject(memory, pen);
            let polygon = sector_polygon(index, count);
            let counts = [polygon.len() as i32];
            PolyPolygon(memory, polygon.as_ptr(), counts.as_ptr(), 1);
            DeleteObject(brush);
            DeleteObject(pen);
        }
        let font_size = if count >= 7 { 11 } else if count >= 5 { 12 } else { 13 };
        let font = create_wheel_font(font_size);
        let old_font = SelectObject(memory, font);
        SetBkMode(memory, TRANSPARENT as i32);
        for (index, action) in actions.iter().enumerate() {
            let selected = hover == Some(index);
            let dimmed = unavailable.get(index).copied().unwrap_or(false);
            SetTextColor(memory, rgb(if selected { COLOR_SELECTED_TEXT } else if dimmed { COLOR_TEXT_DIM } else { COLOR_TEXT }));
            let title = action_title(action, english);
            let (point_x, point_y) = label_point(index, count);
            let (x, y) = (point_x.round() as i32, point_y.round() as i32);
            let split_at = if english && count >= 9 { title.find(' ') } else { None };
            match split_at {
                Some(split) => {
                    draw_centered(memory, &title[..split], x, y - 7);
                    draw_centered(memory, &title[split + 1..], x, y + 7);
                }
                None => draw_centered(memory, title, x, y),
            }
        }
        SelectObject(memory, old_font);
        DeleteObject(font);
    }

    let count_font = create_wheel_font(14);
    let detail_font = create_wheel_font(10);
    let old_font = SelectObject(memory, count_font);
    SetBkMode(memory, TRANSPARENT as i32);
    SetTextColor(memory, rgb(COLOR_TEXT));
    let center_count = if files > 0 { files.to_string() } else { "…".to_string() };
    draw_centered(memory, &center_count, CENTER.0 as i32, CENTER.1 as i32 - 7);
    SelectObject(memory, detail_font);
    SetTextColor(memory, rgb(COLOR_DETAIL));
    draw_centered(memory, if english { "FILES" } else { "个文件" }, CENTER.0 as i32, CENTER.1 as i32 + 13);
    SelectObject(memory, old_font);
    DeleteObject(count_font);
    DeleteObject(detail_font);

    BitBlt(hdc, 0, 0, SIDE, SIDE, memory, 0, 0, SRCCOPY);
    SelectObject(memory, old_brush);
    SelectObject(memory, old_pen);
    SelectObject(memory, old_bitmap);
    DeleteObject(background);
    DeleteObject(border_pen);
    DeleteObject(bitmap);
    DeleteDC(memory);
}

// ---------------------------------------------------------------------------
// OLE drop target (IDropTarget + partial IDataObject vtables; windows-sys
// ships flat bindings without the interfaces, so the vtables are hand-declared)

#[repr(C)]
struct IDataObjectVtbl {
    query_interface: unsafe extern "system" fn(*mut IDataObject, *const GUID, *mut *mut core::ffi::c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut IDataObject) -> u32,
    release: unsafe extern "system" fn(*mut IDataObject) -> u32,
    get_data: unsafe extern "system" fn(*mut IDataObject, *mut FORMATETC, *mut STGMEDIUM) -> i32,
}

#[repr(C)]
struct IDataObject {
    vtbl: *const IDataObjectVtbl,
}

#[repr(C)]
struct DropTargetVtbl {
    query_interface: unsafe extern "system" fn(*mut DropTarget, *const GUID, *mut *mut core::ffi::c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut DropTarget) -> u32,
    release: unsafe extern "system" fn(*mut DropTarget) -> u32,
    drag_enter: unsafe extern "system" fn(*mut DropTarget, *mut IDataObject, u32, POINT, *mut u32) -> i32,
    drag_over: unsafe extern "system" fn(*mut DropTarget, u32, POINT, *mut u32) -> i32,
    drag_leave: unsafe extern "system" fn(*mut DropTarget) -> i32,
    drop: unsafe extern "system" fn(*mut DropTarget, *mut IDataObject, u32, POINT, *mut u32) -> i32,
}

#[repr(C)]
struct DropTarget {
    vtbl: *const DropTargetVtbl,
}

unsafe extern "system" fn drop_target_query_interface(_this: *mut DropTarget, _iid: *const GUID, _out: *mut *mut core::ffi::c_void) -> i32 {
    0x8000_4002u32 as i32 // E_NOINTERFACE: the shell only needs IDropTarget itself
}

unsafe extern "system" fn drop_target_add_ref(_this: *mut DropTarget) -> u32 {
    2 // leaked singleton; never freed
}

unsafe extern "system" fn drop_target_release(_this: *mut DropTarget) -> u32 {
    1 // leaked singleton; never freed
}

/// Reads the CF_HDROP payload without consuming the drag. Returns None for
/// non-file drags (text, internal shells, …).
unsafe fn read_file_list(data_object: *mut IDataObject) -> Option<Vec<PathBuf>> {
    let mut format = FORMATETC {
        cfFormat: CF_HDROP,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT,
        lindex: -1,
        tymed: TYMED_HGLOBAL as u32,
    };
    let mut medium = STGMEDIUM { tymed: TYMED_HGLOBAL as u32, u: std::mem::zeroed(), pUnkForRelease: std::ptr::null_mut() };
    let get_data = (*(*data_object).vtbl).get_data;
    if get_data(data_object, &mut format, &mut medium) < 0 {
        return None;
    }
    let handle = medium.u.hGlobal;
    let files = (|| {
        let locked = GlobalLock(handle);
        if locked.is_null() {
            return None;
        }
        let count = DragQueryFileW(locked, 0xFFFF_FFFF, std::ptr::null_mut(), 0);
        let mut files = Vec::with_capacity(count as usize);
        for index in 0..count {
            let length = DragQueryFileW(locked, index, std::ptr::null_mut(), 0);
            if length == 0 {
                continue;
            }
            let mut buffer = vec![0u16; length as usize + 1];
            let written = DragQueryFileW(locked, index, buffer.as_mut_ptr(), buffer.len() as u32);
            if written > 0 {
                buffer.truncate(written as usize);
                files.push(PathBuf::from(String::from_utf16_lossy(&buffer)));
            }
        }
        GlobalUnlock(handle);
        (!files.is_empty()).then_some(files)
    })();
    ReleaseStgMedium(&mut medium);
    files
}

fn configure_from_files(files: &[PathBuf], availability: &Availability) -> (Vec<&'static str>, Vec<bool>) {
    let kinds: Vec<FileKind> = files.iter().map(|path| engine::file_kind(path)).collect();
    let actions = engine::actions_for(&kinds, files.len());
    let unavailable = actions.iter().map(|action| !engine::action_available(action, availability)).collect();
    (actions, unavailable)
}

/// Current hover under a screen point: requires a small intentional move away
/// from the activation point, converts to client coordinates, then hit-tests.
fn hover_at(point: POINT) -> Option<usize> {
    let mut client = POINT { x: point.x, y: point.y };
    with_environment(|env| unsafe {
        ScreenToClient(env.hwnd, &mut client);
    });
    let (x, y) = (client.x as f64, client.y as f64);
    let mut guard = lock_session();
    let count = guard.actions.len();
    if !(3..=MAXIMUM_SECTORS).contains(&count) {
        return None;
    }
    if ((x - guard.activation.0).hypot(y - guard.activation.1)) >= ACTIVATION_DISTANCE {
        guard.moved = true;
    }
    if !guard.moved {
        return None;
    }
    sector_hit(x, y, count)
}

static DROP_AVAILABILITY: OnceLock<Availability> = OnceLock::new();

unsafe extern "system" fn drop_target_drag_enter(
    _this: *mut DropTarget,
    data_object: *mut IDataObject,
    _key_state: u32,
    point: POINT,
    effect: *mut u32,
) -> i32 {
    let Some(files) = read_file_list(data_object) else {
        *effect = 0;
        return 0;
    };
    let (actions, unavailable) = match DROP_AVAILABILITY.get().copied() {
        Some(availability) => configure_from_files(&files, &availability),
        None => (Vec::new(), Vec::new()),
    };
    {
        let mut guard = lock_session();
        guard.files = files;
        guard.actions = actions;
        guard.unavailable = unavailable;
        guard.hover = None;
    }
    let hover = hover_at(point);
    lock_session().hover = hover;
    *effect = if hover.is_some() { DROPEFFECT_COPY } else { 0 };
    with_environment(|env| InvalidateRect(env.hwnd, std::ptr::null(), 1));
    0
}

unsafe extern "system" fn drop_target_drag_over(_this: *mut DropTarget, _key_state: u32, point: POINT, effect: *mut u32) -> i32 {
    let hover = hover_at(point);
    let changed = {
        let mut guard = lock_session();
        let changed = guard.hover != hover;
        guard.hover = hover;
        changed
    };
    if changed {
        with_environment(|env| InvalidateRect(env.hwnd, std::ptr::null(), 1));
    }
    *effect = if hover.is_some() { DROPEFFECT_COPY } else { 0 };
    0
}

unsafe extern "system" fn drop_target_drag_leave(_this: *mut DropTarget) -> i32 {
    let changed = lock_session().hover.take().is_some();
    if changed {
        with_environment(|env| InvalidateRect(env.hwnd, std::ptr::null(), 1));
    }
    0
}

unsafe extern "system" fn drop_target_drop(
    _this: *mut DropTarget,
    data_object: *mut IDataObject,
    _key_state: u32,
    point: POINT,
    effect: *mut u32,
) -> i32 {
    let already_committed = lock_session().committed;
    if already_committed {
        *effect = 0;
        with_environment(|env| hide_wheel(env.hwnd));
        return 0;
    }
    // Center or gutter: a neutral release is a cancel, never a conversion.
    let Some(index) = hover_at(point) else {
        *effect = 0;
        with_environment(|env| hide_wheel(env.hwnd));
        return 0;
    };
    let action = lock_session().actions.get(index).copied();
    let Some(action) = action else {
        *effect = 0;
        with_environment(|env| hide_wheel(env.hwnd));
        return 0;
    };
    // Re-read at drop time (the hover snapshot may be stale), falling back to
    // the DragEnter list if the source no longer serves the data.
    let files = read_file_list(data_object);
    let files = files.or_else(|| {
        let guard = lock_session();
        (!guard.files.is_empty()).then_some(guard.files.clone())
    });
    let Some(files) = files else {
        *effect = 0;
        with_environment(|env| hide_wheel(env.hwnd));
        return 0;
    };
    lock_session().committed = true;
    with_environment(|env| {
        let _ = env.events.send(Op::Drop { action: action.to_string(), files });
        hide_wheel(env.hwnd);
    });
    *effect = DROPEFFECT_COPY;
    0
}

static DROP_VTABLE: DropTargetVtbl = DropTargetVtbl {
    query_interface: drop_target_query_interface,
    add_ref: drop_target_add_ref,
    release: drop_target_release,
    drag_enter: drop_target_drag_enter,
    drag_over: drop_target_drag_over,
    drag_leave: drop_target_drag_leave,
    drop: drop_target_drop,
};

unsafe fn register_drop_target(hwnd: HWND, availability: Availability) {
    let _ = DROP_AVAILABILITY.set(availability);
    let target = Box::leak(Box::new(DropTarget { vtbl: &DROP_VTABLE }));
    RegisterDragDrop(hwnd, target as *mut DropTarget as *mut core::ffi::c_void);
}

#[cfg(test)]
mod tests;
