//! Single instance (spec §17.3). Two instances would both run engines over
//! the same queue rows and race on the state files, so a second launch
//! focuses the existing window and exits.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct InstanceGuard {
    #[cfg(windows)]
    _mutex: isize,
    #[cfg(not(windows))]
    _file: std::fs::File,
    /// Set when another launch asked us to come to the front.
    pub focus_requested: Arc<AtomicBool>,
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }
    const MUTEX: &str = "Local\\ByteFlowDesktop.Instance";
    const EVENT: &str = "Local\\ByteFlowDesktop.Focus";

    pub fn acquire(on_focus: impl Fn() + Send + 'static) -> Option<InstanceGuard> {
        // SAFETY: plain Win32 calls with valid wide strings.
        unsafe {
            let m = CreateMutexW(std::ptr::null(), 1, wide(MUTEX).as_ptr());
            if m.is_null() {
                return None;
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let e = OpenEventW(EVENT_MODIFY_STATE, 0, wide(EVENT).as_ptr());
                if !e.is_null() {
                    SetEvent(e);
                }
                return None;
            }
            let ev = CreateEventW(std::ptr::null(), 0, 0, wide(EVENT).as_ptr());
            let flag = Arc::new(AtomicBool::new(false));
            if !ev.is_null() {
                let (f2, ev_addr) = (flag.clone(), ev as usize);
                std::thread::Builder::new()
                    .name("instance-focus".into())
                    .spawn(move || loop {
                        let r = WaitForSingleObject(ev_addr as _, INFINITE);
                        if r != 0 {
                            return;
                        }
                        f2.store(true, Ordering::SeqCst);
                        on_focus();
                    })
                    .ok();
            }
            Some(InstanceGuard { _mutex: m as isize, focus_requested: flag })
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub fn acquire(_on_focus: impl Fn() + Send + 'static) -> Option<InstanceGuard> {
        let dir = dirs_lock_dir();
        let _ = std::fs::create_dir_all(&dir);
        let f = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(dir.join("instance.lock")).ok()?;
        f.try_lock().ok()?;
        Some(InstanceGuard { _file: f, focus_requested: Arc::new(AtomicBool::new(false)) })
    }
    fn dirs_lock_dir() -> std::path::PathBuf {
        std::env::temp_dir().join("byteflow")
    }
}

/// `None` → another instance is running (and has been asked to focus).
pub fn acquire(on_focus: impl Fn() + Send + 'static) -> Option<InstanceGuard> {
    imp::acquire(on_focus)
}
