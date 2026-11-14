// Windows HID transport, pure Rust with no C build dependency: hid.dll and
// setupapi.dll are loaded at runtime, so no Windows SDK import libraries are
// needed and the bundled MinGW linker of the GNU toolchain suffices.
//
// The S4 protocol speaks 64-byte reports without a report ID. Windows HID
// always carries the report ID in byte 0, so a report-ID-less device writes
// 0x00 + 64 bytes (65 total = HIDP_CAPS report lengths) and reads return the
// 0x00 report ID before the 64 report bytes. This is the classic macOS-to-
// Windows HID difference and is verified by the real-device QA checklist.
#![allow(non_snake_case, non_camel_case_types)]

use crate::device::Transport;
use crate::error::{failure, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type HANDLE = isize;
const INVALID_HANDLE_VALUE: HANDLE = -1;

const GENERIC_READ: u32 = 0x80000000;
const GENERIC_WRITE: u32 = 0x40000000;
const FILE_SHARE_READ: u32 = 1;
const FILE_SHARE_WRITE: u32 = 2;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_OVERLAPPED: u32 = 0x40000000;
const WAIT_TIMEOUT: u32 = 0x102;
const ERROR_IO_PENDING: u32 = 997;
const ERROR_ALREADY_EXISTS: u32 = 183;
const ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;
const ERROR_NO_SUCH_DEVICE: u32 = 433;
const ERROR_GEN_FAILURE: u32 = 31;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GUID {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

const GUID_DEVINTERFACE_HID: GUID = GUID {
    data1: 0x4d1e55b2,
    data2: 0xf16f,
    data3: 0x11cf,
    data4: [0x88, 0xcb, 0x00, 0x11, 0x11, 0x00, 0x00, 0x30],
};

#[repr(C)]
struct OVERLAPPED {
    Internal: usize,
    InternalHigh: usize,
    Offset: u32,
    OffsetHigh: u32,
    hEvent: HANDLE,
}

#[repr(C)]
#[derive(Default)]
struct HIDD_ATTRIBUTES {
    Size: u32,
    VendorID: u16,
    ProductID: u16,
    VersionNumber: u16,
}

#[repr(C)]
#[derive(Default)]
struct HIDP_CAPS {
    UsagePage: u16,
    Usage: u16,
    InputReportByteLength: u16,
    OutputReportByteLength: u16,
    FeatureReportByteLength: u16,
    Reserved: [u16; 17],
    NumberLinkCollectionNodes: u16,
    NumberInputButtonCaps: u16,
    NumberInputValueCaps: u16,
    NumberInputDataIndices: u16,
    NumberOutputButtonCaps: u16,
    NumberOutputValueCaps: u16,
    NumberOutputDataIndices: u16,
    NumberFeatureButtonCaps: u16,
    NumberFeatureValueCaps: u16,
    NumberFeatureDataIndices: u16,
}

#[repr(C)]
struct SP_DEVICE_INTERFACE_DATA {
    cbSize: u32,
    InterfaceClassGuid: GUID,
    Flags: u32,
    Reserved: usize,
}

extern "system" {
    fn LoadLibraryW(name: *const u16) -> HANDLE;
    fn GetProcAddress(module: HANDLE, name: *const u8) -> *mut core::ffi::c_void;
    fn CreateFileW(name: *const u16, access: u32, share: u32, security: *const core::ffi::c_void,
        disposition: u32, flags: u32, template: HANDLE) -> HANDLE;
    fn ReadFile(handle: HANDLE, buffer: *mut u8, count: u32, read: *mut u32, overlapped: *mut OVERLAPPED) -> i32;
    fn WriteFile(handle: HANDLE, buffer: *const u8, count: u32, written: *mut u32, overlapped: *mut OVERLAPPED) -> i32;
    fn CloseHandle(handle: HANDLE) -> i32;
    fn CreateMutexW(security: *const core::ffi::c_void, initial: i32, name: *const u16) -> HANDLE;
    fn GetLastError() -> u32;
    fn CreateEventW(security: *const core::ffi::c_void, manual: i32, initial: i32, name: *const u16) -> HANDLE;
    fn WaitForSingleObject(handle: HANDLE, milliseconds: u32) -> u32;
    fn CancelIoEx(handle: HANDLE, overlapped: *mut OVERLAPPED) -> i32;
    fn GetOverlappedResult(handle: HANDLE, overlapped: *mut OVERLAPPED, transferred: *mut u32, wait: i32) -> i32;
    fn GetTickCount64() -> u64;
}

#[link(name = "user32")]
extern "system" {
    fn GetLastInputInfo(info: *mut LASTINPUTINFO) -> i32;
}

#[repr(C)]
pub struct LASTINPUTINFO {
    pub cbSize: u32,
    pub dwTime: u32,
}

pub fn system_idle_ms() -> Option<u64> {
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    unsafe {
        if GetLastInputInfo(&mut info) == 0 {
            return None;
        }
        let tick = GetTickCount64();
        let mut delta = tick.wrapping_sub(info.dwTime as u64);
        if delta > u32::MAX as u64 {
            delta = 0; // Tick wrapped more than once; report "just active".
        }
        Some(delta)
    }
}

type HidD_GetAttributes = unsafe extern "system" fn(HANDLE, *mut HIDD_ATTRIBUTES) -> i32;
type HidD_GetPreparsedData = unsafe extern "system" fn(HANDLE, *mut *mut core::ffi::c_void) -> i32;
type HidD_FreePreparsedData = unsafe extern "system" fn(*mut core::ffi::c_void) -> i32;
type HidP_GetCaps = unsafe extern "system" fn(*mut core::ffi::c_void, *mut HIDP_CAPS) -> i32;
type HidD_GetSerialNumberStringW = unsafe extern "system" fn(HANDLE, *mut u16, u32) -> i32;
type SetupDiGetClassDevsW = unsafe extern "system" fn(*const GUID, *const u16, HANDLE, u32) -> HANDLE;
type SetupDiEnumDeviceInterfaces =
    unsafe extern "system" fn(HANDLE, *const core::ffi::c_void, *const GUID, u32, *mut SP_DEVICE_INTERFACE_DATA) -> i32;
type SetupDiGetDeviceInterfaceDetailW = unsafe extern "system" fn(
    HANDLE, *const SP_DEVICE_INTERFACE_DATA, *mut u8, u32, *mut u32, *mut core::ffi::c_void) -> i32;
type SetupDiDestroyDeviceInfoList = unsafe extern "system" fn(HANDLE) -> i32;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

struct Library {
    module: HANDLE,
}

impl Library {
    fn load(name: &str) -> Option<Library> {
        let wide_name = wide(name);
        unsafe {
            let module = LoadLibraryW(wide_name.as_ptr());
            if module == 0 {
                crate::device::diagnostic(&format!("LoadLibraryW({name}) failed with error {}", GetLastError()));
                None
            } else {
                Some(Library { module })
            }
        }
    }
    fn get<T>(&self, name: &str) -> Option<T> {
        let symbol = match std::ffi::CString::new(name) {
            Ok(symbol) => symbol,
            Err(_) => return None,
        };
        unsafe {
            let address = GetProcAddress(self.module, symbol.as_ptr() as *const u8);
            if address.is_null() {
                None
            } else {
                Some(std::mem::transmute_copy(&address))
            }
        }
    }
}

struct HidApi {
    get_attributes: HidD_GetAttributes,
    get_preparsed_data: HidD_GetPreparsedData,
    free_preparsed_data: HidD_FreePreparsedData,
    get_caps: HidP_GetCaps,
    get_serial: HidD_GetSerialNumberStringW,
}

fn hid_api() -> Option<HidApi> {
    let library = Library::load("hid.dll")?;
    let required = ["HidD_GetAttributes", "HidD_GetPreparsedData", "HidD_FreePreparsedData", "HidP_GetCaps", "HidD_GetSerialNumberString"];
    if let Some(missing) = required.into_iter().find(|name| library.get::<usize>(name).is_none()) {
        crate::device::diagnostic(&format!("GetProcAddress({missing}) failed with error {}", unsafe { GetLastError() }));
        return None;
    }
    Some(HidApi {
        get_attributes: library.get("HidD_GetAttributes")?,
        get_preparsed_data: library.get("HidD_GetPreparsedData")?,
        free_preparsed_data: library.get("HidD_FreePreparsedData")?,
        get_caps: library.get("HidP_GetCaps")?,
        get_serial: library.get("HidD_GetSerialNumberString")?,
    })
}

struct SetupApi {
    get_class_devs: SetupDiGetClassDevsW,
    enum_interfaces: SetupDiEnumDeviceInterfaces,
    get_detail: SetupDiGetDeviceInterfaceDetailW,
    destroy: SetupDiDestroyDeviceInfoList,
}

fn setup_api() -> Option<SetupApi> {
    let library = Library::load("setupapi.dll")?;
    let required = ["SetupDiGetClassDevsW", "SetupDiEnumDeviceInterfaces", "SetupDiGetDeviceInterfaceDetailW", "SetupDiDestroyDeviceInfoList"];
    if let Some(missing) = required.into_iter().find(|name| library.get::<usize>(name).is_none()) {
        crate::device::diagnostic(&format!("GetProcAddress({missing}) failed with error {}", unsafe { GetLastError() }));
        return None;
    }
    Some(SetupApi {
        get_class_devs: library.get("SetupDiGetClassDevsW")?,
        enum_interfaces: library.get("SetupDiEnumDeviceInterfaces")?,
        get_detail: library.get("SetupDiGetDeviceInterfaceDetailW")?,
        destroy: library.get("SetupDiDestroyDeviceInfoList")?,
    })
}

const DIGCF_PRESENT: u32 = 0x2;
const DIGCF_DEVICEINTERFACE: u32 = 0x10;

/// All present HID interface paths whose collection matches the Air75 V3
/// vendor pipe: VID 0x19f5, PID 0x1028 and 64-byte reports (65 bytes on
/// Windows including the report ID). Usage page/usage are deliberately not
/// checked: macOS IOKit reports primary usage page 1 / usage 0 for this
/// collection, but Windows HidP_GetCaps reports the same collection as
/// usage page 0 / usage 1 (verified on real hardware). Among the keyboard's
/// six HID collections only the vendor pipe has 65/65 reports, so the
/// report lengths are the discriminator.
fn find_matches() -> Result<Vec<(String, String)>> {
    let Some(api) = hid_api() else {
        return Err(failure("HID_UNAVAILABLE", "Windows HID 服务不可用（hid.dll 加载失败）。"));
    };
    let Some(setup) = setup_api() else {
        return Err(failure("HID_UNAVAILABLE", "Windows 设备枚举不可用（setupapi.dll 加载失败）。"));
    };
    let mut matches = Vec::new();
    unsafe {
        let device_info = (setup.get_class_devs)(&GUID_DEVINTERFACE_HID, std::ptr::null(), 0, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if device_info == INVALID_HANDLE_VALUE {
            return Err(failure("HID_UNAVAILABLE", "Cannot enumerate HID device interfaces."));
        }
        let mut index = 0u32;
        loop {
            let mut interface = SP_DEVICE_INTERFACE_DATA {
                cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                InterfaceClassGuid: GUID_DEVINTERFACE_HID,
                Flags: 0,
                Reserved: 0,
            };
            if (setup.enum_interfaces)(device_info, std::ptr::null(), &GUID_DEVINTERFACE_HID, index, &mut interface) == 0 {
                break;
            }
            index += 1;
            // First call with a null buffer returns the required size.
            let mut required = 0u32;
            let _ = (setup.get_detail)(device_info, &interface, std::ptr::null_mut(), 0, &mut required, std::ptr::null_mut());
            if required == 0 {
                continue;
            }
            let mut detail = vec![0u8; required as usize];
            (detail.as_mut_ptr() as *mut u32).write_unaligned(8); // sizeof(SP_DEVICE_INTERFACE_DETAIL_DATA_W) on x64
            if (setup.get_detail)(device_info, &interface, detail.as_mut_ptr(), required, std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
                continue;
            }
            let path_utf16: Vec<u16> = detail[4..]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&c| c != 0)
                .collect();
            let path = String::from_utf16_lossy(&path_utf16);
            let wide_path = wide(&path);
            // Probe with zero access so a protected interface still reports identity.
            let probe = CreateFileW(wide_path.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(), OPEN_EXISTING, 0, 0);
            if probe == INVALID_HANDLE_VALUE {
                continue;
            }
            let mut attributes = HIDD_ATTRIBUTES { Size: std::mem::size_of::<HIDD_ATTRIBUTES>() as u32, ..Default::default() };
            let identified = (api.get_attributes)(probe, &mut attributes) != 0
                && attributes.VendorID == 0x19f5
                && attributes.ProductID == 0x1028;
            let mut caps_ok = false;
            if identified {
                let mut preparsed: *mut core::ffi::c_void = std::ptr::null_mut();
                if (api.get_preparsed_data)(probe, &mut preparsed) != 0 {
                    let mut caps = HIDP_CAPS::default();
                    caps_ok = (api.get_caps)(preparsed, &mut caps) > 0
                        && caps.InputReportByteLength == 65
                        && caps.OutputReportByteLength == 65;
                    let _ = (api.free_preparsed_data)(preparsed);
                }
            }
            let mut serial = String::new();
            if identified && caps_ok {
                let mut buffer = [0u16; 128];
                if (api.get_serial)(probe, buffer.as_mut_ptr(), (buffer.len() * 2) as u32) != 0 {
                    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
                    serial = String::from_utf16_lossy(&buffer[..end]).trim().to_string();
                }
            }
            CloseHandle(probe);
            if identified && caps_ok {
                matches.push((path, serial));
            }
        }
        (setup.destroy)(device_info);
    }
    Ok(matches)
}

struct Shared {
    replies: Mutex<VecDeque<[u8; 64]>>,
    stop: AtomicBool,
    removed: AtomicBool,
}

pub struct HidTransport {
    write_handle: HANDLE,
    write_event: HANDLE,
    read_handle: HANDLE,
    shared: Arc<Shared>,
    serial: String,
    mutex: HANDLE,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl HidTransport {
    fn disconnected_last_error() -> bool {
        matches!(unsafe { GetLastError() }, ERROR_DEVICE_NOT_CONNECTED | ERROR_NO_SUCH_DEVICE | ERROR_GEN_FAILURE)
    }
}

impl Transport for HidTransport {
    fn exchange(&mut self, frame: &[u8; 64], timeout: Duration) -> Result<[u8; 64]> {
        // Flush already delivered reports. Never resend an uncertain write or
        // send undocumented opcodes to unstick a desynchronized session.
        self.shared.replies.lock().unwrap().clear();
        let mut buffer = [0u8; 65];
        buffer[1..].copy_from_slice(frame); // byte 0 stays the no-report-ID prefix
        unsafe {
            let mut overlapped = OVERLAPPED {
                Internal: 0, InternalHigh: 0, Offset: 0, OffsetHigh: 0,
                hEvent: self.write_event,
            };
            let mut written = 0u32;
            if WriteFile(self.write_handle, buffer.as_ptr(), buffer.len() as u32, &mut written, &mut overlapped) == 0 {
                let error = GetLastError();
                if error == ERROR_IO_PENDING {
                    if WaitForSingleObject(self.write_event, 1000) != 0
                        || GetOverlappedResult(self.write_handle, &mut overlapped, &mut written, 0) == 0
                    {
                        CancelIoEx(self.write_handle, &mut overlapped);
                        if HidTransport::disconnected_last_error() {
                            self.shared.removed.store(true, Ordering::SeqCst);
                            return Err(failure("DISCONNECTED", "Keyboard unplugged during a transaction; backup retained."));
                        }
                        return Err(failure("HID_WRITE_FAILED", "HID output failed; reconnect the cable and close other lighting apps."));
                    }
                } else {
                    if HidTransport::disconnected_last_error() {
                        self.shared.removed.store(true, Ordering::SeqCst);
                        return Err(failure("DISCONNECTED", "Keyboard unplugged during a transaction; backup retained."));
                    }
                    return Err(failure("HID_WRITE_FAILED", format!("HID output failed (0x{error:08x}); reconnect the cable and close other lighting apps.")));
                }
            }
        }
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.shared.removed.load(Ordering::SeqCst) {
                return Err(failure("DISCONNECTED", "Keyboard unplugged during a transaction; backup retained."));
            }
            let reply = self.shared.replies.lock().unwrap().pop_front();
            if let Some(reply) = reply {
                if reply[0] == 0xaa && reply[1] == frame[1] {
                    crate::protocol::validate(&reply, frame[1])?;
                    return Ok(reply);
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(failure(
                    "HID_TIMEOUT",
                    format!("No valid {:02X} response. Stop other HID clients and reconnect; no blind write retry was performed.", frame[1]),
                ));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn close(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        let reader = self.reader.take();
        if let Some(reader) = reader {
            let _ = reader.join();
        }
        unsafe {
            if self.read_handle != 0 && self.read_handle != INVALID_HANDLE_VALUE {
                CloseHandle(self.read_handle);
            }
            if self.write_handle != 0 && self.write_handle != INVALID_HANDLE_VALUE {
                CloseHandle(self.write_handle);
            }
            if self.write_event != 0 && self.write_event != INVALID_HANDLE_VALUE {
                CloseHandle(self.write_event);
            }
            if self.mutex != 0 {
                CloseHandle(self.mutex);
            }
        }
        self.read_handle = 0;
        self.write_handle = 0;
        self.write_event = 0;
        self.mutex = 0;
    }

    fn serial(&self) -> String {
        self.serial.clone()
    }
}

impl Drop for HidTransport {
    fn drop(&mut self) {
        Transport::close(self);
    }
}

/// Acquires the single-instance mutex and opens the one matching Air75 V3.
pub fn open() -> Result<Box<dyn Transport>> {
    let mutex = unsafe {
        let name = wide("Local\\Air75StudioHIDBridge");
        CreateMutexW(std::ptr::null(), 0, name.as_ptr())
    };
    if mutex == 0 {
        return Err(failure("HID_PERMISSION", "Cannot create the bridge single-instance lock."));
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(mutex) };
        return Err(failure("HID_IN_USE", "Another Air75 Studio helper already owns the keyboard connection."));
    }
    let matches = match find_matches() {
        Ok(matches) => matches,
        Err(error) => {
            unsafe { CloseHandle(mutex) };
            return Err(error);
        }
    };
    let (path, serial) = match matches.len() {
        0 => {
            unsafe { CloseHandle(mutex) };
            return Err(failure("DEVICE_NOT_FOUND", "等待 Air75 V3 ANSI：请连接数据线，并将键盘切到有线模式。"));
        }
        1 => matches.into_iter().next().unwrap(),
        _ => {
            unsafe { CloseHandle(mutex) };
            return Err(failure("DEVICE_AMBIGUOUS", "检测到多把 Air75 V3，请仅连接要使用的一把。"));
        }
    };
    let path_wide = wide(&path);
    let open_flags = GENERIC_READ | GENERIC_WRITE;
    unsafe {
        let read_handle = CreateFileW(path_wide.as_ptr(), open_flags, FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(), OPEN_EXISTING, FILE_FLAG_OVERLAPPED, 0);
        if read_handle == INVALID_HANDLE_VALUE {
            let error = GetLastError();
            CloseHandle(mutex);
            return Err(failure("HID_PERMISSION", format!("Cannot open the vendor HID interface (0x{error:08x}). Close NuPhyIO and other lighting apps.")));
        }
        let write_handle = CreateFileW(path_wide.as_ptr(), GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(), OPEN_EXISTING, FILE_FLAG_OVERLAPPED, 0);
        if write_handle == INVALID_HANDLE_VALUE {
            let error = GetLastError();
            CloseHandle(read_handle);
            CloseHandle(mutex);
            return Err(failure("HID_PERMISSION", format!("Cannot open the vendor HID interface for output (0x{error:08x}). Close NuPhyIO and other lighting apps.")));
        }
        let write_event = CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
        if write_event == 0 {
            CloseHandle(write_handle);
            CloseHandle(read_handle);
            CloseHandle(mutex);
            return Err(failure("HID_PERMISSION", "Cannot create the HID write event."));
        }
        let shared = Arc::new(Shared {
            replies: Mutex::new(VecDeque::new()),
            stop: AtomicBool::new(false),
            removed: AtomicBool::new(false),
        });
        // The read handle lives in the reader thread; it blocks in overlapped
        // waits and exits when `stop` is set (or on device removal error).
        let thread_shared = Arc::clone(&shared);
        let reader = std::thread::Builder::new()
            .name("hid-reader".into())
            .spawn(move || reader_loop(read_handle, thread_shared))
            .map_err(|e| {
                CloseHandle(write_handle);
                CloseHandle(read_handle);
                CloseHandle(write_event);
                CloseHandle(mutex);
                failure("HID_PERMISSION", format!("Cannot start the HID reader thread: {e}"))
            })?;
        Ok(Box::new(HidTransport {
            write_handle,
            write_event,
            read_handle: 0, // Ownership moved into the reader thread.
            shared,
            serial,
            mutex,
            reader: Some(reader),
        }))
    }
}

fn reader_loop(handle: HANDLE, shared: Arc<Shared>) {
    unsafe {
        let event = CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
        if event == 0 {
            shared.removed.store(true, Ordering::SeqCst);
            return;
        }
        let mut buffer = [0u8; 65];
        let mut pending = false;
        let mut overlapped = OVERLAPPED { Internal: 0, InternalHigh: 0, Offset: 0, OffsetHigh: 0, hEvent: event };
        loop {
            if shared.stop.load(Ordering::SeqCst) {
                if pending {
                    CancelIoEx(handle, &mut overlapped);
                    let mut transferred = 0u32;
                    let _ = GetOverlappedResult(handle, &mut overlapped, &mut transferred, 1);
                }
                CloseHandle(event);
                return;
            }
            if !pending {
                if ReadFile(handle, buffer.as_mut_ptr(), buffer.len() as u32, std::ptr::null_mut(), &mut overlapped) == 0 {
                    let error = GetLastError();
                    if error != ERROR_IO_PENDING {
                        shared.removed.store(true, Ordering::SeqCst);
                        CloseHandle(event);
                        return;
                    }
                    pending = true;
                } else {
                    deliver(&shared, &buffer);
                }
            }
            if pending {
                let wait = WaitForSingleObject(event, 100);
                if wait == WAIT_TIMEOUT {
                    continue;
                }
                let mut transferred = 0u32;
                if GetOverlappedResult(handle, &mut overlapped, &mut transferred, 0) == 0 {
                    if HidTransport::disconnected_last_error() {
                        shared.removed.store(true, Ordering::SeqCst);
                        CloseHandle(event);
                        return;
                    }
                    pending = false;
                    continue;
                }
                pending = false;
                deliver(&shared, &buffer);
            }
        }
    }
}

fn deliver(shared: &Arc<Shared>, raw: &[u8; 65]) {
    let mut report = [0u8; 64];
    report.copy_from_slice(&raw[1..]);
    let mut replies = shared.replies.lock().unwrap();
    if replies.len() >= 64 {
        replies.pop_front();
    }
    replies.push_back(report);
}
