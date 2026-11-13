//! Where the app meets the operating system: its folders, file URLs, and
//! becoming the default app for data files. The halves that differ live in
//! `platform/macos.rs` (NSWorkspace) and `platform/linux.rs` (XDG folders,
//! desktop entry, one running instance).

use std::path::PathBuf;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

/// Reverse-DNS identifier: the macOS bundle id, and the Linux app id and
/// desktop entry name.
pub const BUNDLE_ID: &str = "dev.ari3.DataAnalyzer";

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// Turns a URL handed to us by Finder or `open` into a file path. Accepts
/// `file://` URLs (percent-decoded) and plain paths; anything else is `None`.
pub fn url_to_path(url: &str) -> Option<PathBuf> {
    let path = if let Some(rest) = url.strip_prefix("file://") {
        // Drop an optional host part: `file://localhost/Users/...`.
        let rest = match rest.find('/') {
            Some(0) => rest,
            Some(slash) => &rest[slash..],
            None => return None,
        };
        percent_encoding::percent_decode_str(rest)
            .decode_utf8()
            .ok()?
            .into_owned()
    } else if url.starts_with('/') || url.starts_with('~') {
        url.to_string()
    } else if url.contains("://") {
        return None;
    } else {
        url.to_string()
    };
    let path = match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(path),
    };
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_file_urls() {
        assert_eq!(
            url_to_path("file:///Users/me/My%20Data/sales%202024.csv"),
            Some(PathBuf::from("/Users/me/My Data/sales 2024.csv"))
        );
        assert_eq!(
            url_to_path("file://localhost/tmp/a.parquet"),
            Some(PathBuf::from("/tmp/a.parquet"))
        );
        assert_eq!(
            url_to_path("file:///tmp/caf%C3%A9.csv"),
            Some(PathBuf::from("/tmp/café.csv"))
        );
    }

    #[test]
    fn passes_plain_paths_through() {
        assert_eq!(url_to_path("/tmp/x.csv"), Some(PathBuf::from("/tmp/x.csv")));
        assert_eq!(url_to_path("data/x.csv"), Some(PathBuf::from("data/x.csv")));
        assert_eq!(url_to_path("https://example.com/x.csv"), None);
        assert!(url_to_path("~/x.csv").unwrap().is_absolute());
    }
}
