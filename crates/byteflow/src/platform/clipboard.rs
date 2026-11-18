//! The only clipboard write the app makes: copying generated share links
//! (spec §3.7, §12). Presigned URLs are secrets, so on Windows the copy is
//! marked as excluded from clipboard history, cloud clipboard and clipboard
//! monitors, and it is auto-cleared after a timeout if still ours
//! (**[DESKTOP decision]**, §17.1).

use std::time::Duration;

#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    unsafe fn put_bytes(format: u32, data: &[u8]) -> bool {
        unsafe {
            let h = GlobalAlloc(GMEM_MOVEABLE, data.len().max(1));
            if h.is_null() {
                return false;
            }
            let p = GlobalLock(h) as *mut u8;
            if p.is_null() {
                GlobalFree(h);
                return false;
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            GlobalUnlock(h);
            if SetClipboardData(format, h as HANDLE).is_null() {
                GlobalFree(h);
                return false;
            }
            true
        }
    }

    fn open() -> bool {
        // The clipboard can be briefly held by another process; retry.
        for _ in 0..10 {
            // SAFETY: plain Win32 call.
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        false
    }

    /// Returns the clipboard sequence number after the write.
    pub fn set_private_text(text: &str) -> Option<u32> {
        if !open() {
            return None;
        }
        // SAFETY: clipboard is open for this thread; buffers outlive calls.
        unsafe {
            EmptyClipboard();
            let w = wide(text);
            let bytes = std::slice::from_raw_parts(w.as_ptr() as *const u8, w.len() * 2);
            let ok = put_bytes(CF_UNICODETEXT as u32, bytes);
            let zero = 0u32.to_le_bytes();
            let excl = RegisterClipboardFormatW(wide("ExcludeClipboardContentFromMonitorProcessing").as_ptr());
            let hist = RegisterClipboardFormatW(wide("CanIncludeInClipboardHistory").as_ptr());
            let cloud = RegisterClipboardFormatW(wide("CanUploadToCloudClipboard").as_ptr());
            if excl != 0 {
                put_bytes(excl, &zero);
            }
            if hist != 0 {
                put_bytes(hist, &zero);
            }
            if cloud != 0 {
                put_bytes(cloud, &zero);
            }
            CloseClipboard();
            ok.then(|| GetClipboardSequenceNumber())
        }
    }

    /// Clear only if the clipboard still holds what we put there.
    pub fn clear_if_unchanged(seq: u32) {
        // SAFETY: plain Win32 calls.
        unsafe {
            if GetClipboardSequenceNumber() != seq || !open() {
                return;
            }
            EmptyClipboard();
            CloseClipboard();
        }
    }
}

/// Copy a share link. Falls back to egui's clipboard on other platforms.
pub fn copy_share_link(ctx: &eframe::egui::Context, text: &str, clear_after: Duration) {
    #[cfg(windows)]
    {
        if let Some(seq) = win::set_private_text(text) {
            if !clear_after.is_zero() {
                std::thread::spawn(move || {
                    std::thread::sleep(clear_after);
                    win::clear_if_unchanged(seq);
                });
            }
            return;
        }
    }
    let _ = clear_after;
    ctx.copy_text(text.to_string());
}
