//! Built-in audio/video playback.
//!
//! **Windows**: Media Foundation's Media Engine (the OS's own codecs) in
//! frame-server mode, entirely in-process. A dedicated thread owns the engine
//! and a D3D11 device; each new video frame is scaled on the GPU to the size
//! it is shown at (`TransferVideoFrame`), read back, and handed to the UI as
//! an egui texture. Audio goes to the default output device through the
//! engine. The file is opened read-only from the app's private temp folder;
//! nothing is handed to another process.
//!
//! Other platforms: [`MediaPlayer::open`] returns `None` and the viewer falls
//! back to opening the file in the default app.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use eframe::egui;
use parking_lot::Mutex;

/// True while a video (not audio-only) is actually playing. The app treats
/// that as user activity while the window is focused, so the idle auto-lock
/// doesn't stop a film halfway through.
static VIDEO_PLAYING: AtomicBool = AtomicBool::new(false);

pub fn video_playing() -> bool {
    VIDEO_PLAYING.load(Ordering::Relaxed)
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    /// Metadata loaded (duration, streams and size are known).
    pub ready: bool,
    pub has_video: bool,
    pub has_audio: bool,
    pub video_size: [u32; 2],
    /// Seconds; NaN while unknown.
    pub duration: f64,
    pub position: f64,
    pub paused: bool,
    pub ended: bool,
    pub seeking: bool,
    /// Static copy; never raw system text.
    pub error: Option<String>,
}

enum Cmd {
    Play,
    Pause,
    Seek(f64),
    Volume(f64),
    Muted(bool),
    Viewport(u32, u32),
    Stop,
}

struct Shared {
    status: Mutex<Status>,
    frame: Mutex<Option<egui::ColorImage>>,
    repaint: egui::Context,
}

pub struct MediaPlayer {
    tx: mpsc::Sender<Cmd>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    viewport: (u32, u32),
}

impl MediaPlayer {
    pub fn supported() -> bool {
        cfg!(windows)
    }

    /// Start playing `path`. `None` where built-in playback isn't available.
    #[allow(unused_variables)]
    pub fn open(path: &Path, repaint: egui::Context) -> Option<Self> {
        #[cfg(windows)]
        {
            let shared = Arc::new(Shared {
                status: Mutex::new(Status { duration: f64::NAN, paused: true, ..Default::default() }),
                frame: Mutex::new(None),
                repaint,
            });
            let (tx, rx) = mpsc::channel();
            let (s2, p2) = (shared.clone(), path.to_path_buf());
            let thread = std::thread::Builder::new().name("byteflow-media".into()).spawn(move || imp::run(&p2, rx, &s2)).ok()?;
            Some(Self { tx, shared, thread: Some(thread), viewport: (0, 0) })
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    pub fn status(&self) -> Status {
        self.shared.status.lock().clone()
    }

    pub fn take_frame(&self) -> Option<egui::ColorImage> {
        self.shared.frame.lock().take()
    }

    pub fn play(&self) {
        let _ = self.tx.send(Cmd::Play);
    }
    pub fn pause(&self) {
        let _ = self.tx.send(Cmd::Pause);
    }
    pub fn seek(&self, secs: f64) {
        self.shared.status.lock().position = secs;
        let _ = self.tx.send(Cmd::Seek(secs));
    }
    pub fn set_volume(&self, v: f64) {
        let _ = self.tx.send(Cmd::Volume(v.clamp(0.0, 1.0)));
    }
    pub fn set_muted(&self, m: bool) {
        let _ = self.tx.send(Cmd::Muted(m));
    }

    /// The on-screen size of the video in physical pixels; frames are scaled
    /// to it on the GPU so readback stays small.
    pub fn set_viewport(&mut self, w: u32, h: u32) {
        let (ow, oh) = self.viewport;
        let far = |a: u32, b: u32| (a as f32 - b as f32).abs() > (b as f32 * 0.08).max(8.0);
        if far(w, ow) || far(h, oh) {
            self.viewport = (w, h);
            let _ = self.tx.send(Cmd::Viewport(w, h));
        }
    }
}

impl Drop for MediaPlayer {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
        // Join so the file handle is closed before the temp copy is deleted.
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        VIDEO_PLAYING.store(false, Ordering::Relaxed);
    }
}

#[cfg(windows)]
mod imp {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    use std::time::Duration;

    use eframe::egui::{self, Color32};
    use windows::Win32::Foundation::{HMODULE, RECT};
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_READ,
        D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11CreateDevice,
        ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};
    use windows::Win32::System::Power::{ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState};
    use windows::core::{BSTR, Interface, PCWSTR};
    use windows_core::implement;

    use super::{Cmd, Shared, VIDEO_PLAYING};

    /// Largest frame ever read back (GPU-scaled down to this).
    const MAX_W: u32 = 2560;
    const MAX_H: u32 = 1440;

    #[implement(IMFMediaEngineNotify)]
    struct Notify {
        shared: std::sync::Arc<Shared>,
    }

    impl IMFMediaEngineNotify_Impl for Notify_Impl {
        fn EventNotify(&self, event: u32, _param1: usize, _param2: u32) -> windows::core::Result<()> {
            let e = MF_MEDIA_ENGINE_EVENT(event as i32);
            if e == MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA {
                self.shared.status.lock().ready = true;
            }
            // Every event is a state change worth a repaint.
            self.shared.repaint.request_repaint();
            Ok(())
        }
    }

    pub(super) fn run(path: &Path, rx: Receiver<Cmd>, shared: &std::sync::Arc<Shared>) {
        // SAFETY: COM/MF init and teardown are balanced on this thread.
        unsafe {
            let com = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
            let mf = MFStartup(MF_VERSION, MFSTARTUP_FULL).is_ok();
            if mf {
                if let Err(msg) = session(path, &rx, shared) {
                    shared.status.lock().error = Some(msg.into());
                    shared.repaint.request_repaint();
                    // Keep the thread (and so the player) alive until closed.
                    while !matches!(rx.recv(), Ok(Cmd::Stop) | Err(_)) {}
                }
                let _ = MFShutdown();
            } else {
                shared.status.lock().error = Some("Media playback isn't available on this computer.".into());
                shared.repaint.request_repaint();
                while !matches!(rx.recv(), Ok(Cmd::Stop) | Err(_)) {}
            }
            SetThreadExecutionState(ES_CONTINUOUS);
            VIDEO_PLAYING.store(false, Ordering::Relaxed);
            if com {
                CoUninitialize();
            }
        }
    }

    unsafe fn device(kind: D3D_DRIVER_TYPE) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
        let (mut dev, mut ctx) = (None, None);
        unsafe {
            D3D11CreateDevice(
                None,
                kind,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                Some(&mut ctx),
            )?;
        }
        Ok((dev.expect("device"), ctx.expect("context")))
    }

    struct Targets {
        size: (u32, u32),
        render: ID3D11Texture2D,
        staging: ID3D11Texture2D,
    }

    unsafe fn targets(dev: &ID3D11Device, w: u32, h: u32) -> windows::core::Result<Targets> {
        let mut desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let (mut render, mut staging) = (None, None);
        unsafe {
            dev.CreateTexture2D(&desc, None, Some(&mut render))?;
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            dev.CreateTexture2D(&desc, None, Some(&mut staging))?;
        }
        Ok(Targets { size: (w, h), render: render.expect("render"), staging: staging.expect("staging") })
    }

    /// Fit `native` inside the viewport (and the readback cap), keeping aspect.
    fn target_size(native: (u32, u32), viewport: (u32, u32)) -> (u32, u32) {
        let (nw, nh) = (native.0.max(1) as f32, native.1.max(1) as f32);
        let (mut bw, mut bh) = (MAX_W as f32, MAX_H as f32);
        if viewport.0 > 16 && viewport.1 > 16 {
            bw = bw.min(viewport.0 as f32);
            bh = bh.min(viewport.1 as f32);
        }
        let s = (bw / nw).min(bh / nh).min(1.0);
        (((nw * s).round() as u32).max(2) & !1, ((nh * s).round() as u32).max(2) & !1)
    }

    fn error_text(code: u16) -> &'static str {
        match MF_MEDIA_ENGINE_ERR(code as i32) {
            MF_MEDIA_ENGINE_ERR_SRC_NOT_SUPPORTED => "This format isn't supported by the media codecs installed on this computer.",
            MF_MEDIA_ENGINE_ERR_DECODE => "This file couldn't be decoded. It may be damaged or use an unsupported codec.",
            _ => "This file couldn't be played.",
        }
    }

    unsafe fn session(path: &Path, rx: &Receiver<Cmd>, shared: &std::sync::Arc<Shared>) -> Result<(), &'static str> {
        const FAIL: &str = "Media playback couldn't start on this computer.";
        unsafe {
            let (dev, ctx) = device(D3D_DRIVER_TYPE_HARDWARE).or_else(|_| device(D3D_DRIVER_TYPE_WARP)).map_err(|_| FAIL)?;
            if let Ok(mt) = ctx.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(|_| FAIL)?;
            let manager = manager.ok_or(FAIL)?;
            manager.ResetDevice(&dev, token).map_err(|_| FAIL)?;

            let notify: IMFMediaEngineNotify = Notify { shared: shared.clone() }.into();
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 3).map_err(|_| FAIL)?;
            let attrs = attrs.ok_or(FAIL)?;
            attrs.SetUnknown(&MF_MEDIA_ENGINE_DXGI_MANAGER, &manager).map_err(|_| FAIL)?;
            attrs.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify).map_err(|_| FAIL)?;
            attrs.SetUINT32(&MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32).map_err(|_| FAIL)?;

            let factory: IMFMediaEngineClassFactory = CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER).map_err(|_| FAIL)?;
            let engine = factory.CreateInstance(0, &attrs).map_err(|_| FAIL)?;

            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let opened = MFCreateFile(MF_ACCESSMODE_READ, MF_OPENMODE_FAIL_IF_NOT_EXIST, MF_FILEFLAGS_NONE, PCWSTR(wide.as_ptr()))
                .and_then(|stream| engine.cast::<IMFMediaEngineEx>().map(|ex| (ex, stream)))
                .and_then(|(ex, stream)| ex.SetSourceFromByteStream(&stream, &BSTR::from_wide(&wide[..wide.len() - 1])));
            if opened.is_err() {
                engine.SetSource(&BSTR::from_wide(&wide[..wide.len() - 1])).map_err(|_| "This file couldn't be opened for playback.")?;
            }
            let _ = engine.Play();

            let result = pump(&engine, &dev, &ctx, rx, shared);
            let _ = engine.Shutdown();
            result
        }
    }

    unsafe fn pump(engine: &IMFMediaEngine, dev: &ID3D11Device, ctx: &ID3D11DeviceContext, rx: &Receiver<Cmd>, shared: &std::sync::Arc<Shared>) -> Result<(), &'static str> {
        let mut viewport = (0u32, 0u32);
        let mut tex: Option<Targets> = None;
        let mut awake = false;
        let mut last_pos = f64::NAN;
        loop {
            // Commands (also the frame pacing: ~200 Hz poll).
            let cmd = match rx.recv_timeout(Duration::from_millis(5)) {
                Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
                Ok(cmd) => Some(cmd),
                Err(RecvTimeoutError::Timeout) => None,
            };
            if let Some(cmd) = cmd {
                unsafe {
                    match cmd {
                        Cmd::Play => {
                            let _ = engine.Play();
                        }
                        Cmd::Pause => {
                            let _ = engine.Pause();
                        }
                        Cmd::Seek(t) => {
                            let _ = engine.SetCurrentTime(t.max(0.0));
                        }
                        Cmd::Volume(v) => {
                            let _ = engine.SetVolume(v);
                        }
                        Cmd::Muted(m) => {
                            let _ = engine.SetMuted(m);
                        }
                        Cmd::Viewport(w, h) => {
                            viewport = (w, h);
                            tex = None; // re-created at the new size
                        }
                        Cmd::Stop => {}
                    }
                }
                shared.repaint.request_repaint();
            }

            // State.
            let (has_video, has_audio, paused, ended, seeking, duration, position) = unsafe {
                (
                    engine.HasVideo().as_bool(),
                    engine.HasAudio().as_bool(),
                    engine.IsPaused().as_bool(),
                    engine.IsEnded().as_bool(),
                    engine.IsSeeking().as_bool(),
                    engine.GetDuration(),
                    engine.GetCurrentTime(),
                )
            };
            let error = unsafe { engine.GetError().ok().map(|e| e.GetErrorCode()).filter(|c| *c != 0) };
            let mut native = (0u32, 0u32);
            if has_video {
                unsafe {
                    let _ = engine.GetNativeVideoSize(Some(&mut native.0), Some(&mut native.1));
                }
            }
            {
                let mut s = shared.status.lock();
                let changed = s.paused != paused || s.ended != ended || s.has_video != has_video || (position - last_pos).abs() > 0.2;
                s.has_video = has_video;
                s.has_audio = has_audio;
                s.paused = paused;
                s.ended = ended;
                s.seeking = seeking;
                s.duration = duration;
                if !seeking {
                    s.position = position;
                }
                s.video_size = [native.0, native.1];
                if let Some(code) = error {
                    s.error = Some(error_text(code).into());
                }
                if changed {
                    last_pos = position;
                    shared.repaint.request_repaint();
                }
            }

            // Keep the display awake only while a video is actually playing.
            let playing_video = has_video && !paused && !ended;
            VIDEO_PLAYING.store(playing_video, Ordering::Relaxed);
            if playing_video != awake {
                awake = playing_video;
                unsafe {
                    SetThreadExecutionState(if awake { ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED } else { ES_CONTINUOUS });
                }
            }

            if !has_video || native.0 == 0 || native.1 == 0 {
                continue;
            }
            // New frame due?
            let mut pts = 0i64;
            // SAFETY: raw vtable call so S_FALSE ("no new frame") is told apart from S_OK.
            let hr = unsafe { (Interface::vtable(engine).OnVideoStreamTick)(Interface::as_raw(engine), &mut pts) };
            if hr.0 != 0 {
                continue;
            }
            let want = target_size(native, viewport);
            if tex.as_ref().is_none_or(|t| t.size != want) {
                tex = unsafe { targets(dev, want.0, want.1).ok() };
            }
            let Some(t) = tex.as_ref() else { continue };
            let dst = RECT { left: 0, top: 0, right: want.0 as i32, bottom: want.1 as i32 };
            let black = MFARGB { rgbBlue: 0, rgbGreen: 0, rgbRed: 0, rgbAlpha: 255 };
            if unsafe { engine.TransferVideoFrame(&t.render, None, &dst, Some(&black)) }.is_err() {
                continue;
            }
            if let Some(img) = unsafe { read_back(ctx, t) } {
                *shared.frame.lock() = Some(img);
                shared.repaint.request_repaint();
            }
        }
    }

    unsafe fn read_back(ctx: &ID3D11DeviceContext, t: &Targets) -> Option<egui::ColorImage> {
        let (w, h) = (t.size.0 as usize, t.size.1 as usize);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            ctx.CopyResource(&t.staging, &t.render);
            ctx.Map(&t.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).ok()?;
        }
        let pitch = mapped.RowPitch as usize;
        let mut px = Vec::with_capacity(w * h);
        // SAFETY: the mapped staging texture is `pitch * h` bytes, BGRA8.
        let data = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, pitch * h) };
        for row in data.chunks_exact(pitch).take(h) {
            for c in row[..w * 4].chunks_exact(4) {
                px.push(Color32::from_rgb(c[2], c[1], c[0]));
            }
        }
        unsafe {
            ctx.Unmap(&t.staging, 0);
        }
        Some(egui::ColorImage::new([w, h], px))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::MediaPlayer;

    /// `BYTEFLOW_TEST_MEDIA=<file.mp4> cargo test --release -p byteflow -- --ignored media`
    #[test]
    #[ignore = "needs BYTEFLOW_TEST_MEDIA pointing at a short video with audio"]
    fn media_engine_plays_seeks_and_produces_frames() {
        let path = std::env::var("BYTEFLOW_TEST_MEDIA").expect("BYTEFLOW_TEST_MEDIA");
        let mut pl = MediaPlayer::open(Path::new(&path), eframe::egui::Context::default()).expect("player");
        pl.set_viewport(640, 360);
        pl.set_muted(true);
        let start = Instant::now();
        let mut frames = 0;
        let mut size = [0usize; 2];
        while start.elapsed() < Duration::from_secs(10) && frames < 20 {
            if let Some(f) = pl.take_frame() {
                frames += 1;
                size = f.size;
            }
            assert!(pl.status().error.is_none(), "{:?}", pl.status().error);
            std::thread::sleep(Duration::from_millis(10));
        }
        let st = pl.status();
        assert!(st.ready, "metadata never loaded");
        assert!(st.has_video && st.has_audio, "{st:?}");
        assert!(st.duration > 1.0, "{st:?}");
        assert!(frames >= 20, "only {frames} frames");
        // GPU-scaled to the viewport, aspect kept (16:9 source).
        assert_eq!(size, [640, 360], "frame size {size:?}");
        assert!(super::video_playing());

        pl.pause();
        std::thread::sleep(Duration::from_millis(300));
        assert!(pl.status().paused);
        pl.seek(2.0);
        std::thread::sleep(Duration::from_millis(600));
        let p = pl.status().position;
        assert!((p - 2.0).abs() < 0.5, "position after seek {p}");
        drop(pl);
        assert!(!super::video_playing());
        // The file handle is closed once the player is dropped.
        std::fs::OpenOptions::new().write(true).open(&path).expect("file still locked after drop");
    }
}
