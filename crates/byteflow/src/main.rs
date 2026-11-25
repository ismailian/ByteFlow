//! ByteFlow Desktop — entry point.
//!
//! Threads: the UI thread runs egui only; a multi-threaded tokio runtime runs
//! all network and disk I/O; CPU-bound work goes to tokio's blocking pool and
//! rayon. The UI never blocks on I/O.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod bidi;
mod ctx;
#[cfg(feature = "dev-env")]
mod demo;
mod platform;
mod theme;
mod ui;

use std::sync::{Arc, OnceLock};

use eframe::egui;

use bf_app::Notifier;

/// Repaint bridge: services ping this from any thread.
struct EguiNotifier(Arc<OnceLock<egui::Context>>);

impl Notifier for EguiNotifier {
    fn changed(&self) {
        if let Some(c) = self.0.get() {
            c.request_repaint();
        }
    }
}

fn main() -> eframe::Result<()> {
    // Release builds abort on panic and have no console: say what happened
    // instead of vanishing.
    std::panic::set_hook(Box::new(|info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown error".into());
        let at = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        platform::crash_dialog(&format!(
            "ByteFlow hit an unexpected error and has to close.\n\n{msg}\n{at}\n\nTransfers resume the next time you unlock."
        ));
    }));
    let egui_ctx: Arc<OnceLock<egui::Context>> = Arc::new(OnceLock::new());

    // Single instance: a second launch focuses the first and exits.
    let focus_ctx = egui_ctx.clone();
    let Some(instance) = platform::instance::acquire(move || {
        if let Some(c) = focus_ctx.get() {
            c.request_repaint();
        }
    }) else {
        return Ok(());
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(std::thread::available_parallelism().map(|n| n.get().clamp(2, 8)).unwrap_or(4))
        .thread_name("byteflow-io")
        .enable_all()
        .build()
        .expect("tokio runtime");

    // dev-env builds only: run against a throwaway vault directory.
    let dev_root = cfg!(feature = "dev-env").then(|| std::env::var_os("BYTEFLOW_DEV_DATA_DIR")).flatten().map(std::path::PathBuf::from);
    let paths = match dev_root {
        Some(root) => bf_data::paths::AppPaths::at(root.join("support"), root.join("cache"), root.join("temp")),
        None => bf_data::paths::AppPaths::system(),
    };
    let paths = match paths {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cannot create app directories: {e}");
            std::process::exit(1);
        }
    };
    // Stale plaintext from a crashed preview session never survives a launch.
    // "updates": a downloaded installer, already applied (or abandoned).
    for sub in ["enc_work", "enc_preview", "preview", "updates"] {
        let _ = std::fs::remove_dir_all(paths.temp.join(sub));
    }

    let placement = platform::placement::Placement::load(paths.window_state());
    let notifier = Arc::new(EguiNotifier(egui_ctx.clone()));
    let core = match runtime.block_on(bf_app::core::Core::open(paths, notifier)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot open app storage: {}", e.developer_message());
            std::process::exit(1);
        }
    };

    #[cfg(feature = "dev-env")]
    if demo::enabled() {
        runtime.block_on(demo::seed(&core));
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(if cfg!(feature = "dev-env") { concat!("ByteFlow ", env!("CARGO_PKG_VERSION"), " (dev-env)") } else { "ByteFlow" })
            // Hidden until the first frame sizes and positions it
            // (platform::placement: centred 70% × 80% first, then the last
            // position and size).
            .with_visible(false)
            .with_maximized(false)
            .with_inner_size([1320.0, 820.0])
            .with_min_inner_size(platform::placement::MIN_SIZE)
            .with_drag_and_drop(true)
            .with_icon(Arc::new(app_icon())),
        persist_window: false,
        ..Default::default()
    };

    let rt = runtime.handle().clone();
    let result = eframe::run_native(
        "ByteFlow",
        options,
        Box::new(move |cc| {
            let _ = egui_ctx.set(cc.egui_ctx.clone());
            Ok(Box::new(app::ByteFlowApp::new(cc, rt, core, instance, placement)))
        }),
    );
    runtime.shutdown_timeout(std::time::Duration::from_secs(3));
    result
}

/// Window/taskbar icon (the exe's embedded icon is for Explorer and shortcuts;
/// eframe would otherwise show its own default). Drawn by scripts/make-icon.ps1.
fn app_icon() -> egui::IconData {
    let img = image::load_from_memory_with_format(include_bytes!("../assets/icon/byteflow.png"), image::ImageFormat::Png)
        .expect("bundled icon is a valid PNG")
        .into_rgba8();
    let (width, height) = img.dimensions();
    egui::IconData { rgba: img.into_raw(), width, height }
}
