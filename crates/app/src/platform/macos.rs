//! macOS: the Library folders and the NSWorkspace call that makes us the
//! default app for data files.

use std::path::PathBuf;

use super::{BUNDLE_ID, home};

/// `~/Library/Caches/<bundle id>`: scratch DuckDB databases and updates.
pub fn cache_dir() -> PathBuf {
    home().join("Library/Caches").join(BUNDLE_ID)
}

/// `~/Library/Application Support/<bundle id>`: recent files and settings.
pub fn data_dir() -> PathBuf {
    home().join("Library/Application Support").join(BUNDLE_ID)
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
/// macOS confirms each change with the user, so this returns, with the line
/// to show, once the requests are sent. Only works from the bundled `.app`.
pub fn make_default_for_data_files() -> anyhow::Result<&'static str> {
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
    Ok("Requested. macOS will ask you to confirm each file type.")
}
