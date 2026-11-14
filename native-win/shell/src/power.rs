// Sleep/resume notification (WM_POWERBROADCAST), porting AppHost's
// NSWorkspace.willSleepNotification. A dedicated message-only window on its
// own thread receives the broadcast; the event loop reacts by stopping audio
// and screen capture before the machine suspends (capture restarts through
// the regular /api/state poll after resume).
use std::cell::RefCell;

use tao::event_loop::EventLoopProxy;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetMessageW, RegisterClassW, UnregisterClassW,
    HWND_MESSAGE, MSG, WNDCLASSW,
};

// Not exposed by every windows-sys feature set; stable ABI values.
const WM_POWERBROADCAST: u32 = 0x0218;
const PBT_APMSUSPEND: usize = 0x0004;
const PBT_APMRESUME: usize = 0x0007;
const PBT_APMRESUMEAUTOMATIC: usize = 0x0018;
const CLASS_NAME: &str = "NuPhyLabPowerWindow";

thread_local! {
    static PROXY: RefCell<Option<EventLoopProxy<crate::UserEvent>>> = const { RefCell::new(None) };
}

/// Starts the watcher thread; harmless to call once at shell startup.
pub fn watch(proxy: EventLoopProxy<crate::UserEvent>) {
    std::thread::spawn(move || unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = CLASS_NAME.to_wide();
        let class_ptr = class.as_ptr();
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = instance;
        wc.lpszClassName = class_ptr;
        if RegisterClassW(&wc) == 0 {
            return; // Without the window there is no notification; the existing watchdogs still cover resume.
        }
        // Message-only window: no visibility, no z-order, receives broadcasts.
        let hwnd = CreateWindowExW(
            0,
            class_ptr,
            std::ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            UnregisterClassW(class_ptr, instance);
            return;
        }
        PROXY.with(|slot| *slot.borrow_mut() = Some(proxy));
        let mut message: MSG = std::mem::zeroed();
        // Returns 0 on WM_QUIT; negative on failure — both end the pump.
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {}
        DestroyWindow(hwnd);
        UnregisterClassW(class_ptr, instance);
    });
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_POWERBROADCAST {
        let sleeping = match wparam as usize {
            PBT_APMSUSPEND => Some(true),
            PBT_APMRESUME | PBT_APMRESUMEAUTOMATIC => Some(false),
            _ => None,
        };
        if let Some(sleeping) = sleeping {
            let _ = PROXY.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .and_then(|proxy| proxy.send_event(crate::UserEvent::PowerChanged { sleeping }).ok())
            });
        }
        // TRUE acknowledges the notification (a veto would return
        // BROADCAST_QUERY_DENY); sleep is never vetoed.
        return 1;
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Allocation-free wide-string helper for the static class name.
trait ToWide {
    fn to_wide(&self) -> Vec<u16>;
}
impl ToWide for &str {
    fn to_wide(&self) -> Vec<u16> {
        self.encode_utf16().chain(std::iter::once(0)).collect()
    }
}
