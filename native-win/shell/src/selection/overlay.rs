// Fullscreen selection overlay (Windows port of the FlowKit region selector):
// a topmost layered window covering the whole virtual desktop with a dim
// alpha, a rubber-band rectangle drawn while dragging, and mouse capture so
// the gesture survives leaving the window bounds. On mouse-up the normalized
// screen rect goes back to the worker for the DXGI region capture.

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tao::dpi::{PhysicalPosition, PhysicalSize};
use tao::event_loop::EventLoop;
use tao::window::{Window, WindowBuilder};
use windows_sys::Win32::Foundation::{POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect, GetStockObject,
    ScreenToClient, SelectObject, SetBkMode, SetTextColor, DEFAULT_GUI_FONT, DT_END_ELLIPSIS, DT_SINGLELINE, DT_VCENTER,
    HFONT, PAINTSTRUCT,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetCursorPos, GetWindowLongPtrW, SetLayeredWindowAttributes, SetWindowLongPtrW, GWL_EXSTYLE, LWA_ALPHA,
    WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};

/// A drag shorter than this is treated as a click and cancels (a zero-size
/// region would make a useless request).
pub const MINIMUM_REGION: i32 = 4;

/// Normalizes two corner points into an (x0, y0, x1, y1) rect with x0 <= x1,
/// y0 <= y1. Returns None when the region is too small to be intentional.
pub fn normalize_region(start: (i32, i32), end: (i32, i32)) -> Option<(i32, i32, i32, i32)> {
    let x0 = start.0.min(end.0);
    let y0 = start.1.min(end.1);
    let x1 = start.0.max(end.0);
    let y1 = start.1.max(end.1);
    (x1 - x0 >= MINIMUM_REGION && y1 - y0 >= MINIMUM_REGION).then_some((x0, y0, x1, y1))
}

/// COLORREF helper (windows-sys 0.59 has no RGB macro): 0x00BBGGRR.
fn rgb(r: u32, g: u32, b: u32) -> u32 {
    (r & 0xFF) | ((g & 0xFF) << 8) | ((b & 0xFF) << 16)
}

pub struct Overlay {
    pub window: Window,
    hwnd: isize,
    origin: (i32, i32),
    width: i32,
    height: i32,
    drag_start: Option<(i32, i32)>,
    cursor: (i32, i32),
    active: bool,
}

impl Overlay {
    pub fn create(event_loop: &EventLoop<crate::UserEvent>) -> Overlay {
        let (vx, vy, vw, vh) = crate::win::virtual_screen();
        let width = if vw > 0 { vw } else { 1920 };
        let height = if vh > 0 { vh } else { 1080 };
        let window = WindowBuilder::new()
            .with_title("NuPhyLab Selection")
            .with_visible(false)
            .with_decorations(false)
            .with_resizable(false)
            .with_always_on_top(true)
            .with_inner_size(PhysicalSize::new(width as u32, height as u32))
            .build(event_loop)
            .expect("selection overlay window");
        let hwnd = match window.window_handle().ok().map(|h| h.as_raw()) {
            Some(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
            _ => 0,
        };
        if hwnd != 0 {
            unsafe {
                let ex_style = GetWindowLongPtrW(hwnd as _, GWL_EXSTYLE) as u32;
                SetWindowLongPtrW(hwnd as _, GWL_EXSTYLE, (ex_style | WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW) as isize);
                // Whole-window dim: the desktop shows through at ~55% brightness.
                SetLayeredWindowAttributes(hwnd as _, 0, 140, LWA_ALPHA);
            }
        }
        Overlay {
            window,
            hwnd,
            origin: (vx, vy),
            width,
            height,
            drag_start: None,
            cursor: (-1, -1),
            active: false,
        }
    }

    /// Covers the (recomputed) virtual desktop and takes over the screen.
    pub fn show(&mut self) {        let (vx, vy, vw, vh) = crate::win::virtual_screen();
        if vw > 0 && vh > 0 {
            self.origin = (vx, vy);
            self.width = vw;
            self.height = vh;
            self.window.set_outer_position(PhysicalPosition::new(vx, vy));
            self.window.set_inner_size(PhysicalSize::new(vw as u32, vh as u32));
        }
        self.drag_start = None;
        self.cursor = (-1, -1);
        self.active = true;
        self.window.set_visible(true);
        self.window.request_redraw();
    }

    pub fn hide(&mut self) {
        self.active = false;
        self.drag_start = None;
        self.window.set_visible(false);
    }

    /// Client coordinates → virtual-desktop coordinates.
    fn to_screen(&self, x: i32, y: i32) -> (i32, i32) {
        (self.origin.0 + x, self.origin.1 + y)
    }

    /// Current cursor position in window-client coordinates (the tao mouse
    /// events carry DPI-scaled positions; the physical read keeps the region
    /// math in native pixels).
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

    pub fn on_mouse_down(&mut self, x: i32, y: i32) {
        if !self.active {
            return;
        }
        unsafe {
            SetCapture(self.hwnd as _);
        }
        self.drag_start = Some(self.to_screen(x, y));
        self.cursor = self.to_screen(x, y);
        self.window.request_redraw();
    }

    pub fn on_mouse_move(&mut self, x: i32, y: i32) {
        if !self.active {
            return;
        }
        self.cursor = self.to_screen(x, y);
        if self.drag_start.is_some() {
            self.window.request_redraw();
        }
    }

    /// Finishes the gesture; returns the normalized screen region when the
    /// drag was intentional (≥ MINIMUM_REGION in both axes).
    pub fn on_mouse_up(&mut self) -> Option<(i32, i32, i32, i32)> {
        if !self.active {
            return None;
        }
        unsafe {
            ReleaseCapture();
        }
        let region = self.drag_start.and_then(|start| normalize_region(start, self.cursor));
        self.drag_start = None;
        region
    }

    /// A shortcut key was released before the drag started: cancel quietly.
    /// Returns whether the overlay was actually dismissed.
    pub fn cancel_if_undragged(&mut self) -> bool {
        let cancelled = self.active && self.drag_start.is_none();
        if cancelled {
            self.hide();
        }
        cancelled
    }

    pub fn repaint(&self) {
        unsafe { paint(self) };
    }
}

unsafe fn paint(overlay: &Overlay) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(overlay.hwnd as _, &mut ps);
    if hdc.is_null() {
        return;
    }
    let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    GetClientRect(overlay.hwnd as _, &mut client);
    let background = CreateSolidBrush(rgb(12, 14, 20));
    FillRect(hdc, &client, background);
    DeleteObject(background as _);

    // The drag rectangle: a 2px bright frame around a slightly darker fill so
    // the region reads clearly even through the layered alpha.
    if let Some(start) = overlay.drag_start {
        let x0 = (start.0 - overlay.origin.0).clamp(client.left, client.right);
        let y0 = (start.1 - overlay.origin.1).clamp(client.top, client.bottom);
        let x1 = (overlay.cursor.0 - overlay.origin.0).clamp(client.left, client.right);
        let y1 = (overlay.cursor.1 - overlay.origin.1).clamp(client.top, client.bottom);
        let left = x0.min(x1);
        let top = y0.min(y1);
        let right = x0.max(x1);
        let bottom = y0.max(y1);
        if right - left >= MINIMUM_REGION && bottom - top >= MINIMUM_REGION {
            let fill = CreateSolidBrush(rgb(10, 12, 18));
            let inside = RECT { left: left + 2, top: top + 2, right: right - 2, bottom: bottom - 2 };
            FillRect(hdc, &inside, fill);
            DeleteObject(fill as _);
            let frame = CreateSolidBrush(rgb(240, 240, 250));
            let outer = RECT { left, top, right, bottom };
            FrameRect(hdc, &outer, frame);
            let inner = RECT { left: left + 1, top: top + 1, right: right - 1, bottom: bottom - 1 };
            FrameRect(hdc, &inner, frame);
            DeleteObject(frame as _);
        }
    }

    // Hint near the cursor.
    let font = create_overlay_font();
    let previous_font = SelectObject(hdc, font as _);
    SetBkMode(hdc, 1); // TRANSPARENT
    SetTextColor(hdc, rgb(235, 235, 245));
    let hint_x = (overlay.cursor.0 - overlay.origin.0 + 18).clamp(client.left, client.right - 240);
    let hint_y = (overlay.cursor.1 - overlay.origin.1 + 18).clamp(client.top, client.bottom - 32);
    let hint_rect = RECT { left: hint_x, top: hint_y, right: hint_x + 236, bottom: hint_y + 26 };
    let text: Vec<u16> = "拖动框选，松开执行动作 · Esc 取消".encode_utf16().collect();
    DrawTextW(hdc, text.as_ptr(), text.len() as i32, &hint_rect as *const RECT as *mut RECT, DT_SINGLELINE | DT_END_ELLIPSIS | DT_VCENTER);
    SelectObject(hdc, previous_font);
    if font != GetStockObject(DEFAULT_GUI_FONT) as HFONT {
        DeleteObject(font as _);
    }
    EndPaint(overlay.hwnd as _, &ps);
}

unsafe fn create_overlay_font() -> HFONT {
    let face: Vec<u16> = "Microsoft YaHei UI\0".encode_utf16().collect();
    let font = CreateFontW(
        -15, 0, 0, 0, 500, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr(),
    );
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
    fn regions_normalize_across_drag_directions() {
        assert_eq!(normalize_region((10, 20), (110, 80)), Some((10, 20, 110, 80)));
        assert_eq!(normalize_region((110, 80), (10, 20)), Some((10, 20, 110, 80)));
        assert_eq!(normalize_region((-50, -10), (30, 40)), Some((-50, -10, 30, 40)));
        assert_eq!(normalize_region((0, 0), (100, 100)), Some((0, 0, 100, 100)));
    }

    #[test]
    fn tiny_or_empty_drags_do_not_capture() {
        assert_eq!(normalize_region((10, 10), (12, 90)), None, "below the width floor");
        assert_eq!(normalize_region((10, 10), (90, 12)), None, "below the height floor");
        assert_eq!(normalize_region((10, 10), (10, 10)), None, "a plain click selects nothing");
        assert_eq!(normalize_region((10, 10), (15, 15)), Some((10, 10, 15, 15)));
    }
}
