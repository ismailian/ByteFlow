//! Main-window placement: the first launch opens centred at 70% × 80% of the
//! screen; every later launch reopens at the position and size the window
//! had (in its normal state) when the app last closed.
//!
//! The window starts hidden; [`Placement::frame`] sizes and positions it on
//! the first frame that knows the monitor, then shows it. A saved rect that
//! is no longer on a connected monitor (unplugged screen, resolution change)
//! falls back to centred. Maximised/minimised/fullscreen geometry is never
//! recorded (Windows reports a minimised window at -32000,-32000).
//!
//! Stored as plain text (`x y w h`, egui points) in the support folder:
//! no eframe persistence, nothing else written.

use std::path::PathBuf;

use eframe::egui::{self, Pos2, Rect, Vec2, ViewportCommand};

/// Smallest size the window may be restored to (matches `with_min_inner_size`).
pub const MIN_SIZE: Vec2 = Vec2::new(980.0, 600.0);

pub struct Placement {
    file: PathBuf,
    saved: Option<Rect>,
    placed: bool,
    frames: u32,
    /// Latest normal-state (outer position, inner size).
    last: Option<Rect>,
    written: Option<Rect>,
}

impl Placement {
    pub fn load(file: PathBuf) -> Self {
        let saved = std::fs::read_to_string(&file).ok().and_then(|s| parse(&s));
        Self { file, saved, placed: false, frames: 0, last: None, written: None }
    }

    /// Call once per frame.
    pub fn frame(&mut self, ctx: &egui::Context) {
        if !self.placed {
            self.place(ctx);
            return;
        }
        let rect = ctx.input(|i| {
            let v = i.viewport();
            let normal = !v.minimized.unwrap_or(false) && !v.maximized.unwrap_or(false) && !v.fullscreen.unwrap_or(false);
            match (normal, v.outer_rect, v.inner_rect) {
                (true, Some(o), Some(inner)) if o.min.x > -10_000.0 => Some(Rect::from_min_size(o.min, inner.size())),
                _ => None,
            }
        });
        if rect.is_some() {
            self.last = rect;
        }
    }

    fn place(&mut self, ctx: &egui::Context) {
        self.frames += 1;
        let monitor = ctx.input(|i| i.viewport().monitor_size);
        let Some(monitor) = monitor else {
            // Never stay hidden: after a few frames show at the default size.
            if self.frames > 10 {
                ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                self.placed = true;
            }
            ctx.request_repaint();
            return;
        };
        let default_size = Vec2::new(monitor.x * 0.7, monitor.y * 0.8).max(MIN_SIZE).min(monitor);
        let centred = Rect::from_min_size(((monitor - default_size) * 0.5).to_pos2(), default_size);
        let rect = self
            .saved
            .map(|r| Rect::from_min_size(r.min, r.size().max(MIN_SIZE)))
            .filter(|r| is_on_screen(ctx, *r))
            .unwrap_or(centred);
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(rect.size()));
        ctx.send_viewport_cmd(ViewportCommand::OuterPosition(rect.min));
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        self.placed = true;
        ctx.request_repaint();
    }

    /// Write the last normal-state rect (on exit, and whenever it settles).
    pub fn save(&mut self) {
        if let Some(r) = self.last
            && self.written != Some(r)
        {
            let text = format!("{} {} {} {}", r.min.x.round(), r.min.y.round(), r.width().round(), r.height().round());
            if std::fs::write(&self.file, text).is_ok() {
                self.written = Some(r);
            }
        }
    }
}

fn parse(s: &str) -> Option<Rect> {
    let v: Vec<f32> = s.split_whitespace().map(str::parse).collect::<Result<_, _>>().ok()?;
    let [x, y, w, h] = v[..] else { return None };
    (w.is_finite() && h.is_finite() && x.is_finite() && y.is_finite() && w > 0.0 && h > 0.0)
        .then(|| Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h)))
}

/// The title-bar strip must be on a connected monitor so the user can grab it.
fn is_on_screen(ctx: &egui::Context, r: Rect) -> bool {
    let title = Rect::from_min_size(r.min, egui::vec2(r.width(), 40.0));
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromRect};
        let ppp = ctx.native_pixels_per_point().unwrap_or(1.0);
        let rc = RECT {
            left: (title.min.x * ppp) as i32,
            top: (title.min.y * ppp) as i32,
            right: (title.max.x * ppp) as i32,
            bottom: (title.max.y * ppp) as i32,
        };
        // SAFETY: plain value call; RECT lives for the call.
        unsafe { !MonitorFromRect(&rc, MONITOR_DEFAULTTONULL).is_null() }
    }
    #[cfg(not(windows))]
    {
        ctx.input(|i| i.viewport().monitor_size).is_some_and(|m| Rect::from_min_size(Pos2::ZERO, m).intersects(title))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_saved_rect() {
        assert_eq!(parse("10 20 1300 800"), Some(Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(1300.0, 800.0))));
        assert_eq!(parse("-1500 0 1000 700").map(|r| r.min.x), Some(-1500.0));
        assert_eq!(parse("1 2 3"), None);
        assert_eq!(parse("a b c d"), None);
        assert_eq!(parse("0 0 0 700"), None);
    }
}
