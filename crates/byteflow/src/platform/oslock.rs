//! OS session-lock / sleep detection → immediate vault lock
//! (**[DESKTOP decision]**, §17.1 auto-lock trigger).
//!
//! Windows: the input desktop is not `Default` while the workstation is
//! locked (or the secure desktop is up). A wall-clock jump much larger than
//! the poll interval means the machine slept. Polled once a second on a
//! dedicated thread — negligible cost.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

#[cfg(windows)]
fn workstation_locked() -> bool {
    use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, DESKTOP_READOBJECTS, GetUserObjectInformationW, OpenInputDesktop, UOI_NAME};
    // SAFETY: plain Win32 calls; buffer sized for the name.
    unsafe {
        let d = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
        if d.is_null() {
            return true;
        }
        let mut buf = [0u16; 64];
        let mut needed = 0u32;
        let ok = GetUserObjectInformationW(d as _, UOI_NAME, buf.as_mut_ptr() as _, (buf.len() * 2) as u32, &mut needed);
        CloseDesktop(d);
        if ok == 0 {
            return false;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        !String::from_utf16_lossy(&buf[..len]).eq_ignore_ascii_case("Default")
    }
}

#[cfg(not(windows))]
fn workstation_locked() -> bool {
    false
}

/// Spawns the watcher; `fired` is set on lock/sleep and `wake` is called.
pub fn spawn(wake: impl Fn() + Send + 'static) -> Arc<AtomicBool> {
    let fired = Arc::new(AtomicBool::new(false));
    let f2 = fired.clone();
    std::thread::Builder::new()
        .name("os-lock-watch".into())
        .spawn(move || {
            let mut was_locked = false;
            let mut last_wall = SystemTime::now();
            let mut last_mono = Instant::now();
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let wall_gap = SystemTime::now().duration_since(last_wall).unwrap_or_default();
                let mono_gap = last_mono.elapsed();
                last_wall = SystemTime::now();
                last_mono = Instant::now();
                let slept = wall_gap > mono_gap + Duration::from_secs(5) || mono_gap > Duration::from_secs(10);
                let locked = workstation_locked();
                if (locked && !was_locked) || slept {
                    f2.store(true, Ordering::SeqCst);
                    wake();
                }
                was_locked = locked;
            }
        })
        .ok();
    fired
}
