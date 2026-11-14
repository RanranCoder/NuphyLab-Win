// Diagnostic probe: why does LoadLibraryW("hid.dll") fail at runtime?
extern "system" {
    fn LoadLibraryW(name: *const u16) -> isize;
    fn GetProcAddress(module: isize, name: *const u8) -> *mut core::ffi::c_void;
    fn GetLastError() -> u32;
}

fn main() {
    for dll in ["hid.dll", "setupapi.dll", "user32.dll"] {
        let name: Vec<u16> = dll.encode_utf16().chain(std::iter::once(0)).collect();
        let module = unsafe { LoadLibraryW(name.as_ptr()) };
        println!("{dll}: module={module} err={}", unsafe { GetLastError() });
        if module != 0 {
            for symbol in ["HidD_GetAttributes", "HidP_GetCaps", "SetupDiGetClassDevsW"] {
                let symbol = std::ffi::CString::new(symbol).unwrap();
                let address = unsafe { GetProcAddress(module, symbol.as_ptr() as *const u8) };
                println!("  {} = {address:p}", symbol.to_string_lossy());
            }
        }
    }
}
