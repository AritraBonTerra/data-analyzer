//! macOS integration details: well-known directories, URL decoding, and the
//! NSWorkspace call that makes us the default app for data files.

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

/// Content types we offer to open by default. Must match `CFBundleDocumentTypes`
/// in `packaging/Info.plist`.
pub const DATA_FILE_TYPES: &[&str] = &[
    "public.comma-separated-values-text",
    "public.tab-separated-values-text",
    "org.openxmlformats.spreadsheetml.sheet",
    "com.microsoft.excel.xls",
    "org.apache.parquet",
    "public.json",
    "public.ndjson",
    "org.jsonlines.jsonl",
];

/// Asks macOS to make this app the default for every type in [`DATA_FILE_TYPES`].
/// macOS confirms each change with the user, so this returns once the requests
/// are sent. Only works from the bundled `.app`.
#[cfg(target_os = "macos")]
pub fn make_default_for_data_files() -> anyhow::Result<()> {
    use block2::RcBlock;
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::{NSBundle, NSError, NSString};
    use objc2_uniform_type_identifiers::UTType;

    let app_url = NSBundle::mainBundle().bundleURL();
    let is_bundle = app_url
        .path()
        .is_some_and(|path| path.to_string().ends_with(".app"));
    if !is_bundle {
        anyhow::bail!("Run the installed Data Analyzer.app to make it the default app.");
    }
    let workspace = NSWorkspace::sharedWorkspace();
    for identifier in DATA_FILE_TYPES {
        let Some(content_type) = UTType::typeWithIdentifier(&NSString::from_str(identifier)) else {
            continue;
        };
        let identifier = identifier.to_string();
        let done = RcBlock::new(move |error: *mut NSError| {
            // SAFETY: AppKit passes either null or a valid NSError.
            if let Some(error) = unsafe { error.as_ref() } {
                eprintln!("couldn't become default for {identifier}: {error:?}");
            }
        });
        workspace.setDefaultApplicationAtURL_toOpenContentType_completionHandler(
            &app_url,
            &content_type,
            Some(&done),
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn make_default_for_data_files() -> anyhow::Result<()> {
    anyhow::bail!("Setting the default app is only supported on macOS.")
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
