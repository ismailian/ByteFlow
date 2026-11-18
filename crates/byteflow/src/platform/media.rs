//! Built-in audio/video playback.
//!
//! **Windows**: Media Foundation's Media Engine (the OS's own codecs) in
//! frame-server mode, entirely in-process. A dedicated thread owns the engine
//! and a D3D11 device; each new video frame is scaled on the GPU to the size
//! it is shown at (`TransferVideoFrame`), read back, and handed to the UI as
//! an egui texture. Audio goes to the default output device through the
//! engine. Nothing is handed to another process.
//!
//! Sources ([`Source`]): a local file (opened read-only), or — the normal
//! case since 0.4.0 — a [`ByteSource`] streamed from the bucket. The engine
//! reads a stream through an in-process `IMFByteStream` ([`imp::RemoteStream`])
//! whose reads run on a dedicated thread, so the engine only ever pulls the
//! ranges it needs (the index first, then playback, then wherever the user
//! seeks) instead of waiting for the whole object.
//!
//! Other platforms: [`MediaPlayer::open`] returns `None` and the viewer falls
//! back to opening the file in the default app.

use std::path::PathBuf;
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

/// Random-access bytes for the player (a bucket object, streamed).
pub trait ByteSource: Send + Sync {
    fn len(&self) -> u64;
    /// Blocking read at `off`; short only at the end. Called on the player's
    /// own I/O thread, never the UI thread.
    fn read_at(&self, off: u64, buf: &mut [u8]) -> std::io::Result<usize>;
    /// Make pending and future reads fail fast (the player is closing).
    fn cancel(&self);
}

pub enum Source {
    /// A local file (dev-env viewer and tests; the app streams).
    #[cfg_attr(not(any(test, feature = "dev-env")), allow(dead_code))]
    File(PathBuf),
    /// `name` carries the extension the OS uses to pick a demuxer.
    Stream { source: Arc<dyn ByteSource>, name: String },
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
    /// Waiting for data (start-up, after a seek, or a slow connection).
    pub buffering: bool,
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
    stream: Option<Arc<dyn ByteSource>>,
}

impl MediaPlayer {
    pub fn supported() -> bool {
        cfg!(windows)
    }

    /// Start playing a local file. `None` where built-in playback isn't available.
    #[cfg(any(test, feature = "dev-env"))]
    pub fn open(path: &std::path::Path, repaint: egui::Context) -> Option<Self> {
        Self::open_source(Source::File(path.to_path_buf()), repaint, true)
    }

    /// Start playing `source`; with `autoplay` false it loads paused.
    #[allow(unused_variables)]
    pub fn open_source(source: Source, repaint: egui::Context, autoplay: bool) -> Option<Self> {
        #[cfg(windows)]
        {
            let shared = Arc::new(Shared {
                status: Mutex::new(Status { duration: f64::NAN, paused: true, buffering: true, ..Default::default() }),
                frame: Mutex::new(None),
                repaint,
            });
            let stream = match &source {
                Source::Stream { source, .. } => Some(source.clone()),
                Source::File(_) => None,
            };
            let (tx, rx) = mpsc::channel();
            let s2 = shared.clone();
            let thread = std::thread::Builder::new().name("byteflow-media".into()).spawn(move || imp::run(source, autoplay, rx, &s2)).ok()?;
            Some(Self { tx, shared, thread: Some(thread), viewport: (0, 0), stream })
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
        // Unblock any read in flight first so the engine shuts down promptly.
        if let Some(s) = &self.stream {
            s.cancel();
        }
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
    use std::collections::HashMap;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
    use std::time::Duration;

    use parking_lot::Mutex;

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
    use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, E_NOTIMPL, S_OK};
    use windows::core::{BSTR, IUnknown, Interface, PCWSTR};
    use windows_core::{BOOL, Ref, implement};

    use super::{ByteSource, Cmd, Shared, Source, VIDEO_PLAYING};

    // ------------------------------------------------- streamed byte stream --

    /// Raw COM pointers moved to the I/O thread. The async result is
    /// free-threaded, and the destination buffer stays valid until `EndRead`
    /// (the `IMFByteStream` contract).
    struct Job {
        buf: usize,
        len: u32,
        result: IMFAsyncResult,
    }
    // SAFETY: see above; MF async results may be completed from any thread.
    unsafe impl Send for Job {}

    struct Inner {
        source: Arc<dyn ByteSource>,
        pos: Mutex<u64>,
        /// Bytes read per finished async result (keyed by its pointer).
        done: Mutex<HashMap<usize, u32>>,
    }

    impl Inner {
        fn read(&self, buf: &mut [u8]) -> windows::core::Result<u32> {
            let at = *self.pos.lock();
            let n = self.source.read_at(at, buf).map_err(|_| windows::core::Error::from(E_FAIL))?;
            *self.pos.lock() = at + n as u64;
            Ok(n as u32)
        }
    }

    /// `IMFByteStream` over a [`ByteSource`]: readable, seekable, read-only.
    /// Async reads are queued to one I/O thread, in order.
    #[implement(IMFByteStream)]
    pub(super) struct RemoteStream {
        inner: Arc<Inner>,
        jobs: Mutex<Option<mpsc::Sender<Job>>>,
    }

    impl RemoteStream {
        pub(super) fn create(source: Arc<dyn ByteSource>) -> Option<IMFByteStream> {
            let inner = Arc::new(Inner { source, pos: Mutex::new(0), done: Mutex::new(HashMap::new()) });
            let (tx, rx) = mpsc::channel::<Job>();
            let i2 = inner.clone();
            std::thread::Builder::new()
                .name("byteflow-media-io".into())
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        // SAFETY: the caller keeps `buf` valid until EndRead.
                        let buf = unsafe { std::slice::from_raw_parts_mut(job.buf as *mut u8, job.len as usize) };
                        let status = match i2.read(buf) {
                            Ok(n) => {
                                i2.done.lock().insert(job.result.as_raw() as usize, n);
                                S_OK
                            }
                            Err(e) => e.code(),
                        };
                        unsafe {
                            let _ = job.result.SetStatus(status);
                            let _ = MFInvokeCallback(&job.result);
                        }
                    }
                })
                .ok()?;
            Some(Self { inner, jobs: Mutex::new(Some(tx)) }.into())
        }
    }

    #[allow(non_snake_case)]
    impl IMFByteStream_Impl for RemoteStream_Impl {
        fn GetCapabilities(&self) -> windows::core::Result<u32> {
            Ok(MFBYTESTREAM_IS_READABLE | MFBYTESTREAM_IS_SEEKABLE)
        }
        fn GetLength(&self) -> windows::core::Result<u64> {
            Ok(self.inner.source.len())
        }
        fn SetLength(&self, _len: u64) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }
        fn GetCurrentPosition(&self) -> windows::core::Result<u64> {
            Ok(*self.inner.pos.lock())
        }
        fn SetCurrentPosition(&self, pos: u64) -> windows::core::Result<()> {
            if pos > self.inner.source.len() {
                return Err(E_INVALIDARG.into());
            }
            *self.inner.pos.lock() = pos;
            Ok(())
        }
        fn IsEndOfStream(&self) -> windows::core::Result<BOOL> {
            Ok((*self.inner.pos.lock() >= self.inner.source.len()).into())
        }
        fn Read(&self, pb: *mut u8, cb: u32, pcbread: *mut u32) -> windows::core::Result<()> {
            if pb.is_null() || pcbread.is_null() {
                return Err(E_INVALIDARG.into());
            }
            // SAFETY: MF passes a writable buffer of `cb` bytes.
            let n = self.inner.read(unsafe { std::slice::from_raw_parts_mut(pb, cb as usize) })?;
            unsafe { *pcbread = n };
            Ok(())
        }
        fn BeginRead(&self, pb: *mut u8, cb: u32, pcallback: Ref<IMFAsyncCallback>, punkstate: Ref<IUnknown>) -> windows::core::Result<()> {
            if pb.is_null() {
                return Err(E_INVALIDARG.into());
            }
            let result = unsafe { MFCreateAsyncResult(None, pcallback.as_ref(), punkstate.as_ref())? };
            let tx = self.jobs.lock().clone().ok_or_else(|| windows::core::Error::from(E_FAIL))?;
            tx.send(Job { buf: pb as usize, len: cb, result }).map_err(|_| windows::core::Error::from(E_FAIL))
        }
        fn EndRead(&self, presult: Ref<IMFAsyncResult>) -> windows::core::Result<u32> {
            let r = presult.ok()?;
            let n = self.inner.done.lock().remove(&(r.as_raw() as usize));
            unsafe { r.GetStatus()? };
            n.ok_or_else(|| E_FAIL.into())
        }
        fn Write(&self, _pb: *const u8, _cb: u32) -> windows::core::Result<u32> {
            Err(E_NOTIMPL.into())
        }
        fn BeginWrite(&self, _pb: *const u8, _cb: u32, _c: Ref<IMFAsyncCallback>, _s: Ref<IUnknown>) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }
        fn EndWrite(&self, _r: Ref<IMFAsyncResult>) -> windows::core::Result<u32> {
            Err(E_NOTIMPL.into())
        }
        fn Seek(&self, origin: MFBYTESTREAM_SEEK_ORIGIN, offset: i64, _flags: u32) -> windows::core::Result<u64> {
            let mut pos = self.inner.pos.lock();
            let base = if origin == msoCurrent { *pos as i64 } else { 0 };
            let new = base.checked_add(offset).filter(|n| *n >= 0).ok_or_else(|| windows::core::Error::from(E_INVALIDARG))? as u64;
            *pos = new.min(self.inner.source.len());
            Ok(*pos)
        }
        fn Flush(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn Close(&self) -> windows::core::Result<()> {
            // Ends the I/O thread once queued reads drain.
            self.jobs.lock().take();
            Ok(())
        }
    }

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

    pub(super) fn run(source: Source, autoplay: bool, rx: Receiver<Cmd>, shared: &std::sync::Arc<Shared>) {
        // SAFETY: COM/MF init and teardown are balanced on this thread.
        unsafe {
            let com = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
            let mf = MFStartup(MF_VERSION, MFSTARTUP_FULL).is_ok();
            if mf {
                if let Err(msg) = session(source, autoplay, &rx, shared) {
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

    unsafe fn session(source: Source, autoplay: bool, rx: &Receiver<Cmd>, shared: &std::sync::Arc<Shared>) -> Result<(), &'static str> {
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

            match source {
                Source::File(path) => {
                    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
                    let opened = MFCreateFile(MF_ACCESSMODE_READ, MF_OPENMODE_FAIL_IF_NOT_EXIST, MF_FILEFLAGS_NONE, PCWSTR(wide.as_ptr()))
                        .and_then(|stream| engine.cast::<IMFMediaEngineEx>().map(|ex| (ex, stream)))
                        .and_then(|(ex, stream)| ex.SetSourceFromByteStream(&stream, &BSTR::from_wide(&wide[..wide.len() - 1])));
                    if opened.is_err() {
                        engine.SetSource(&BSTR::from_wide(&wide[..wide.len() - 1])).map_err(|_| "This file couldn't be opened for playback.")?;
                    }
                }
                Source::Stream { source, name } => {
                    const OPEN: &str = "This file couldn't be opened for playback.";
                    let stream = RemoteStream::create(source).ok_or(FAIL)?;
                    let ex = engine.cast::<IMFMediaEngineEx>().map_err(|_| FAIL)?;
                    // The "URL" only names the stream: its extension picks the demuxer.
                    let url: String = name.chars().map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' }).collect();
                    ex.SetSourceFromByteStream(&stream, &BSTR::from(format!("byteflow-stream/{url}"))).map_err(|_| OPEN)?;
                }
            }
            if autoplay {
                let _ = engine.Play();
            }

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
            let ready = unsafe { engine.GetReadyState() };
            let buffering = error.is_none() && !ended && (seeking || ready < MF_MEDIA_ENGINE_READY_HAVE_FUTURE_DATA.0 as u16);
            let mut native = (0u32, 0u32);
            if has_video {
                unsafe {
                    let _ = engine.GetNativeVideoSize(Some(&mut native.0), Some(&mut native.1));
                }
            }
            {
                let mut s = shared.status.lock();
                let changed = s.paused != paused
                    || s.ended != ended
                    || s.has_video != has_video
                    || s.buffering != buffering
                    || (position - last_pos).abs() > 0.2;
                s.buffering = buffering;
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

    /// A network-like source: 40 ms per request, 8 MB/s.
    struct Throttled(bf_app::stream::MemoryFetch);
    impl bf_app::stream::RangeFetch for Throttled {
        fn fetch(&self, start: u64, len: u64) -> futures::future::BoxFuture<'_, bf_core::AppResult<bf_app::stream::Bytes>> {
            let inner = self.0.fetch(start, len);
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(40) + Duration::from_secs_f64(len as f64 / 8e6)).await;
                inner.await
            })
        }
    }

    /// Stream `path` through the in-process byte stream, as the app streams
    /// a bucket object: encrypted with the vault format and served from
    /// memory in ranged reads, decrypted per segment.
    fn streamed(path: &str, encrypted: bool) -> (MediaPlayer, std::sync::Arc<bf_app::stream::MediaStream>, tokio::runtime::Runtime) {
        use bf_core::crypto::file_crypto::{DEFAULT_CHUNK_SIZE, encrypt_file, no_control};
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let key = bf_core::crypto::secure_bytes::random_bytes(32);
        let bytes = if encrypted {
            let d = std::env::temp_dir().join(format!("bf-media-test-{}", std::process::id()));
            encrypt_file(Path::new(path), &d, &key, DEFAULT_CHUNK_SIZE, None, &mut no_control()).unwrap();
            let b = std::fs::read(&d).unwrap();
            let _ = std::fs::remove_file(&d);
            b
        } else {
            std::fs::read(path).unwrap()
        };
        let len = bytes.len() as u64;
        let fetch = std::sync::Arc::new(Throttled(bf_app::stream::MemoryFetch(bytes)));
        let master = encrypted.then_some(&key[..]);
        let s = rt.block_on(bf_app::stream::MediaStream::new(fetch, len, master, tokio_util::sync::CancellationToken::new())).unwrap();
        let name = Path::new(path).file_name().unwrap().to_string_lossy().into_owned();
        let src = super::Source::Stream { source: std::sync::Arc::new(crate::ui::viewer::StreamSource(s.clone())), name };
        let pl = MediaPlayer::open_source(src, eframe::egui::Context::default(), true).expect("player");
        (pl, s, rt)
    }

    /// `BYTEFLOW_TEST_MEDIA=<video.mp4> cargo test --release -p byteflow -- --ignored streamed`
    #[test]
    #[ignore = "needs BYTEFLOW_TEST_MEDIA pointing at a video with audio"]
    fn streamed_encrypted_video_plays_and_seeks() {
        let path = std::env::var("BYTEFLOW_TEST_MEDIA").expect("BYTEFLOW_TEST_MEDIA");
        let size = std::fs::metadata(&path).unwrap().len();
        let (mut pl, stream, _rt) = streamed(&path, true);
        pl.set_viewport(640, 360);
        pl.set_muted(true);
        let start = Instant::now();
        let mut frames = 0;
        let mut first_frame = None;
        while start.elapsed() < Duration::from_secs(15) && frames < 30 {
            if pl.take_frame().is_some() {
                frames += 1;
                first_frame.get_or_insert(start.elapsed());
            }
            assert!(pl.status().error.is_none(), "{:?}", pl.status().error);
            std::thread::sleep(Duration::from_millis(10));
        }
        let st = pl.status();
        assert!(st.ready && st.has_video && st.has_audio, "{st:?}");
        assert!(frames >= 30, "only {frames} frames");
        let fetched_at_start = stream.fetched();
        let first = first_frame.unwrap();
        eprintln!("first frame after {first:?}; fetched {fetched_at_start} of {size} bytes");
        // Streaming: the picture starts well before the whole object could
        // have arrived at 8 MB/s.
        let full = Duration::from_secs_f64(size as f64 / 8e6);
        assert!(first < full / 2, "first frame {first:?} vs full download {full:?}");

        // Seek near the end: playback resumes there without reading it all.
        pl.seek(st.duration - 2.5);
        std::thread::sleep(Duration::from_millis(1500));
        let p = pl.status().position;
        assert!((p - (st.duration - 2.5)).abs() < 1.5, "position after seek {p}");
        let before = frames;
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) && frames < before + 10 {
            if pl.take_frame().is_some() {
                frames += 1;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(frames >= before + 10, "no frames after the seek");
        let t = Instant::now();
        drop(pl);
        assert!(t.elapsed() < Duration::from_secs(3), "slow shutdown {:?}", t.elapsed());
        assert!(!super::video_playing());
    }

    /// `BYTEFLOW_TEST_AUDIO=<song.mp3> cargo test --release -p byteflow -- --ignored streamed`
    #[test]
    #[ignore = "needs BYTEFLOW_TEST_AUDIO pointing at an audio file"]
    fn streamed_audio_plays() {
        let path = std::env::var("BYTEFLOW_TEST_AUDIO").expect("BYTEFLOW_TEST_AUDIO");
        for encrypted in [false, true] {
            let (pl, _s, _rt) = streamed(&path, encrypted);
            pl.set_muted(true);
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(10) && pl.status().position < 1.0 {
                assert!(pl.status().error.is_none(), "{:?}", pl.status().error);
                std::thread::sleep(Duration::from_millis(20));
            }
            let st = pl.status();
            assert!(st.ready && st.has_audio && !st.has_video, "{st:?}");
            assert!(st.position >= 1.0, "not advancing: {st:?}");
            assert!(!super::video_playing(), "audio isn't video");
            pl.seek(st.duration * 0.8);
            std::thread::sleep(Duration::from_millis(800));
            assert!(pl.status().position >= st.duration * 0.75, "{:?}", pl.status());
        }
    }
}
