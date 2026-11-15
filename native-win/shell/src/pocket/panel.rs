// Native floating panel for Keyboard Pocket (simplified PocketPanel.swift):
// a topmost, never-activating popup window drawn with plain GDI. It shows the
// nine slots while a prefix is held or after the openPanel op; clicking a slot
// stores into it (capture mode) or retrieves it (use mode). ESC closes it via
// the hook path, so the window never takes keyboard focus.

use std::path::Path;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tao::dpi::PhysicalSize;
use tao::event_loop::EventLoop;
use tao::window::{Window, WindowBuilder};
use windows_sys::Win32::Foundation::{POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, GetStockObject,
    ScreenToClient, SelectObject, SetBkMode, SetTextColor, DEFAULT_GUI_FONT, DT_END_ELLIPSIS, DT_SINGLELINE,
    DT_VCENTER, HDC, HFONT, PAINTSTRUCT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetClientRect, GetCursorPos, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST};

use super::keys::SLOT_COUNT;
use super::store::is_image_extension;
use super::PocketCore;

/// COLORREF helper (windows-sys 0.59 has no RGB macro): 0x00BBGGRR.
fn rgb(r: u32, g: u32, b: u32) -> u32 {
    (r & 0xFF) | ((g & 0xFF) << 8) | ((b & 0xFF) << 16)
}

const MARGIN: i32 = 10;
const HEADER: i32 = 26;
const CELL_WIDTH: i32 = 224;
const CELL_HEIGHT: i32 = 58;
const GAP: i32 = 8;

pub struct Panel {
    pub window: Window,
    hwnd: isize,
    width: i32,
    height: i32,
}

impl Panel {
    pub fn create(event_loop: &EventLoop<crate::UserEvent>) -> Panel {
        let width = MARGIN * 2 + CELL_WIDTH * 3 + GAP * 2;
        let height = MARGIN * 2 + HEADER + CELL_HEIGHT * 3 + GAP * 2;
        let window = WindowBuilder::new()
            .with_title("NuPhyLab Pocket")
            .with_visible(false)
            .with_decorations(false)
            .with_resizable(false)
            .with_always_on_top(true)
            .with_inner_size(PhysicalSize::new(width as u32, height as u32))
            .build(event_loop)
            .expect("pocket panel window");
        let hwnd = match window.window_handle().ok().map(|h| h.as_raw()) {
            Some(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
            _ => 0,
        };

        if hwnd != 0 {
            unsafe {
                // Never-activating, tool window (no taskbar entry), above all.
                SetWindowLongPtrW(
                    hwnd as _,
                    GWL_EXSTYLE,
                    (WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_TOOLWINDOW) as isize,
                );
            }
        }
        Panel { window, hwnd, width, height }
    }

    /// Shows the panel near the bottom-center of the primary work area.
    pub fn show(&self) {
        if let Some(area) = crate::win::primary_work_area() {
            let x = area.left + ((area.right - area.left) - self.width) / 2;
            let y = area.bottom - self.height - 80;
            self.window.set_outer_position(tao::dpi::PhysicalPosition::new(x.max(area.left), y.max(area.top)));
        }
        self.window.set_visible(true);
        self.window.request_redraw();
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
    }

    /// Maps a click (physical client coordinates) to a display position 0..8.
    /// Display order follows settings.slotOrder, matching the Mac panel.
    pub fn cell_at(&self, x: i32, y: i32) -> Option<usize> {
        let inner_x = x - MARGIN;
        let inner_y = y - MARGIN - HEADER;
        if inner_x < 0 || inner_y < 0 {
            return None;
        }
        let column = inner_x / (CELL_WIDTH + GAP);
        let row = inner_y / (CELL_HEIGHT + GAP);
        let within_cell_x = inner_x % (CELL_WIDTH + GAP) < CELL_WIDTH;
        let within_cell_y = inner_y % (CELL_HEIGHT + GAP) < CELL_HEIGHT;
        if !(0..3).contains(&column) || !(0..3).contains(&row) || !within_cell_x || !within_cell_y {
            return None;
        }
        Some((row * 3 + column) as usize)
    }

    /// Hit test at the current cursor position (converted to client coords).
    pub fn cursor_cell(&self) -> Option<usize> {
        let (x, y) = self.client_cursor();
        self.cell_at(x, y)
    }

    /// Current cursor position converted to panel client coordinates.
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

    pub fn repaint(&self, core: &PocketCore) {
        unsafe { paint(self.hwnd, core) };
    }
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "text" => "文字",
        "richText" => "富文本",
        "link" => "链接",
        "image" => "图片",
        "files" => "文件",
        _ => "内容",
    }
}

fn kind_label_en(kind: &str) -> &'static str {
    match kind {
        "text" => "Text",
        "richText" => "Rich text",
        "link" => "Link",
        "image" => "Image",
        "files" => "Files",
        _ => "Content",
    }
}

unsafe fn paint(hwnd: isize, core: &PocketCore) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd as _, &mut ps);
    if hdc.is_null() {
        return;
    }
    let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    GetClientRect(hwnd as _, &mut client);
    let background = CreateSolidBrush(rgb(24, 24, 27));
    FillRect(hdc, &client, background);
    DeleteObject(background as _);

    // Font: CJK-capable UI face.
    let font = create_panel_font();
    let previous_font = SelectObject(hdc, font as _);
    SetBkMode(hdc, 1); // TRANSPARENT

    let capture_mode = core.panel_capture_mode;
    let header = if capture_mode {
        "按槽位键收纳 / Press a slot key to store"
    } else {
        "选择槽位取用 / Choose a slot to retrieve"
    };
    SetTextColor(hdc, rgb(160, 160, 170));
    draw_text(hdc, header, &RECT { left: MARGIN, top: MARGIN, right: client.right - MARGIN, bottom: MARGIN + HEADER });

    let order: Vec<usize> = {
        let valid = core.settings.slot_order.len() == SLOT_COUNT
            && core.settings.slot_order.iter().collect::<std::collections::HashSet<_>>().len() == SLOT_COUNT;
        if valid {
            core.settings.slot_order.clone()
        } else {
            (0..SLOT_COUNT).collect()
        }
    };
    for display_position in 0..SLOT_COUNT {
        let index = order[display_position];
        let row = (display_position / 3) as i32;
        let column = (display_position % 3) as i32;
        let left = MARGIN + column * (CELL_WIDTH + GAP);
        let top = MARGIN + HEADER + row * (CELL_HEIGHT + GAP);
        let cell_rect = RECT { left, top, right: left + CELL_WIDTH, bottom: top + CELL_HEIGHT };
        let cell_brush = CreateSolidBrush(rgb(44, 44, 48));
        FillRect(hdc, &cell_rect, cell_brush);
        DeleteObject(cell_brush as _);

        let key_name = core.settings.slot_key_names().get(index).cloned().unwrap_or_else(|| format!("{}", index + 1));
        let mut badge = RECT { left: left + 8, top: top + 6, right: left + 40, bottom: top + 30 };
        SetTextColor(hdc, rgb(250, 200, 90));
        draw_text(hdc, &key_name, &badge);

        let Some(item) = core.store.slots.get(index).and_then(|slot| slot.as_ref()) else {
            badge.top = top + CELL_HEIGHT / 2 - 10;
            badge.bottom = top + CELL_HEIGHT / 2 + 14;
            SetTextColor(hdc, rgb(110, 110, 118));
            draw_text(hdc, "空口袋 / Empty", &badge);
            continue;
        };
        let missing = core.store.slot_missing(item);
        let mut title_rect = RECT { left: left + 44, top: top + 6, right: left + CELL_WIDTH - 8, bottom: top + 30 };
        let mut title = format!("{} {}", kind_label(&item.kind), item.display_title());
        if item.kind == "files" && !is_image_extension(&item.title) && item.files.len() > 1 {
            title = format!("文件 {} 个 / {} files", item.files.len(), item.files.len());
        }
        if missing {
            title.push_str(" ·原文件不可用");
        }
        SetTextColor(hdc, if missing { rgb(220, 130, 120) } else { rgb(235, 235, 240) });
        draw_text(hdc, &title, &title_rect);
        let summary = summary_for(item);
        title_rect.top = top + 32;
        title_rect.bottom = top + CELL_HEIGHT - 6;
        SetTextColor(hdc, rgb(150, 150, 160));
        draw_text(hdc, &summary, &title_rect);
    }

    SelectObject(hdc, previous_font);
    if font != unsafe { GetStockObject(DEFAULT_GUI_FONT) as _ } {
        DeleteObject(font as _);
    }
    EndPaint(hwnd as _, &ps);
}

fn summary_for(item: &super::store::PocketItem) -> String {
    let mut summary = item
        .text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| text.chars().take(120).collect::<String>())
        .unwrap_or_else(|| {
            if item.kind == "files" && item.files.len() == 1 {
                Path::new(&item.files[0])
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            } else {
                String::new()
            }
        });
    if summary.is_empty() {
        summary = format!("{} / {}", kind_label(&item.kind), kind_label_en(&item.kind));
    }
    summary
}

unsafe fn draw_text(hdc: HDC, text: &str, rect: &RECT) {
    let wide: Vec<u16> = text.encode_utf16().collect();
    DrawTextW(hdc, wide.as_ptr(), wide.len() as i32, rect as *const RECT as *mut RECT, DT_SINGLELINE | DT_END_ELLIPSIS | DT_VCENTER);
}

unsafe fn create_panel_font() -> HFONT {
    // "Microsoft YaHei UI" covers the CJK strings; fall back to the stock GUI
    // font when creation fails.
    let face: Vec<u16> = "Microsoft YaHei UI\0".encode_utf16().collect();
    let font = CreateFontW(
        -15,
        0,
        0,
        0,
        400,
        0,
        0,
        0,
        1, // DEFAULT_CHARSET
        0,
        0,
        5, // CLEARTYPE_QUALITY
        0,
        face.as_ptr(),
    );
    if font.is_null() {
        GetStockObject(DEFAULT_GUI_FONT) as HFONT
    } else {
        font
    }
}
