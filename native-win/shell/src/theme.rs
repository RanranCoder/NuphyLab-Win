// OS theme (dark/light) and accent color, read from the registry and pushed
// into the WebView as data attributes + CSS variables. The page never polls:
// a watcher thread re-reads every few seconds (registry reads are cheap) and
// only sends on change.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OsTheme {
    pub dark: bool,
    pub accent: String,
}

const PERSONALIZE_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
const LIGHT_THEME_VALUE: &str = "AppsUseLightTheme";
const DWM_KEY: &str = "Software\\Microsoft\\Windows\\DWM";
const COLORIZATION_VALUE: &str = "ColorizationColor";
// Windows 11 default accent; used when the DWM value is missing.
const DEFAULT_ACCENT: &str = "#0067c0";

/// DWM stores colors as 0xAABBGGRR; lighting and CSS want #RRGGBB.
pub fn abgr_to_hex(value: u32) -> String {
    format!("#{:02x}{:02x}{:02x}", value & 0xff, (value >> 8) & 0xff, (value >> 16) & 0xff)
}

fn read_dword(subkey: &str, name: &str) -> Option<u32> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
    };
    use crate::win::wide;
    let mut key: HKEY = std::ptr::null_mut();
    let subkey = wide(subkey);
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut key) } != ERROR_SUCCESS {
        return None;
    }
    let name = wide(name);
    let mut kind = 0u32;
    let mut value = 0u32;
    let mut size = 4u32;
    let result = unsafe {
        let result = RegQueryValueExW(key, name.as_ptr(), std::ptr::null_mut(), &mut kind, &mut value as *mut u32 as *mut u8, &mut size);
        RegCloseKey(key);
        result
    };
    (result == ERROR_SUCCESS && kind == REG_DWORD).then_some(value)
}

pub fn read() -> OsTheme {
    OsTheme {
        // AppsUseLightTheme = 0 means apps prefer dark.
        dark: read_dword(PERSONALIZE_KEY, LIGHT_THEME_VALUE).map(|value| value == 0).unwrap_or(false),
        accent: read_dword(DWM_KEY, COLORIZATION_VALUE).map(abgr_to_hex).unwrap_or_else(|| DEFAULT_ACCENT.to_string()),
    }
}

/// Applies the OS theme facts to the page. The final theme is the UI's call:
/// settings.appearance may be light/dark regardless of the OS, so the shell
/// only publishes the system value (`osThemeSystem`) and the accent color.
/// The page recalculates on the `nuphylab:os-theme` event.
pub fn apply_script(theme: &OsTheme) -> String {
    let mode = if theme.dark { "dark" } else { "light" };
    format!(
        "(function(d){{d.documentElement.dataset.osThemeSystem='{mode}';d.documentElement.style.setProperty('--accent','{accent}');d.dispatchEvent(new Event('nuphylab:os-theme'));}})(document);",
        mode = mode,
        accent = theme.accent,
    )
}

/// Watcher thread: reads the registry every 4 s and notifies on change only.
pub fn watch(proxy: tao::event_loop::EventLoopProxy<crate::UserEvent>) {
    std::thread::spawn(move || {
        let mut last = read();
        let _ = proxy.send_event(crate::UserEvent::OsThemeChanged { dark: last.dark, accent: last.accent.clone() });
        loop {
            std::thread::sleep(std::time::Duration::from_secs(4));
            let current = read();
            if current != last {
                last = current.clone();
                let _ = proxy.send_event(crate::UserEvent::OsThemeChanged { dark: current.dark, accent: current.accent });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abgr_swizzle_yields_rrggbb() {
        assert_eq!(abgr_to_hex(0xff_ff0000), "#0000ff");
        assert_eq!(abgr_to_hex(0x00_102030), "#302010");
        assert_eq!(abgr_to_hex(0xff_0067c0), "#c06700");
    }

    #[test]
    fn apply_script_publishes_system_facts_not_the_final_theme() {
        let script = apply_script(&OsTheme { dark: true, accent: "#c06700".to_string() });
        assert!(script.contains("osThemeSystem='dark'"));
        assert!(script.contains("--accent','#c06700'"));
        // The final theme belongs to settings.appearance; the shell must not
        // overwrite it from the OS on every poll.
        assert!(!script.contains("osTheme='"));
        let light = apply_script(&OsTheme { dark: false, accent: "#0067c0".to_string() });
        assert!(light.contains("osThemeSystem='light'"));
    }
}
