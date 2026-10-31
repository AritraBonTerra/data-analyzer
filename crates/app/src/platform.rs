//! macOS integration details: well-known directories, URL decoding, and the
//! LaunchServices hook for becoming the default app for data files.

use std::path::PathBuf;

/// Reverse-DNS identifier used for the Caches and Application Support folders.
pub const BUNDLE_ID: &str = "dev.ari3.DataAnalyzer";

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// `~/Library/Caches/<bundle id>`: scratch DuckDB databases live here.
pub fn cache_dir() -> PathBuf {
    home().join("Library/Caches").join(BUNDLE_ID)
}

/// `~/Library/Application Support/<bundle id>`: recent files and settings.
pub fn app_support_dir() -> PathBuf {
    home().join("Library/Application Support").join(BUNDLE_ID)
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

/// Registers Data Analyzer as the default handler for CSV, TSV, Excel,
/// Parquet and JSON files.
///
/// TODO(aritra): implement with `NSWorkspace.setDefaultApplication(at:toOpen:)`
/// for each UTI once the app is bundled and signed. Until then this is a no-op
/// that reports what it would do.
pub fn make_default_for_data_files() -> anyhow::Result<()> {
    anyhow::bail!("Making Data Analyzer the default app needs the bundled build. Coming soon.")
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
