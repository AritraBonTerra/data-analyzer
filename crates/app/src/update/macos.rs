//! The updater's macOS steps. Releases ship a DMG holding `Data Analyzer.app`.
//! Every step calls the system's own tools by absolute path: curl fetches,
//! `ssh-keygen` checks the signature, hdiutil and ditto stage the app,
//! codesign and plutil check it, and `renamex_np` swaps it in.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};

use super::{CANCELLED, Manifest, Source};
use crate::platform;

/// This platform's manifest, under `<base>/latest/download/`.
pub const MANIFEST: &str = "latest.json";

/// What the download is saved as in the scratch folder.
pub const DOWNLOAD: &str = "update.dmg";

/// The app's name inside a release DMG, and the name it is installed under.
pub const DMG_APP: &str = "Data Analyzer.app";

/// The one sentence shown when the app can't replace itself where it is.
pub const MOVE_FIRST: &str = "Move Data Analyzer to Applications to update it.";

/// `/usr/bin/curl` set up the same way for every request.
fn curl(source: &Source) -> Command {
    let mut command = Command::new("/usr/bin/curl");
    command
        .args(["--fail", "--location", "--silent", "--show-error"])
        .args(["--proto", source.protocols()])
        .stdin(Stdio::null());
    command
}

/// Reads the manifest's text (10 second and 64 KiB limits).
pub fn fetch_manifest(source: &Source) -> Result<String> {
    let output = curl(source)
        .args(["--max-time", "10", "--max-filesize", "65536"])
        .arg(source.manifest_url())
        .output()
        .context("Couldn't run curl")?;
    ensure!(
        output.status.success(),
        "Couldn't check for updates: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Downloads the manifest's DMG to `dest`. About five times a second it
/// calls `progress` with the bytes so far; when that returns `false`, curl is
/// killed and the partial file deleted.
pub fn download(
    source: &Source,
    manifest: &Manifest,
    dest: &Path,
    mut progress: impl FnMut(u64) -> bool,
) -> Result<()> {
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    let _ = fs::remove_file(dest);
    let mut child = curl(source)
        .args(["--max-filesize", &manifest.size.to_string()])
        // Give up on a stalled connection rather than wait forever.
        .args(["--speed-limit", "1024", "--speed-time", "30"])
        .arg("--output")
        .arg(dest)
        .arg(&manifest.asset)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("Couldn't run curl")?;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        let done = fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
        if !progress(done) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(dest);
            bail!(CANCELLED);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let _ = fs::remove_file(dest);
        bail!(
            "Couldn't download the update: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Checks `file` against `signature` and `source`'s key with
/// `ssh-keygen -Y verify`.
pub fn verify_signature(source: &Source, signature: &str, file: &Path) -> Result<()> {
    let mut key = source.key.split_whitespace();
    let (Some(kind), Some(data)) = (key.next(), key.next()) else {
        bail!("The app's update key is malformed.");
    };
    let signers = file.with_extension("signers");
    let sig = file.with_extension("sig");
    fs::write(&signers, format!("data-analyzer {kind} {data}\n"))?;
    fs::write(&sig, signature)?;
    let status = Command::new("/usr/bin/ssh-keygen")
        .args([
            "-Y",
            "verify",
            "-I",
            "data-analyzer",
            "-n",
            super::NAMESPACE,
        ])
        .arg("-f")
        .arg(&signers)
        .arg("-s")
        .arg(&sig)
        .stdin(fs::File::open(file)?)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = fs::remove_file(&signers);
    let _ = fs::remove_file(&sig);
    ensure!(
        status.context("Couldn't run ssh-keygen")?.success(),
        "The download isn't signed with the app's update key."
    );
    Ok(())
}

/// Mounts the verified DMG read-only at `<scratch>/mount`, copies its app to
/// `staged`, and checks the copy: bundle id, version equal to `version`, and
/// code signature. On any failure no staged copy is left.
pub fn stage(version: &str, dmg: &Path, scratch: &Path, staged: &Path) -> Result<()> {
    let mount = scratch.join("mount");
    let _ = fs::remove_dir_all(staged);
    fs::create_dir_all(&mount)?;
    run(Command::new("/usr/bin/hdiutil")
        .args([
            "attach",
            "-nobrowse",
            "-readonly",
            "-noautoopen",
            "-mountpoint",
        ])
        .arg(&mount)
        .arg(dmg))
    .context("Couldn't open the downloaded disk image")?;
    let copied = run(Command::new("/usr/bin/ditto")
        .arg(mount.join(DMG_APP))
        .arg(staged))
    .context("Couldn't copy the app out of the disk image");
    detach(&mount);

    let checked = copied.and_then(|()| check_bundle(staged, version));
    if checked.is_err() {
        let _ = fs::remove_dir_all(staged);
    }
    checked
}

/// Force-detaches a disk image mounted at `mount` and removes the folder.
fn detach(mount: &Path) {
    let _ = run(Command::new("/usr/bin/hdiutil")
        .args(["detach", "-force"])
        .arg(mount));
    let _ = fs::remove_dir(mount);
}

/// Removes an install's scratch folder, first detaching the disk image a
/// dead process may have left mounted in it.
pub fn remove_scratch(dir: &Path) {
    use std::os::unix::fs::MetadataExt;
    let mount = dir.join("mount");
    let mounted = fs::metadata(&mount)
        .and_then(|m| Ok(m.dev() != fs::metadata(dir)?.dev()))
        .unwrap_or(false);
    if mounted {
        detach(&mount);
    }
    let _ = fs::remove_dir_all(dir);
}

/// Checks that `app` is Data Analyzer at `version` and that its code
/// signature holds. The version check stops an older signed release from
/// being replayed as a newer one.
pub fn check_bundle(app: &Path, version: &str) -> Result<()> {
    let (id, found) = bundle_info(app)?;
    ensure!(
        id == platform::BUNDLE_ID,
        "The update isn't Data Analyzer (its bundle id is {id})."
    );
    ensure!(
        found == version,
        "The update holds version {found}, not the {version} the release lists."
    );
    run(Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app))
    .context("The update's code signature doesn't hold")?;
    Ok(())
}

/// The version of the app at `app`, from its `Info.plist`.
pub fn installed_version(app: &Path) -> Result<String> {
    Ok(bundle_info(app)?.1)
}

/// A bundle's `CFBundleIdentifier` and `CFBundleShortVersionString`.
fn bundle_info(app: &Path) -> Result<(String, String)> {
    let plist = app.join("Contents/Info.plist");
    let read = |key: &str| -> Result<String> {
        let output = Command::new("/usr/bin/plutil")
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(&plist)
            .output()?;
        ensure!(output.status.success(), "{} has no {key}.", app.display());
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    Ok((
        read("CFBundleIdentifier")?,
        read("CFBundleShortVersionString")?,
    ))
}

/// Runs a system tool, turning a failure into an error with its stderr.
fn run(command: &mut Command) -> Result<()> {
    let output = command.stdin(Stdio::null()).output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Puts the staged app at `app` and the old one at `staged`, in one
/// `renamex_np(RENAME_SWAP)`: `app` always holds a whole app, old or new, and
/// a failure moves nothing. [`super::clean_up`] deletes the old app at a
/// later launch. The exchange isn't flushed to disk: after a power cut
/// either app may be the one at `app`.
pub fn replace(staged: &Path, app: &Path) -> std::io::Result<()> {
    exchange(staged, app)
}

/// Swaps two paths on one volume atomically.
fn exchange(a: &Path, b: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let a = CString::new(a.as_os_str().as_bytes())?;
    let b = CString::new(b.as_os_str().as_bytes())?;
    // SAFETY: both are NUL-terminated paths that outlive the call.
    if unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_SWAP) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Starts a new instance of `app` with `files` open and returns once macOS
/// has started it.
pub fn relaunch(app: &Path, files: &[PathBuf]) -> Result<()> {
    run(Command::new("/usr/bin/open")
        .arg("-n")
        .arg(app)
        .arg("--args")
        .args(files))
    .context("Couldn't start the new version")
}

/// The `.app` this executable runs from, or `None` in a developer build.
pub fn running_app() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let macos = exe.parent()?;
    let app = macos.parent()?.parent()?;
    let in_bundle =
        macos.ends_with("Contents/MacOS") && app.extension().is_some_and(|ext| ext == "app");
    in_bundle.then(|| app.to_path_buf())
}

/// Whether `app` can be replaced where it is. Not when macOS runs it from an
/// App Translocation copy (opened straight from Downloads or a DMG), nor
/// when its folder can't be written (a mounted DMG, any read-only place).
pub fn can_update_in_place(app: &Path) -> bool {
    !app.to_string_lossy().contains("/AppTranslocation/") && super::folder_writable(app)
}

/// Test stand-ins for a release's app, shared with the engine's tests.
#[cfg(test)]
pub mod stand_in {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::DMG_APP;
    use crate::platform;
    use crate::update::tests::sh;

    /// What an installed app is called.
    pub const APP: &str = DMG_APP;

    /// A stand-in Data Analyzer at `version`; see [`bundle`].
    pub fn app(app: &Path, version: &str) {
        bundle(app, platform::BUNDLE_ID, version);
    }

    /// A stand-in `.app` with the given bundle id and version, signed ad hoc.
    /// Its executable is a script that writes its arguments to `launched.txt`
    /// beside the bundle, so a relaunch can be seen.
    pub fn bundle(app: &Path, id: &str, version: &str) {
        let macos = app.join("Contents/MacOS");
        fs::create_dir_all(&macos).unwrap();
        fs::write(
            app.join("Contents/Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{id}</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
<key>CFBundleExecutable</key><string>run</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>
"#
            ),
        )
        .unwrap();
        let out = app.parent().unwrap().join("launched.txt");
        let script = macos.join("run");
        fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", out.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        sh(Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(app));
    }

    /// A release DMG at `dest` holding the stand-in `app`.
    pub fn package(app: &Path, dest: &Path) -> PathBuf {
        let content = app.parent().unwrap();
        sh(Command::new("/usr/bin/hdiutil")
            .args([
                "create",
                "-quiet",
                "-format",
                "UDZO",
                "-volname",
                "Data Analyzer",
            ])
            .arg("-srcfolder")
            .arg(content)
            .arg(dest));
        dest.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::stand_in::{self, bundle};
    use super::{DMG_APP, bundle_info, can_update_in_place, check_bundle, exchange};
    use crate::platform;
    use crate::update::clean_up;
    use crate::update::tests::{make_writable, marked, scratch, sh};

    /// Makes a read-only disk image of `content` and mounts it at `mount`.
    /// One at a time: attaches from tests running in parallel fail.
    fn mount_image(content: &Path, image: &Path, mount: &Path) {
        static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _turn = ONE_AT_A_TIME.lock().unwrap();
        sh(Command::new("/usr/bin/hdiutil")
            .args(["create", "-quiet", "-format", "UDZO", "-srcfolder"])
            .arg(content)
            .arg(image));
        fs::create_dir_all(mount).unwrap();
        sh(Command::new("/usr/bin/hdiutil")
            .args(["attach", "-quiet", "-nobrowse", "-readonly", "-mountpoint"])
            .arg(mount)
            .arg(image));
    }

    /// The text a [`marked`] folder holds.
    fn mark(dir: &Path) -> String {
        fs::read_to_string(dir.join("v")).unwrap()
    }

    fn unmount(mount: &Path) {
        sh(Command::new("/usr/bin/hdiutil")
            .args(["detach", "-quiet", "-force"])
            .arg(mount));
    }

    #[test]
    fn checks_the_bundle() {
        let dir = scratch("bundle");
        let app = dir.join("Data Analyzer.app");
        stand_in::app(&app, "0.2.1");
        check_bundle(&app, "0.2.1").unwrap();

        // An older signed release replayed under a newer manifest.
        let err = check_bundle(&app, "0.2.2").unwrap_err();
        assert!(err.to_string().contains("holds version 0.2.1"), "{err}");

        let other = dir.join("Other.app");
        bundle(&other, "com.example.Other", "0.2.1");
        let err = check_bundle(&other, "0.2.1").unwrap_err();
        assert!(err.to_string().contains("com.example.Other"), "{err}");

        // A change after signing breaks the code signature.
        fs::write(app.join("Contents/MacOS/run"), "#!/bin/sh\necho changed\n").unwrap();
        let err = check_bundle(&app, "0.2.1").unwrap_err();
        assert!(format!("{err:#}").contains("code signature"), "{err:#}");
        assert_eq!(bundle_info(&app).unwrap().0, platform::BUNDLE_ID);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn exchanges_in_one_step() {
        let dir = scratch("exchange");
        let (a, b) = (dir.join("a"), dir.join("b"));
        marked(&a, "a");
        marked(&b, "b");
        exchange(&a, &b).unwrap();
        assert_eq!((mark(&a), mark(&b)), ("b".into(), "a".into()));

        // Across volumes the exchange fails, and neither folder moves.
        let content = dir.join("content");
        marked(&content.join("c"), "c");
        let mount = dir.join("mount");
        mount_image(&content, &dir.join("image.dmg"), &mount);
        let failed = exchange(&mount.join("c"), &a);
        let (on_image, here) = (mark(&mount.join("c")), mark(&a));
        unmount(&mount);
        assert!(failed.is_err());
        assert_eq!((on_image, here), ("c".into(), "b".into()));

        // Nothing staged: the installed folder stays.
        assert!(exchange(&dir.join("missing"), &a).is_err());
        assert_eq!(mark(&a), "b");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn detaches_an_image_a_dead_process_left_mounted() {
        let dir = scratch("clean-up-mount");
        let dead = crate::update::tests::dead_pid();
        let updates = dir.join("updates");
        let app = dir.join("Applications").join(DMG_APP);
        marked(&app, "installed");
        let content = dir.join("content");
        marked(&content, "image");
        let mount = updates.join(format!("{dead}-3/mount"));
        mount_image(&content, &dir.join("image.dmg"), &mount);

        clean_up(&updates, &app);

        assert!(!updates.join(format!("{dead}-3")).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn knows_when_it_cannot_update_in_place() {
        let dir = scratch("in-place");
        let app = dir.join("Data Analyzer.app");
        fs::create_dir_all(&app).unwrap();
        assert!(can_update_in_place(&app));

        let translocated = PathBuf::from(
            "/private/var/folders/xy/T/AppTranslocation/1234-ABCD/d/Data Analyzer.app",
        );
        assert!(!can_update_in_place(&translocated));

        // A folder the user can't write to.
        let locked = dir.join("locked");
        fs::create_dir_all(locked.join("Data Analyzer.app")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        assert!(!can_update_in_place(&locked.join("Data Analyzer.app")));
        make_writable(&locked).unwrap();

        // A mounted DMG, as when the app runs from the volume it came on.
        let content = dir.join("content");
        fs::create_dir_all(content.join("Data Analyzer.app")).unwrap();
        let mount = dir.join("mount");
        mount_image(&content, &dir.join("image.dmg"), &mount);
        let mounted = can_update_in_place(&mount.join("Data Analyzer.app"));
        unmount(&mount);
        assert!(!mounted);
        fs::remove_dir_all(dir).unwrap();
    }
}
