//! The only sanctioned log sink (spec §3.7). `debug`/`info` are dropped in
//! release builds. Callers pass already-redacted strings.

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

type Sink = Box<dyn Fn(Level, &str) + Send + Sync>;
static SINK: OnceLock<Sink> = OnceLock::new();

/// Optionally install a sink (tests use this). Default: stderr.
pub fn install_sink(f: impl Fn(Level, &str) + Send + Sync + 'static) {
    let _ = SINK.set(Box::new(f));
}

pub fn log(level: Level, msg: &str) {
    if !cfg!(debug_assertions) && level < Level::Warn {
        return;
    }
    match SINK.get() {
        Some(s) => s(level, msg),
        None => eprintln!("[{level:?}] {msg}"),
    }
}

#[macro_export]
macro_rules! log_debug { ($($t:tt)*) => { if cfg!(debug_assertions) { $crate::logging::log($crate::logging::Level::Debug, &format!($($t)*)) } } }
#[macro_export]
macro_rules! log_info { ($($t:tt)*) => { if cfg!(debug_assertions) { $crate::logging::log($crate::logging::Level::Info, &format!($($t)*)) } } }
#[macro_export]
macro_rules! log_warn { ($($t:tt)*) => { $crate::logging::log($crate::logging::Level::Warn, &format!($($t)*)) } }
#[macro_export]
macro_rules! log_error { ($($t:tt)*) => { $crate::logging::log($crate::logging::Level::Error, &format!($($t)*)) } }

/// Keep only the last 4 characters.
pub fn redact(s: &str) -> String {
    let n = s.chars().count();
    if n <= 4 {
        return "****".into();
    }
    let tail: String = s.chars().skip(n - 4).collect();
    format!("****{tail}")
}

/// Strip the query string (presigned signatures live there).
pub fn redact_url(url: &str) -> String {
    match url.find('?') {
        Some(i) => format!("{}?<redacted>", &url[..i]),
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redaction() {
        assert_eq!(redact("abcdefgh"), "****efgh");
        assert_eq!(redact("abc"), "****");
        assert_eq!(redact_url("https://x/y?X-Amz-Signature=s"), "https://x/y?<redacted>");
        assert_eq!(redact_url("https://x/y"), "https://x/y");
    }
}
