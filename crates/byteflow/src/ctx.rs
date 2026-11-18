//! Shared UI context: the runtime handle, the auth controller, async task
//! handles the UI polls each frame, and toasts.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use parking_lot::Mutex;

use bf_app::auth::AuthController;
use bf_app::core::Core;
use bf_app::session::Session;

/// The result slot of a background future. The UI polls it each frame; the
/// future pings a repaint when it finishes.
pub struct Task<T>(Arc<Mutex<Option<T>>>);

impl<T> Clone for Task<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: Send + 'static> Task<T> {
    /// Take the result once it's ready.
    pub fn poll(&self) -> Option<T> {
        self.0.lock().take()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warn,
    Error,
}

pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub until: Instant,
}

#[derive(Clone, Default)]
pub struct Toasts(Arc<Mutex<Vec<Toast>>>);

impl Toasts {
    pub fn push(&self, kind: ToastKind, text: impl Into<String>) {
        let secs = if matches!(kind, ToastKind::Error | ToastKind::Warn) { 7 } else { 4 };
        self.0.lock().push(Toast { text: text.into(), kind, until: Instant::now() + Duration::from_secs(secs) });
    }
    pub fn info(&self, t: impl Into<String>) {
        self.push(ToastKind::Info, t)
    }
    pub fn ok(&self, t: impl Into<String>) {
        self.push(ToastKind::Success, t)
    }
    pub fn warn(&self, t: impl Into<String>) {
        self.push(ToastKind::Warn, t)
    }
    pub fn error(&self, t: impl Into<String>) {
        self.push(ToastKind::Error, t)
    }
    pub fn take_live(&self) -> Vec<(String, ToastKind)> {
        let mut g = self.0.lock();
        let now = Instant::now();
        g.retain(|t| t.until > now);
        g.iter().map(|t| (t.text.clone(), t.kind)).collect()
    }
    pub fn clear(&self) {
        self.0.lock().clear();
    }
}

#[derive(Clone)]
pub struct AppCtx {
    pub rt: tokio::runtime::Handle,
    pub auth: Arc<AuthController>,
    pub core: Arc<Core>,
    pub egui: egui::Context,
    pub toasts: Toasts,
}

impl AppCtx {
    pub fn spawn(&self, f: impl Future<Output = ()> + Send + 'static) {
        let ctx = self.egui.clone();
        self.rt.spawn(async move {
            f.await;
            ctx.request_repaint();
        });
    }

    pub fn task<T: Send + 'static>(&self, f: impl Future<Output = T> + Send + 'static) -> Task<T> {
        let slot = Arc::new(Mutex::new(None));
        let (s2, ctx) = (slot.clone(), self.egui.clone());
        self.rt.spawn(async move {
            let v = f.await;
            *s2.lock() = Some(v);
            ctx.request_repaint();
        });
        Task(slot)
    }

    pub fn session(&self) -> Option<Arc<Session>> {
        self.auth.session()
    }

    pub fn prefs(&self) -> bf_app::prefs::Prefs {
        self.core.prefs.get()
    }
}

pub fn secret(s: &str) -> bf_core::crypto::SecretBytes {
    zeroize::Zeroizing::new(s.as_bytes().to_vec())
}

/// Clear a text buffer that held a secret.
pub fn wipe_string(s: &mut String) {
    zeroize::Zeroize::zeroize(s);
}
