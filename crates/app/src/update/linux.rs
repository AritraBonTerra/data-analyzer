//! The updater's Linux steps. Releases ship one AppImage per architecture,
//! and the installed app is the AppImage file the user runs (`$APPIMAGE`).
//! Fetching and the signature check run in-process (ureq, ssh-key), since
//! curl and ssh-keygen aren't on every desktop. The new AppImage is staged
//! beside the old one and renamed over it; the running process keeps the old
//! file open until it quits.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail, ensure};

use super::{CANCELLED, Manifest, Source};

/// This build's manifest, under `<base>/latest/download/`: one per architecture.
pub const MANIFEST: &str = if cfg!(target_arch = "aarch64") {
    "latest-linux-aarch64.json"
} else {
    "latest-linux-x86_64.json"
};

/// What the download is saved as in the scratch folder.
pub const DOWNLOAD: &str = "update.AppImage";

/// The one sentence shown when the app can't replace itself where it is.
pub const MOVE_FIRST: &str =
    "Move the AppImage to a folder you own, like ~/Applications, to update it.";

/// An HTTP agent that only speaks HTTPS (plain HTTP for a local test server),
/// gives up on a request after `timeout`, and follows GitHub's redirects.
fn agent(source: &Source, timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(source.protocols() == "=https")
        .timeout_global(Some(timeout))
        .user_agent(concat!("data-analyzer/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Reads the manifest's text (10 second and 64 KiB limits).
pub fn fetch_manifest(source: &Source) -> Result<String> {
    agent(source, Duration::from_secs(10))
        .get(source.manifest_url())
        .call()
        .and_then(|mut response| {
            response
                .body_mut()
                .with_config()
                .limit(65536)
                .read_to_string()
        })
        .context("Couldn't check for updates")
}

/// Downloads the manifest's AppImage to `dest`. About five times a second it
/// calls `progress` with the bytes so far; when that returns `false`, the
/// download stops and the partial file is deleted. Gives up after 30 seconds
/// with no data, as curl's speed limit does on macOS.
pub fn download(
    source: &Source,
    manifest: &Manifest,
    dest: &Path,
    mut progress: impl FnMut(u64) -> bool,
) -> Result<()> {
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = fs::File::create(dest)?;
    // The transfer runs on a thread of its own, so a stalled connection can't
    // keep this loop from noticing Cancel or the stall. `stop` ends it at its
    // next read; one stuck in a read ends with the 30 minute request limit.
    let stop = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::channel();
    let transfer = {
        let agent = agent(source, Duration::from_secs(30 * 60));
        let (url, size, stop) = (manifest.asset.clone(), manifest.size, stop.clone());
        move || {
            let _ = done_tx.send(fetch(&agent, &url, size, file, &stop));
        }
    };
    std::thread::spawn(transfer);

    let (mut seen, mut moved) = (0, Instant::now());
    let result = loop {
        match done_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(fetched) => break fetched.context("Couldn't download the update"),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(anyhow!("Couldn't download the update."));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let done = fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
        if done != seen {
            (seen, moved) = (done, Instant::now());
        }
        if !progress(done) {
            break Err(anyhow!(CANCELLED));
        }
        if moved.elapsed() > Duration::from_secs(30) {
            break Err(anyhow!(
                "Couldn't download the update: no data for 30 seconds."
            ));
        }
    };
    if result.is_err() {
        stop.store(true, Ordering::Relaxed);
        let _ = fs::remove_file(dest);
    }
    result
}

/// Streams `url` into `file`, refusing more than `size` bytes, until done or `stop`.
fn fetch(
    agent: &ureq::Agent,
    url: &str,
    size: u64,
    mut file: fs::File,
    stop: &AtomicBool,
) -> Result<()> {
    let mut response = agent.get(url).call()?;
    let mut body = response.body_mut().as_reader();
    let mut buffer = vec![0; 1 << 16];
    let mut done = 0u64;
    loop {
        ensure!(!stop.load(Ordering::Relaxed), CANCELLED);
        let read = body.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        done += read as u64;
        ensure!(
            done <= size,
            "The download is bigger than the {size} bytes the release lists."
        );
        file.write_all(&buffer[..read])?;
    }
    file.sync_all()?;
    Ok(())
}

/// Checks `file` against `signature`, an `ssh-keygen -Y sign` signature, and
/// `source`'s key.
pub fn verify_signature(source: &Source, signature: &str, file: &Path) -> Result<()> {
    let key = ssh_key::PublicKey::from_openssh(source.key.trim())
        .context("The app's update key is malformed.")?;
    let signature: ssh_key::SshSig = signature
        .parse()
        .context("The update's signature is malformed.")?;
    let bytes = fs::read(file)?;
    key.verify(super::NAMESPACE, &bytes, &signature)
        .ok()
        .context("The download isn't signed with the app's update key.")
}

/// Copies the verified AppImage to `staged`, makes it executable, and checks
/// that it runs here and reports `version`. The version check stops an older
/// signed release from being replayed as a newer one. On any failure no
/// staged copy is left.
pub fn stage(version: &str, appimage: &Path, _scratch: &Path, staged: &Path) -> Result<()> {
    let _ = fs::remove_file(staged);
    let checked = (|| -> Result<()> {
        fs::copy(appimage, staged).context("Couldn't copy the update beside the app")?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(staged, fs::Permissions::from_mode(0o755))?;
        let found = installed_version(staged)?;
        ensure!(
            found == version,
            "The update holds version {found}, not the {version} the release lists."
        );
        Ok(())
    })();
    if checked.is_err() {
        let _ = fs::remove_file(staged);
    }
    checked
}

/// Scratch folders hold only plain files here.
pub fn remove_scratch(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

/// The version the AppImage at `app` reports with `--version` (30 second limit).
pub fn installed_version(app: &Path) -> Result<String> {
    let spawn = || {
        Command::new(app)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    };
    // A file just written can briefly be "busy": a process forked meanwhile
    // holds the write handle until it execs.
    let mut spawned = spawn();
    for _ in 0..20 {
        match &spawned {
            Err(err) if err.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(Duration::from_millis(50));
                spawned = spawn();
            }
            _ => break,
        }
    }
    let mut child = spawned.with_context(|| format!("Couldn't run {}", app.display()))?;
    let started = Instant::now();
    while child.try_wait()?.is_none() {
        if started.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{} didn't report its version.", app.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "{} didn't report its version.",
        app.display()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Renames the staged AppImage over `app`, in one step: `app` always holds a
/// whole AppImage, old or new, and a failure moves nothing. The running
/// process keeps reading the old file, which goes once it quits.
pub fn replace(staged: &Path, app: &Path) -> std::io::Result<()> {
    fs::rename(staged, app)
}

/// Starts the new version with `files` open, as a process of its own that
/// outlives this one. This instance stops taking later launches' files
/// first, so the new one doesn't hand them back to it.
pub fn relaunch(app: &Path, files: &[PathBuf]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    crate::platform::stop_listening();
    Command::new(app)
        .args(files)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("Couldn't start the new version")?;
    Ok(())
}

/// The AppImage this process runs from, or `None` in a developer build. The
/// AppImage runtime sets `$APPIMAGE` and mounts the image at `$APPDIR`; the
/// executable must be inside that mount, since a process started from
/// another AppImage (a terminal, say) inherits both.
pub fn running_app() -> Option<PathBuf> {
    let appimage = PathBuf::from(std::env::var_os("APPIMAGE")?);
    let appdir = PathBuf::from(std::env::var_os("APPDIR")?);
    let exe = std::env::current_exe().ok()?;
    (appimage.is_absolute() && appimage.is_file() && exe.starts_with(appdir)).then_some(appimage)
}

/// Whether `app` can be replaced where it is: its folder must be writable
/// (not a system folder, a read-only mount, or someone else's).
pub fn can_update_in_place(app: &Path) -> bool {
    super::folder_writable(app)
}

/// Test stand-ins for a release's AppImage, shared with the engine's tests.
#[cfg(test)]
pub mod stand_in {
    use std::fs;
    use std::path::{Path, PathBuf};

    /// What an installed app is called.
    pub const APP: &str = "Data-Analyzer-x86_64.AppImage";

    /// A stand-in AppImage at `version`: a script that prints the version for
    /// `--version`, and otherwise writes its arguments to `launched.txt`
    /// beside it, so a relaunch can be seen.
    pub fn app(app: &Path, version: &str) {
        fs::create_dir_all(app.parent().unwrap()).unwrap();
        let out = app.parent().unwrap().join("launched.txt");
        fs::write(
            app,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo {version}; exit; }}\nprintf '%s\\n' \"$@\" > '{}'\n",
                out.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(app, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A release asset at `dest` holding the stand-in `app`: the AppImage itself.
    pub fn package(app: &Path, dest: &Path) -> PathBuf {
        fs::copy(app, dest).unwrap();
        dest.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use super::stand_in::{self, APP};
    use super::{can_update_in_place, installed_version, relaunch, stage};
    use crate::update::tests::{make_writable, scratch};

    #[test]
    fn stages_a_copy_that_runs_and_holds_the_version() {
        let dir = scratch("stage");
        let download = dir.join("update.AppImage");
        stand_in::app(&download, "0.3.0");
        // Downloads arrive without the executable bit.
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&download, fs::Permissions::from_mode(0o644)).unwrap();
        let staged = dir.join(format!(".{APP}.update-1-1"));

        stage("0.3.0", &download, &dir, &staged).unwrap();
        assert_eq!(installed_version(&staged).unwrap(), "0.3.0");

        // An older signed release replayed under a newer manifest.
        let err = stage("0.3.1", &download, &dir, &staged).unwrap_err();
        assert!(err.to_string().contains("holds version 0.3.0"), "{err}");
        assert!(!staged.exists());

        // Not an AppImage that runs here.
        fs::write(&download, b"\x7fELF but not really").unwrap();
        assert!(stage("0.3.0", &download, &dir, &staged).is_err());
        assert!(!staged.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn relaunches_with_the_open_files() {
        let dir = scratch("relaunch");
        let app = dir.join(APP);
        stand_in::app(&app, "0.3.0");
        let files = [
            dir.join("sales 2024.csv"),
            std::path::PathBuf::from("/tmp/b.parquet"),
        ];
        relaunch(&app, &files).unwrap();
        let launched = dir.join("launched.txt");
        let mut text = String::new();
        for _ in 0..50 {
            text = fs::read_to_string(&launched).unwrap_or_default();
            if !text.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(text, format!("{}\n/tmp/b.parquet\n", files[0].display()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn knows_when_it_cannot_update_in_place() {
        let dir = scratch("in-place");
        let app = dir.join(APP);
        stand_in::app(&app, "0.3.0");
        assert!(can_update_in_place(&app));

        // A folder the user can't write to, like /opt or /usr/local/bin.
        let locked = dir.join("locked");
        stand_in::app(&locked.join(APP), "0.3.0");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        let writable = can_update_in_place(&locked.join(APP));
        make_writable(&locked).unwrap();
        // Root writes anywhere, so only check where permissions apply.
        if unsafe { libc::geteuid() } != 0 {
            assert!(!writable);
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
