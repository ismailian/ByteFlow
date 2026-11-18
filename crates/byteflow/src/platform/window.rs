//! Screen-capture blocking (spec §3.7, §17.1).
//!
//! Windows: `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` — the window is
//! excluded from screenshots, screen recording, screen sharing, and the
//! taskbar/Alt-Tab live previews. macOS/Linux: no general equivalent is wired
//! here (the privacy overlay still applies).

use raw_window_handle::{HasWindowHandle, RawWindowHandle};

pub fn exclude_from_capture(handle: &impl HasWindowHandle) -> bool {
    // dev-env builds only (never shipped): allow screenshots for UI work.
    if cfg!(feature = "dev-env") && std::env::var_os("BYTEFLOW_DEV_ALLOW_CAPTURE").is_some() {
        return true;
    }
    let Ok(h) = handle.window_handle() else { return false };
    match h.as_raw() {
        #[cfg(windows)]
        RawWindowHandle::Win32(w) => {
            use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE};
            let hwnd = w.hwnd.get() as windows_sys::Win32::Foundation::HWND;
            // SAFETY: hwnd is the live window handle owned by eframe.
            unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) != 0 }
        }
        _ => false,
    }
}
