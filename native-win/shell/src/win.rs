// Raw Win32 facilities for the shell: job objects, shell/URL launching,
// message boxes and screen metrics. Signatures follow windows-sys (flat
// unsafe functions) so no COM-style generic parameters creep in.
use std::io;
use std::os::windows::io::AsRawHandle;
use std::process::Child;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, RECT};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, MessageBoxW, SystemParametersInfoW, IDYES, MB_DEFBUTTON2,
    MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    MB_YESNO, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SPI_GETWORKAREA, SW_SHOWNORMAL,
};

pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Owns a job-object handle. Children assigned to this job are killed by the
/// kernel when the handle closes (including when the shell process dies), so
/// a crashed or force-killed shell can never leave node.exe orphaned.
pub struct JobObject(HANDLE);

impl JobObject {
    pub fn kill_on_close() -> io::Result<Self> {
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            unsafe { CloseHandle(handle) };
            return Err(io::Error::last_os_error());
        }
        Ok(JobObject(handle))
    }

    pub fn assign(&self, child: &Child) -> io::Result<()> {
        let process = child.as_raw_handle() as HANDLE;
        let ok = unsafe { AssignProcessToJobObject(self.0, process) };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Blocks until the process exits and returns its exit code. None if the
/// process handle cannot be opened (already gone and reaped elsewhere).
pub fn wait_process_exit(pid: u32) -> Option<i32> {
    unsafe {
        let process = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        WaitForSingleObject(process, u32::MAX);
        let mut code: u32 = 0;
        let have_code = GetExitCodeProcess(process, &mut code);
        CloseHandle(process);
        if have_code == 0 {
            None
        } else {
            Some(code as i32)
        }
    }
}

fn shell_execute_raw(file: &[u16], parameters: Option<&[u16]>, directory: Option<&[u16]>) -> bool {
    let empty = wide("");
    let verb = wide("open");
    unsafe {
        let result = ShellExecuteW(
            ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            parameters.map(|p| p.as_ptr()).unwrap_or(empty.as_ptr()),
            directory.map(|d| d.as_ptr()).unwrap_or(empty.as_ptr()),
            SW_SHOWNORMAL as i32,
        );
        result as isize > 32
    }
}

/// Opens a URL (or any shell verb target) with the default handler.
pub fn shell_open(target: &str) -> bool {
    shell_execute_raw(&wide(target), None, None)
}

/// Opens `url` in a specific browser executable (Chrome / Edge preference for
/// drive.nuphy.io, mirroring the macOS bundle-ID preference).
pub fn open_with_browser(exe_path: &str, url: &str) -> bool {
    shell_execute_raw(&wide(exe_path), Some(&wide(url)), None)
}

pub fn open_settings(deep_link: &str) -> bool {
    shell_execute_raw(&wide(deep_link), None, None)
}

/// Blocking system message box, shown top-most. Runs its own message pump on
/// the calling thread; call from a worker thread when the main loop must keep
/// servicing the webview.
pub fn alert(text: &str, caption: &str, warning: bool) {
    let style = MB_TOPMOST | MB_SETFOREGROUND | MB_OK | if warning { MB_ICONWARNING } else { MB_ICONINFORMATION };
    unsafe {
        MessageBoxW(ptr::null_mut(), wide(text).as_ptr(), wide(caption).as_ptr(), style);
    }
}

/// Yes/No question. Returns true for Yes.
pub fn ask_yes_no(text: &str, caption: &str) -> bool {
    let style = MB_TOPMOST | MB_SETFOREGROUND | MB_YESNO | MB_ICONINFORMATION | MB_DEFBUTTON2;
    let result = unsafe { MessageBoxW(ptr::null_mut(), wide(text).as_ptr(), wide(caption).as_ptr(), style) };
    result == IDYES
}

pub fn show_in_explorer(path: &str) {
    let argument = format!("/select,\"{}\"", path.replace('"', ""));
    let _ = shell_execute_raw(&wide("explorer.exe"), Some(&wide(&argument)), None);
}

pub struct WorkArea {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// Primary monitor work area (excludes taskbar), in physical pixels.
pub fn primary_work_area() -> Option<WorkArea> {
    unsafe {
        let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut rect as *mut RECT as *mut core::ffi::c_void, 0) == 0 {
            return None;
        }
        Some(WorkArea { left: rect.left, top: rect.top, right: rect.right, bottom: rect.bottom })
    }
}

/// Union bounding box of all monitors: (left, top, width, height).
pub fn virtual_screen() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

pub fn downloads_dir() -> std::path::PathBuf {
    if let Some(home) = std::env::var_os("USERPROFILE") {
        let dir = std::path::PathBuf::from(home).join("Downloads");
        if dir.is_dir() {
            return dir;
        }
    }
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}
