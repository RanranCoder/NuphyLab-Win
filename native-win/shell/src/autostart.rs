// Per-user autostart through the HKCU Run key — the Windows equivalent of a
// login item. The stored command is the absolute path of this executable
// (quoted, since install paths contain spaces); nothing else is launched.
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_READ, KEY_SET_VALUE, REG_SZ,
};

use crate::win::wide;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "NuPhyLab";

/// True when the stored Run command points at exactly this executable.
/// Compared without quotes and case-insensitively: the value may have been
/// written by hand, by an installer, or by a previous install location.
pub fn run_value_matches(stored: &str, exe: &str) -> bool {
    fn strip(value: &str) -> &str {
        value.trim().trim_matches('"')
    }
    strip(stored).eq_ignore_ascii_case(strip(exe))
}

fn open_key(access: u32) -> Result<HKEY, String> {
    let subkey = wide(RUN_KEY);
    let mut key: HKEY = std::ptr::null_mut();
    let result = unsafe {
        RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, access, &mut key)
    };
    if result != ERROR_SUCCESS {
        return Err(format!("打开注册表 Run 键失败（{result}）"));
    }
    Ok(key)
}

pub fn read_run_value() -> Option<String> {
    let key = open_key(KEY_READ).ok()?;
    let name = wide(VALUE_NAME);
    let mut kind = 0u32;
    let mut size = 0u32;
    let result = unsafe {
        RegQueryValueExW(key, name.as_ptr(), std::ptr::null_mut(), &mut kind, std::ptr::null_mut(), &mut size)
    };
    if result != ERROR_SUCCESS || kind != REG_SZ || size == 0 {
        unsafe { RegCloseKey(key) };
        return None;
    }
    let mut buffer = vec![0u16; (size as usize / 2).max(1)];
    let value = unsafe {
        let result = RegQueryValueExW(key, name.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), buffer.as_mut_ptr() as *mut u8, &mut size);
        RegCloseKey(key);
        if result != ERROR_SUCCESS {
            return None;
        }
        String::from_utf16_lossy(&buffer[..size as usize / 2])
    };
    let trimmed = value.trim_end_matches('\0').to_string();
    if trimmed.is_empty() { None } else { Some(trimmed) }
}

pub fn current_exe_path() -> Option<String> {
    std::env::current_exe().ok().map(|path| path.to_string_lossy().into_owned())
}

pub fn enabled_for(exe: &str) -> bool {
    read_run_value().map(|stored| run_value_matches(&stored, exe)).unwrap_or(false)
}

/// Enables or disables autostart for this executable. Returns a message on
/// failure instead of panicking: the tray toggle reports it and stays usable.
pub fn set(enabled: bool, exe: &str) -> Result<(), String> {
    if !enabled {
        let key = open_key(KEY_SET_VALUE)?;
        let name = wide(VALUE_NAME);
        let result = unsafe { RegDeleteValueW(key, name.as_ptr()) };
        unsafe { RegCloseKey(key) };
        // Deleting an absent value is the desired end state, not a failure.
        if result != ERROR_SUCCESS && result != 2 {
            return Err(format!("移除自启项失败（{result}）"));
        }
        return Ok(());
    }
    let key = open_key(KEY_SET_VALUE | KEY_READ)?;
    let name = wide(VALUE_NAME);
    // The path is quoted: install locations routinely contain spaces.
    let command = wide(&format!("\"{exe}\""));
    let bytes = command.len() * 2;
    let result = unsafe {
        RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, command.as_ptr() as *const u8, bytes as u32)
    };
    unsafe { RegCloseKey(key) };
    if result != ERROR_SUCCESS {
        return Err(format!("写入自启项失败（{result}）"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_commands_are_matched_without_quotes_or_case() {
        let exe = r"C:\Users\zhang\AppData\Local\Programs\NuPhyLab\NuPhyLab.exe";
        assert!(run_value_matches(&format!("\"{exe}\""), exe));
        assert!(run_value_matches(exe, exe));
        assert!(run_value_matches(&format!("  \"{exe}\" "), exe));
        assert!(run_value_matches(&format!("\"{}\"", exe.to_uppercase()), exe));
        assert!(!run_value_matches(&format!("\"{exe}\" --minimized"), exe));
        assert!(!run_value_matches(r"D:\Elsewhere\NuPhyLab.exe", exe));
        assert!(!run_value_matches("", exe));
    }
}
