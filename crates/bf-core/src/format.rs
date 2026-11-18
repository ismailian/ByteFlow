//! Human formatting helpers.

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v >= 100.0 { format!("{v:.0} {}", UNITS[i]) } else { format!("{v:.1} {}", UNITS[i]) }
}

pub fn rate(bytes_per_sec: f64) -> String {
    format!("{}/s", bytes(bytes_per_sec.max(0.0) as u64))
}

pub fn duration_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        if secs % 60 == 0 { format!("{} min", secs / 60) } else { format!("{}m {}s", secs / 60, secs % 60) }
    } else if secs < 86_400 {
        if secs % 3600 == 0 { format!("{} h", secs / 3600) } else { format!("{}h {}m", secs / 3600, (secs % 3600) / 60) }
    } else if secs % 86_400 == 0 {
        format!("{} d", secs / 86_400)
    } else {
        format!("{}d {}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}

/// Relative age ("3 min ago") from epoch millis.
pub fn relative_age(ms: i64, now_ms: i64) -> String {
    let d = ((now_ms - ms).max(0) / 1000) as u64;
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", d / 60),
        3600..=86_399 => format!("{} h ago", d / 3600),
        86_400..=2_591_999 => format!("{} d ago", d / 86_400),
        _ => format!("{} mo ago", d / 2_592_000),
    }
}

/// Exact local time from epoch millis.
pub fn local_time(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
        None => "—".into(),
    }
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    #[test]
    fn bytes_fmt() {
        assert_eq!(super::bytes(0), "0 B");
        assert_eq!(super::bytes(1536), "1.5 KB");
        assert_eq!(super::bytes(8 * 1024 * 1024), "8.0 MB");
    }
}
