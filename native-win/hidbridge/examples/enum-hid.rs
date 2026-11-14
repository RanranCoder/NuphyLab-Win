// One-off diagnostic: enumerate every present HID interface and print
// VID/PID/usage/caps so the Air75 V3 filter in winhid.rs can be compared
// against what Windows actually reports. DLLs are loaded at runtime, the
// same way winhid.rs does it. Not part of the app.
#![allow(non_snake_case, non_camel_case_types)]

type HANDLE = isize;
const INVALID_HANDLE_VALUE: HANDLE = -1;

#[repr(C)]
#[derive(Clone, Copy)]
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
    fn CloseHandle(handle: HANDLE) -> i32;
    fn GetLastError() -> u32;
}

type HidD_GetAttributesFn = unsafe extern "system" fn(HANDLE, *mut HIDD_ATTRIBUTES) -> i32;
type HidD_GetPreparsedDataFn = unsafe extern "system" fn(HANDLE, *mut *mut core::ffi::c_void) -> i32;
type HidD_FreePreparsedDataFn = unsafe extern "system" fn(*mut core::ffi::c_void) -> i32;
type HidP_GetCapsFn = unsafe extern "system" fn(*mut core::ffi::c_void, *mut HIDP_CAPS) -> i32;
type SetupDiGetClassDevsFn = unsafe extern "system" fn(*const GUID, *const u16, HANDLE, u32) -> HANDLE;
type SetupDiEnumDeviceInterfacesFn = unsafe extern "system" fn(HANDLE, *const core::ffi::c_void, *const GUID, u32, *mut SP_DEVICE_INTERFACE_DATA) -> i32;
type SetupDiGetDeviceInterfaceDetailFn = unsafe extern "system" fn(HANDLE, *const SP_DEVICE_INTERFACE_DATA, *mut u8, u32, *mut u32, *mut core::ffi::c_void) -> i32;
type SetupDiDestroyDeviceInfoListFn = unsafe extern "system" fn(HANDLE) -> i32;

const DIGCF_PRESENT: u32 = 0x2;
const DIGCF_DEVICEINTERFACE: u32 = 0x10;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn load(module: HANDLE, name: &str) -> Option<*mut core::ffi::c_void> {
    let symbol = std::ffi::CString::new(name).ok()?;
    let address = unsafe { GetProcAddress(module, symbol.as_ptr() as *const u8) };
    if address.is_null() { None } else { Some(address) }
}

fn main() {
    let hid_module = unsafe { LoadLibraryW(wide("hid.dll").as_ptr()) };
    if hid_module == 0 {
        println!("hid.dll load failed: {}", unsafe { GetLastError() });
        return;
    }
    let setup_module = unsafe { LoadLibraryW(wide("setupapi.dll").as_ptr()) };
    if setup_module == 0 {
        println!("setupapi.dll load failed: {}", unsafe { GetLastError() });
        return;
    }
    unsafe {
        let get_attributes: HidD_GetAttributesFn = std::mem::transmute_copy(&load(hid_module, "HidD_GetAttributes").unwrap());
        let get_preparsed: HidD_GetPreparsedDataFn = std::mem::transmute_copy(&load(hid_module, "HidD_GetPreparsedData").unwrap());
        let free_preparsed: HidD_FreePreparsedDataFn = std::mem::transmute_copy(&load(hid_module, "HidD_FreePreparsedData").unwrap());
        let get_caps: HidP_GetCapsFn = std::mem::transmute_copy(&load(hid_module, "HidP_GetCaps").unwrap());
        let get_class_devs: SetupDiGetClassDevsFn = std::mem::transmute_copy(&load(setup_module, "SetupDiGetClassDevsW").unwrap());
        let enum_interfaces: SetupDiEnumDeviceInterfacesFn = std::mem::transmute_copy(&load(setup_module, "SetupDiEnumDeviceInterfaces").unwrap());
        let get_detail: SetupDiGetDeviceInterfaceDetailFn = std::mem::transmute_copy(&load(setup_module, "SetupDiGetDeviceInterfaceDetailW").unwrap());
        let destroy: SetupDiDestroyDeviceInfoListFn = std::mem::transmute_copy(&load(setup_module, "SetupDiDestroyDeviceInfoList").unwrap());

        let device_info = get_class_devs(&GUID_DEVINTERFACE_HID, std::ptr::null(), 0, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if device_info == INVALID_HANDLE_VALUE {
            println!("SetupDiGetClassDevsW failed: {}", GetLastError());
            return;
        }
        let mut index = 0u32;
        loop {
            let mut interface = SP_DEVICE_INTERFACE_DATA {
                cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                InterfaceClassGuid: GUID_DEVINTERFACE_HID,
                Flags: 0,
                Reserved: 0,
            };
            if enum_interfaces(device_info, std::ptr::null(), &GUID_DEVINTERFACE_HID, index, &mut interface) == 0 {
                break;
            }
            index += 1;
            let mut required = 0u32;
            let _ = get_detail(device_info, &interface, std::ptr::null_mut(), 0, &mut required, std::ptr::null_mut());
            if required == 0 {
                continue;
            }
            let mut detail = vec![0u8; required as usize];
            (detail.as_mut_ptr() as *mut u32).write_unaligned(8);
            if get_detail(device_info, &interface, detail.as_mut_ptr(), required, std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
                println!("[{}] get detail failed: 0x{:08x}", index, GetLastError());
                continue;
            }
            let path_utf16: Vec<u16> = detail[4..]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&c| c != 0)
                .collect();
            let path = String::from_utf16_lossy(&path_utf16);
            if !path.to_ascii_lowercase().contains("vid_19f5") {
                continue;
            }
            let wide_path = wide(&path);
            let probe = CreateFileW(wide_path.as_ptr(), 0, 3, std::ptr::null(), 3, 0, 0);
            if probe == INVALID_HANDLE_VALUE {
                println!("{}: open failed 0x{:08x}", path, GetLastError());
                continue;
            }
            let mut attributes = HIDD_ATTRIBUTES { Size: std::mem::size_of::<HIDD_ATTRIBUTES>() as u32, VendorID: 0, ProductID: 0, VersionNumber: 0 };
            if get_attributes(probe, &mut attributes) == 0 {
                println!("{}: HidD_GetAttributes failed", path);
                CloseHandle(probe);
                continue;
            }
            let mut caps = HIDP_CAPS::default();
            let mut caps_text = String::from("preparsed failed");
            let mut preparsed: *mut core::ffi::c_void = std::ptr::null_mut();
            if get_preparsed(probe, &mut preparsed) != 0 {
                if get_caps(preparsed, &mut caps) > 0 {
                    caps_text = format!(
                        "UsagePage={:#06x} Usage={:#06x} In={} Out={} Feature={}",
                        caps.UsagePage, caps.Usage, caps.InputReportByteLength, caps.OutputReportByteLength, caps.FeatureReportByteLength
                    );
                }
                free_preparsed(preparsed);
            }
            let filter_hit = caps.UsagePage == 1 && caps.Usage == 0
                && caps.InputReportByteLength == 65 && caps.OutputReportByteLength == 65;
            println!(
                "VID={:04x} PID={:04x} ver={:04x} | {} | filter_hit={} | {}",
                attributes.VendorID, attributes.ProductID, attributes.VersionNumber, caps_text, filter_hit, path
            );
            CloseHandle(probe);
        }
        destroy(device_info);
    }
}
