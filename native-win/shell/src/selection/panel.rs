// Answer floating window (Windows port of the selection answer panel): a
// topmost tool window near the captured region showing the region thumbnail,
// the streaming answer text, the current action (wheel / button switching),
// a copy button and a follow-up input. All display state lives on the main
// thread; the worker mutates it through UserEvent forwarding.

use std::sync::mpsc::Sender;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tao::dpi::PhysicalSize;
use tao::event_loop::EventLoop;
use tao::window::{Window, WindowBuilder};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, GetStockObject, InvalidateRect,
    ScreenToClient, SelectObject, SetBkMode, SetTextColor, DEFAULT_GUI_FONT, DT_CALCRECT, DT_END_ELLIPSIS, DT_SINGLELINE,
    DT_VCENTER, DT_WORDBREAK, HDC, HFONT, PAINTSTRUCT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_RETURN};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, GetClientRect, GetCursorPos, GetWindowTextW, ES_AUTOHSCROLL, WS_BORDER, WS_CHILD,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_TABSTOP, WS_VISIBLE,
};

use super::keys::action_label;
use super::Op;

/// COLORREF helper (windows-sys 0.59 has no RGB macro): 0x00BBGGRR.
fn rgb(r: u32, g: u32, b: u32) -> u32 {
    (r & 0xFF) | ((g & 0xFF) << 8) | ((b & 0xFF) << 16)
}

pub const PANEL_WIDTH: i32 = 384;
pub const PANEL_HEIGHT: i32 = 468;
const MARGIN: i32 = 14;
const HEADER_H: i32 = 26;
const THUMB_H: i32 = 100;
const BUTTON_W: i32 = 108;
const BUTTON_H: i32 = 28;
const EDIT_H: i32 = 30;

// Hit-test rects are recomputed from the same layout the painter uses.
fn copy_button_rect() -> RECT {
    RECT { left: MARGIN, top: PANEL_HEIGHT - 112, right: MARGIN + BUTTON_W, bottom: PANEL_HEIGHT - 112 + BUTTON_H }
}

fn send_button_rect() -> RECT {
    RECT { left: PANEL_WIDTH - MARGIN - 64, top: PANEL_HEIGHT - 76, right: PANEL_WIDTH - MARGIN, bottom: PANEL_HEIGHT - 76 + EDIT_H }
}

fn result_rect() -> RECT {
    RECT { left: MARGIN, top: MARGIN + HEADER_H + 8 + THUMB_H + 10, right: PANEL_WIDTH - MARGIN, bottom: PANEL_HEIGHT - 120 }
}

fn point_in(rect: &RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

thread_local! {
    /// The subclassed Edit control forwards Enter to the worker through this
    /// channel (installed on the thread that creates the panel).
    static FOLLOW_UP_SENDER: std::cell::RefCell<Option<Sender<Op>>> = const { std::cell::RefCell::new(None) };
}

const EDIT_SUBCLASS_ID: usize = 0x51EC;

/// Edit-control subclass: Enter sends the follow-up, everything else passes to
/// the default edit procedure.
unsafe extern "system" fn edit_subclass_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _ref: usize) -> LRESULT {
    if message == 0x0100 {
        // WM_KEYDOWN
        if (wparam as u32 & 0xFFFF) == VK_RETURN as u32 {
            FOLLOW_UP_SENDER.with(|cell| {
                if let Some(sender) = cell.borrow().as_ref() {
                    let text = read_edit_text(hwnd as isize);
                    if !text.is_empty() {
                        let _ = sender.send(Op::PanelFollowUp { text });
                        set_window_text(hwnd as isize, &[]);
                    }
                }
            });
            return 0;
        }
    }
    DefSubclassProc(hwnd, message, wparam, lparam)
}

fn read_edit_text(hwnd: isize) -> String {
    unsafe {
        let length = GetWindowTextW(hwnd as _, std::ptr::null_mut(), 0);
        if length <= 0 {
            return String::new();
        }
        let mut buffer = vec![0u16; length as usize + 1];
        let read = GetWindowTextW(hwnd as _, buffer.as_mut_ptr(), buffer.len() as i32);
        String::from_utf16_lossy(&buffer[..read.max(0) as usize])
    }
}

unsafe fn set_window_text(hwnd: isize, text: &[u16]) {
    windows_sys::Win32::UI::WindowsAndMessaging::SetWindowTextW(hwnd as _, text.as_ptr());
}

pub struct Panel {
    pub window: Window,
    hwnd: isize,
    edit_hwnd: isize,
    action: String,
    status: String,
    result: String,
    error: String,
    thumbnail: Option<(Vec<u8>, u32, u32)>, // RGBA pixels + dimensions
    generation: u64,
}

impl Panel {
    pub fn create(event_loop: &EventLoop<crate::UserEvent>, follow_ups: Sender<Op>) -> Panel {
        let window = WindowBuilder::new()
            .with_title("NuPhyLab Selection Answer")
            .with_visible(false)
            .with_decorations(false)
            .with_resizable(false)
            .with_always_on_top(true)
            .with_inner_size(PhysicalSize::new(PANEL_WIDTH as u32, PANEL_HEIGHT as u32))
            .build(event_loop)
            .expect("selection answer panel window");
        let hwnd = match window.window_handle().ok().map(|h| h.as_raw()) {
            Some(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
            _ => 0,
        };
        if hwnd != 0 {
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
                    hwnd as _,
                    windows_sys::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE,
                    (WS_EX_TOPMOST | WS_EX_TOOLWINDOW) as isize,
                );
            }
        }
        let edit_hwnd = unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let class: Vec<u16> = "EDIT\0".encode_utf16().collect();
            CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                (WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP) as u32 | ES_AUTOHSCROLL as u32,
                MARGIN,
                PANEL_HEIGHT - 76,
                PANEL_WIDTH - MARGIN * 2 - 72,
                EDIT_H,
                hwnd as _,
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            ) as isize
        };
        if edit_hwnd != 0 {
            FOLLOW_UP_SENDER.with(|cell| *cell.borrow_mut() = Some(follow_ups));
            unsafe {
                SetWindowSubclass(edit_hwnd as _, Some(edit_subclass_proc), EDIT_SUBCLASS_ID, 0);
            }
        }
        Panel {
            window,
            hwnd,
            edit_hwnd,
            action: "translate".into(),
            status: String::new(),
            result: String::new(),
            error: String::new(),
            thumbnail: None,
            generation: 0,
        }
    }

    /// Current cursor position in panel-client coordinates.
    pub fn client_cursor(&self) -> (i32, i32) {
        unsafe {
            let mut point = POINT { x: 0, y: 0 };
            if GetCursorPos(&mut point) == 0 {
                return (-1, -1);
            }
            if ScreenToClient(self.hwnd as _, &mut point) == 0 {
                return (-1, -1);
            }
            (point.x, point.y)
        }
    }

    /// Shows the panel at (x, y), clamped to the virtual desktop, without
    /// stealing focus from the application being questioned.
    pub fn show_at(&mut self, x: i32, y: i32) {
        self.reset_layout();
        let (vx, vy, vw, vh) = crate::win::virtual_screen();
        if vw > 0 && vh > 0 {
            let clamped_x = x.clamp(vx, vx + vw - PANEL_WIDTH);
            let clamped_y = y.clamp(vy, vy + vh - PANEL_HEIGHT);
            self.window.set_outer_position(tao::dpi::PhysicalPosition::new(clamped_x, clamped_y));
        }
        self.window.set_visible(true);
        unsafe { InvalidateRect(self.hwnd as _, std::ptr::null(), 0) };
    }

    fn reset_layout(&mut self) {
        if self.edit_hwnd != 0 {
            unsafe { set_window_text(self.edit_hwnd, &[]) };
        }
    }

    pub fn hide(&mut self) {
        self.window.set_visible(false);
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn set_action(&mut self, action: &str) {
        self.action = action.to_string();
        unsafe { InvalidateRect(self.hwnd as _, std::ptr::null(), 0) };
        self.window.request_redraw();
    }

    pub fn begin_request(&mut self, generation: u64) {
        self.generation = generation;
        self.result.clear();
        self.error.clear();
        self.status = "正在回答…".into();
        unsafe { InvalidateRect(self.hwnd as _, std::ptr::null(), 0) };
        self.window.request_redraw();
    }

    /// Decodes the region PNG into a paintable thumbnail (called once per
    /// capture, not per frame).
    pub fn set_thumbnail_png(&mut self, png: &[u8]) {
        let decoded = image::load_from_memory(png).ok();
        if let Some(decoded) = decoded {
            let rgba = decoded.to_rgba8();
            let (width, height) = (rgba.width(), rgba.height());
            self.thumbnail = Some((rgba.into_raw(), width, height));
        }
        unsafe { InvalidateRect(self.hwnd as _, std::ptr::null(), 0) };
        self.window.request_redraw();
    }

    pub fn append_delta(&mut self, generation: u64, text: &str) {
        if generation != self.generation {
            return; // A switched action superseded this stream.
        }
        self.result.push_str(text);
        self.error.clear();
        self.window.request_redraw();
    }

    pub fn finish(&mut self, generation: u64, ok: bool, text: &str) {
        if generation != self.generation {
            return;
        }
        if ok {
            self.result = text.to_string();
            self.error.clear();
            self.status = String::new();
        } else {
            self.error = text.to_string();
            self.status = String::new();
        }
        unsafe { InvalidateRect(self.hwnd as _, std::ptr::null(), 0) };
        self.window.request_redraw();
    }

    /// Left-click hit test: (copy pressed, send pressed, header drag).
    pub fn click(&self, x: i32, y: i32) -> (bool, bool, bool) {
        (
            point_in(&copy_button_rect(), x, y),
            point_in(&send_button_rect(), x, y),
            y < MARGIN + HEADER_H,
        )
    }

    pub fn read_follow_up(&self) -> String {
        if self.edit_hwnd == 0 {
            return String::new();
        }
        read_edit_text(self.edit_hwnd)
    }

    pub fn clear_follow_up(&self) {
        if self.edit_hwnd != 0 {
            unsafe { set_window_text(self.edit_hwnd, &[]) };
        }
    }

    pub fn focus_edit(&self) {
        if self.edit_hwnd != 0 {
            unsafe { SetFocus(self.edit_hwnd as _) };
        }
    }

    pub fn repaint(&self) {
        unsafe { paint(self) };
    }
}

// Rect used for the thumbnail display area.
fn thumbnail_rect() -> RECT {
    RECT { left: MARGIN, top: MARGIN + HEADER_H + 8, right: PANEL_WIDTH - MARGIN, bottom: MARGIN + HEADER_H + 8 + THUMB_H }
}

unsafe fn paint(panel: &Panel) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(panel.hwnd as _, &mut ps);
    if hdc.is_null() {
        return;
    }
    let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    GetClientRect(panel.hwnd as _, &mut client);
    let background = CreateSolidBrush(rgb(24, 26, 32));
    FillRect(hdc, &client, background);
    DeleteObject(background as _);

    let font = create_panel_font(-15, 500);
    let small_font = create_panel_font(-13, 400);
    let previous_font = SelectObject(hdc, font as _);
    SetBkMode(hdc, 1); // TRANSPARENT

    // Header: action label + status.
    SetTextColor(hdc, rgb(122, 178, 255));
    let mut header = RECT { left: MARGIN, top: MARGIN, right: MARGIN + 200, bottom: MARGIN + HEADER_H };
    let label: Vec<u16> = action_label(&panel.action).encode_utf16().collect();
    DrawTextW(hdc, label.as_ptr(), label.len() as i32, &mut header, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
    SelectObject(hdc, small_font as _);
    SetTextColor(hdc, if panel.error.is_empty() { rgb(150, 150, 160) } else { rgb(230, 130, 120) });
    let status_text = if !panel.error.is_empty() { "出错了".to_string() } else if !panel.status.is_empty() { panel.status.clone() } else { String::new() };
    if !status_text.is_empty() {
        let mut status_rect = RECT { left: PANEL_WIDTH - MARGIN - 140, top: MARGIN, right: PANEL_WIDTH - MARGIN, bottom: MARGIN + HEADER_H };
        let wide: Vec<u16> = status_text.encode_utf16().collect();
        DrawTextW(hdc, wide.as_ptr(), wide.len() as i32, &mut status_rect, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
    }

    // Thumbnail (region capture preview).
    let thumb = thumbnail_rect();
    if let Some((rgba, width, height)) = &panel.thumbnail {
        draw_thumbnail(hdc, rgba, *width, *height, &thumb);
    } else {
        let placeholder = CreateSolidBrush(rgb(34, 37, 44));
        FillRect(hdc, &thumb, placeholder);
        DeleteObject(placeholder as _);
    }

    // Result text: word wrapped, bottom-anchored when it overflows.
    let mut area = result_rect();
    let shown = tail_for_paint(&panel.result, 4000);
    let text: Vec<u16> = shown.encode_utf16().collect();
    SelectObject(hdc, font as _);
    SetTextColor(hdc, rgb(232, 232, 240));
    if !text.is_empty() {
        let mut measure = area;
        DrawTextW(hdc, text.as_ptr(), text.len() as i32, &mut measure, DT_CALCRECT | DT_WORDBREAK);
        if measure.bottom > area.bottom {
            area.top = area.bottom - (area.bottom - area.top).min(measure.bottom - measure.top);
        }
        let mut draw = area;
        DrawTextW(hdc, text.as_ptr(), text.len() as i32, &mut draw, DT_WORDBREAK);
    } else if panel.error.is_empty() && panel.status.is_empty() {
        SetTextColor(hdc, rgb(120, 120, 130));
        let mut hint = area;
        let hint_text: Vec<u16> = "回答会显示在这里。".encode_utf16().collect();
        DrawTextW(hdc, hint_text.as_ptr(), hint_text.len() as i32, &mut hint, DT_WORDBREAK);
    }
    if !panel.error.is_empty() {
        SetTextColor(hdc, rgb(230, 130, 120));
        let mut error_rect = result_rect();
        let error_text: Vec<u16> = panel.error.encode_utf16().collect();
        DrawTextW(hdc, error_text.as_ptr(), error_text.len() as i32, &mut error_rect, DT_WORDBREAK);
    }

    // Copy button.
    let copy = copy_button_rect();
    let copy_fill = CreateSolidBrush(rgb(52, 56, 66));
    FillRect(hdc, &copy, copy_fill);
    DeleteObject(copy_fill as _);
    SetTextColor(hdc, rgb(235, 235, 245));
    SelectObject(hdc, small_font as _);
    let copy_label: Vec<u16> = "复制结果".encode_utf16().collect();
    let mut copy_text = copy;
    DrawTextW(hdc, copy_label.as_ptr(), copy_label.len() as i32, &mut copy_text, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);

    // Send button (the Edit control paints itself).
    let send = send_button_rect();
    let send_fill = CreateSolidBrush(rgb(122, 178, 255));
    FillRect(hdc, &send, send_fill);
    DeleteObject(send_fill as _);
    SetTextColor(hdc, rgb(20, 24, 32));
    let send_label: Vec<u16> = "发送".encode_utf16().collect();
    let mut send_text = send;
    DrawTextW(hdc, send_label.as_ptr(), send_label.len() as i32, &mut send_text, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);

    SelectObject(hdc, previous_font);
    if font != GetStockObject(DEFAULT_GUI_FONT) as HFONT {
        DeleteObject(font as _);
    }
    if small_font != GetStockObject(DEFAULT_GUI_FONT) as HFONT {
        DeleteObject(small_font as _);
    }
    EndPaint(panel.hwnd as _, &ps);
}

/// Draws an RGBA thumbnail scaled to fit inside `area`, centered.
unsafe fn draw_thumbnail(hdc: HDC, rgba: &[u8], width: u32, height: u32, area: &RECT) {
    use windows_sys::Win32::Graphics::Gdi::{StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, SRCCOPY};
    if width == 0 || height == 0 {
        return;
    }
    let area_w = (area.right - area.left) as f64;
    let area_h = (area.bottom - area.top) as f64;
    let scale = (area_w / width as f64).min(area_h / height as f64).min(2.0);
    let draw_w = (width as f64 * scale).max(1.0) as i32;
    let draw_h = (height as f64 * scale).max(1.0) as i32;
    let x = area.left + ((area.right - area.left) - draw_w) / 2;
    let y = area.top + ((area.bottom - area.top) - draw_h) / 2;
    // RGBA → BGRA (top-down via negative biHeight).
    let mut bgra: Vec<u8> = rgba.chunks_exact(4).flat_map(|px| [px[2], px[1], px[0], px[3]]).collect();
    let mut bmi: BITMAPINFO = std::mem::zeroed();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = width as i32;
    bmi.bmiHeader.biHeight = -(height as i32);
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = 0; // BI_RGB
    StretchDIBits(
        hdc,
        x, y, draw_w, draw_h,
        0, 0, width as i32, height as i32,
        bgra.as_mut_ptr() as *const std::ffi::c_void,
        &bmi,
        DIB_RGB_COLORS,
        SRCCOPY,
    );
}

/// Bounds the paint cost for long answers: only the tail is rendered, which is
/// also the part the user is reading while the stream appends.
fn tail_for_paint(text: &str, maximum_chars: usize) -> &str {
    let char_count = text.chars().count();
    if char_count <= maximum_chars {
        return text;
    }
    let skip = char_count - maximum_chars;
    let start = text.char_indices().nth(skip).map(|(index, _)| index).unwrap_or(0);
    let tail = &text[start..];
    match tail.find('\n') {
        Some(position) => &tail[position + 1..],
        None => tail,
    }
}

unsafe fn create_panel_font(size: i32, weight: i32) -> HFONT {
    let face: Vec<u16> = "Microsoft YaHei UI\0".encode_utf16().collect();
    let font = CreateFontW(size, 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr());
    if font.is_null() {
        GetStockObject(DEFAULT_GUI_FONT) as HFONT
    } else {
        font
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_test_rects_stay_inside_the_panel() {
        let copy = copy_button_rect();
        let send = send_button_rect();
        let result = result_rect();
        for rect in [&copy, &send, &result] {
            assert!(rect.left >= 0 && rect.top >= 0);
            assert!(rect.right <= PANEL_WIDTH && rect.bottom <= PANEL_HEIGHT);
            assert!(rect.right > rect.left && rect.bottom > rect.top);
        }
        // The result area must not overlap the footer controls.
        assert!(result.bottom < copy.top && result.bottom < send.top);
        assert!(copy.bottom < send.top);
    }

    #[test]
    fn click_hit_tests_split_the_three_zones() {
        let (copy, send, header) = (copy_button_rect(), send_button_rect(), 0..MARGIN + HEADER_H);
        let panel_point = |x: i32, y: i32| (point_in(&copy, x, y), point_in(&send, x, y), header.contains(&y));
        assert_eq!(panel_point(copy.left + 5, copy.top + 5), (true, false, false));
        assert_eq!(panel_point(send.left + 5, send.top + 5), (false, true, false));
        assert_eq!(panel_point(PANEL_WIDTH / 2, 10), (false, false, true));
        assert_eq!(panel_point(PANEL_WIDTH / 2, PANEL_HEIGHT - 50), (false, false, false));
    }

    #[test]
    fn paint_tail_keeps_the_last_page_of_long_answers() {
        let long = "段落\n".repeat(3000);
        let tail = tail_for_paint(&long, 100);
        assert!(tail.chars().count() <= 101);
        assert!(tail.starts_with('\n') || tail.starts_with("段落"));
        assert_eq!(tail_for_paint("短回答", 100), "短回答");
    }
}
