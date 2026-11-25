//! Desktop platform adaptations (spec §17).

pub mod clipboard;
pub mod instance;
pub mod media;
pub mod oslock;
pub mod placement;
pub mod update;
pub mod window;

use std::path::Path;

/// Open a local file with the OS default application.
pub fn open_with_default_app(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let verb: Vec<u16> = "open\0".encode_utf16().collect();
        // SAFETY: valid NUL-terminated wide strings for the call duration.
        let r = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), wide.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
        r as isize > 32
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(path).spawn().is_ok()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(path).spawn().is_ok()
    }
}

/// Last-resort error dialog (from the panic hook). No logging, no disk.
pub fn crash_dialog(text: &str) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MB_TOPMOST, MessageBoxW};
        let body: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let title: Vec<u16> = "ByteFlow".encode_utf16().chain(Some(0)).collect();
        // SAFETY: valid NUL-terminated wide strings for the call duration.
        unsafe {
            MessageBoxW(std::ptr::null_mut(), body.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR | MB_TOPMOST);
        }
    }
    #[cfg(not(windows))]
    eprintln!("{text}");
}

/// Reveal a folder in the file manager.
pub fn open_folder(path: &Path) -> bool {
    open_with_default_app(path)
}
