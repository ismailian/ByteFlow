//! File kinds by extension (spec §10.1, §6.2).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Image,
    Video,
    Audio,
    Text,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StorageCategory {
    Docs,
    Images,
    Videos,
    Music,
    Other,
}

impl StorageCategory {
    pub const ALL: [StorageCategory; 5] =
        [Self::Docs, Self::Images, Self::Videos, Self::Music, Self::Other];
    pub fn label(self) -> &'static str {
        match self {
            Self::Docs => "Docs",
            Self::Images => "Images",
            Self::Videos => "Videos",
            Self::Music => "Music",
            Self::Other => "Other",
        }
    }
}

pub const VIEWABLE_IMAGE_EXTS: &[&str] =
    &["jpg", "jpeg", "png", "gif", "webp", "heic", "bmp", "svg"];
const VIDEO_EXTS: &[&str] = &["mp4", "mov", "mkv", "avi", "webm", "m4v"];
const AUDIO_EXTS: &[&str] = &["mp3", "wav", "flac", "aac", "ogg", "m4a", "opus"];
const TEXT_EXTS: &[&str] = &[
    "txt", "md", "log", "ini", "conf", "env", "csv", "tsv", "json", "yaml", "yml", "toml", "xml",
    "html", "htm", "css", "js", "ts", "tsx", "jsx", "rs", "dart", "py", "rb", "go", "java", "kt",
    "swift", "c", "h", "cpp", "hpp", "cs", "sh", "ps1", "bat", "sql", "gradle", "properties",
    "lua", "php", "vue", "svelte",
];
const DOC_EXTS: &[&str] = &[
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf", "pages",
    "numbers", "key", "epub",
];

pub fn extension(key: &str) -> String {
    let leaf = leaf_name(key);
    match leaf.rfind('.') {
        Some(i) if i > 0 => leaf[i + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// Last path segment; for a folder key (`a/b/`) that is `b`.
pub fn leaf_name(key: &str) -> &str {
    let k = key.trim_end_matches('/');
    match k.rfind('/') {
        Some(i) => &k[i + 1..],
        None => k,
    }
}

/// Parent prefix including its trailing `/`, or `""` at root.
pub fn parent_prefix(key: &str) -> &str {
    let k = key.trim_end_matches('/');
    match k.rfind('/') {
        Some(i) => &key[..=i],
        None => "",
    }
}

pub fn media_kind_for_key(key: &str) -> MediaKind {
    let ext = extension(key);
    let e = ext.as_str();
    if VIEWABLE_IMAGE_EXTS.contains(&e) {
        MediaKind::Image
    } else if VIDEO_EXTS.contains(&e) {
        MediaKind::Video
    } else if AUDIO_EXTS.contains(&e) {
        MediaKind::Audio
    } else if TEXT_EXTS.contains(&e) {
        MediaKind::Text
    } else {
        MediaKind::None
    }
}

pub fn storage_category(key: &str) -> StorageCategory {
    match media_kind_for_key(key) {
        MediaKind::Image => StorageCategory::Images,
        MediaKind::Video => StorageCategory::Videos,
        MediaKind::Audio => StorageCategory::Music,
        MediaKind::Text => StorageCategory::Docs,
        MediaKind::None => {
            if DOC_EXTS.contains(&extension(key).as_str()) {
                StorageCategory::Docs
            } else {
                StorageCategory::Other
            }
        }
    }
}

pub fn content_type_for(key: &str) -> &'static str {
    match extension(key).as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "ogg" | "opus" => "audio/ogg",
        "m4a" => "audio/mp4",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "txt" | "md" | "log" | "ini" | "conf" | "env" => "text/plain; charset=utf-8",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kinds() {
        assert_eq!(media_kind_for_key("a/b/Photo.JPG"), MediaKind::Image);
        assert_eq!(media_kind_for_key("clip.mkv"), MediaKind::Video);
        assert_eq!(media_kind_for_key("notes.md"), MediaKind::Text);
        assert_eq!(media_kind_for_key("archive.zip"), MediaKind::None);
        assert_eq!(leaf_name("a/b/c.txt"), "c.txt");
        assert_eq!(leaf_name("a/b/"), "b");
        assert_eq!(parent_prefix("a/b/c.txt"), "a/b/");
        assert_eq!(parent_prefix("a/b/"), "a/");
        assert_eq!(parent_prefix("c.txt"), "");
        assert_eq!(storage_category("x.pdf"), StorageCategory::Docs);
    }
}
