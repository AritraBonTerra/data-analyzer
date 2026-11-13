//! Linux: XDG folders, the desktop entry that puts the AppImage in the app
//! menu and makes it the default for data files, and one running instance
//! per user, so a second launch opens its files in the first (as macOS does).

use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};

use super::{BUNDLE_ID, home};

/// The AppImage's own desktop entry. Its `MimeType` line is the list of
/// types we open by default.
const DESKTOP_ENTRY: &str =
    include_str!("../../../../packaging/linux/dev.ari3.DataAnalyzer.desktop");
/// MIME types shared-mime-info lacks (Parquet, JSON Lines, NDJSON).
const MIME_PACKAGE: &str = include_str!("../../../../packaging/linux/dev.ari3.DataAnalyzer.xml");
const ICON: &str = include_str!("../../../../assets/AppIcon.svg");

/// An XDG base folder: `$<var>` when it is an absolute path, else `~/<fallback>`.
fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

/// `$XDG_CACHE_HOME/data-analyzer`: scratch DuckDB databases and updates.
pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("data-analyzer")
}

/// `$XDG_DATA_HOME/data-analyzer`: recent files and settings.
pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("data-analyzer")
}

/// `$XDG_DATA_HOME`, where desktop entries, icons and MIME types are installed.
fn share_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share")
}

fn desktop_entry_path() -> PathBuf {
    share_dir()
        .join("applications")
        .join(format!("{BUNDLE_ID}.desktop"))
}

/// The MIME types we offer to open, from the desktop entry's `MimeType` line.
pub fn data_file_types() -> Vec<&'static str> {
    DESKTOP_ENTRY
        .lines()
        .find_map(|line| line.strip_prefix("MimeType="))
        .unwrap_or_default()
        .split(';')
        .filter(|kind| !kind.is_empty())
        .collect()
}

/// The desktop entry for the AppImage at `app`: ours, starting `app`.
fn desktop_entry(app: &Path) -> String {
    DESKTOP_ENTRY.replace("Exec=data-analyzer ", &format!("Exec={} ", exec_quote(app)))
}

/// `path` as one quoted argument of a desktop entry's `Exec` key. The spec
/// escapes `"`, `` ` ``, `$` and `\` inside quotes, then escapes backslashes
/// again because the value is a string; `%` starts a field code.
fn exec_quote(path: &Path) -> String {
    let mut quoted = String::from("\"");
    for c in path.to_string_lossy().chars() {
        match c {
            '"' | '`' | '$' => {
                quoted.push_str("\\\\");
                quoted.push(c);
            }
            '\\' => quoted.push_str("\\\\\\\\"),
            '%' => quoted.push_str("%%"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// Puts Data Analyzer in the app menu and makes it the default for every
/// type in [`data_file_types`]: installs the desktop entry (starting this
/// AppImage), the icon and the MIME types, then asks `xdg-mime` to set the
/// defaults. Returns the line to show. Only works from the AppImage.
pub fn make_default_for_data_files() -> Result<&'static str> {
    let app = crate::update::running_app()
        .context("Run the Data Analyzer AppImage to make it the default app.")?;
    let share = share_dir();
    write(
        &share
            .join("icons/hicolor/scalable/apps")
            .join(format!("{BUNDLE_ID}.svg")),
        ICON,
    )?;
    write(
        &share.join("mime/packages").join(format!("{BUNDLE_ID}.xml")),
        MIME_PACKAGE,
    )?;
    write(&desktop_entry_path(), &desktop_entry(&app))?;

    // Refresh the caches desktops read. Without these tools the desktop
    // catches up on its own, later.
    let _ = run(Command::new("update-mime-database").arg(share.join("mime")));
    let _ = run(Command::new("update-desktop-database").arg(share.join("applications")));
    run(Command::new("xdg-mime")
        .args(["default", &format!("{BUNDLE_ID}.desktop")])
        .args(data_file_types()))
    .context("Couldn't set the default app with xdg-mime")?;
    Ok("Done. Data Analyzer is in your app menu and opens data files.")
}

/// Points an installed desktop entry at this AppImage, so the app menu and
/// file associations keep working after the AppImage moves or an update
/// changes the entry. Does nothing until Make Default has installed one.
pub fn refresh_desktop_entry() {
    let (Some(app), path) = (crate::update::running_app(), desktop_entry_path()) else {
        return;
    };
    let wanted = desktop_entry(&app);
    if fs::read_to_string(&path).is_ok_and(|current| current != wanted) {
        let _ = fs::write(&path, wanted);
    }
}

fn write(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, contents).with_context(|| format!("Couldn't write {}", path.display()))
}

/// Runs a desktop tool found on `PATH`, turning a failure into an error with its stderr.
fn run(command: &mut Command) -> Result<()> {
    let output = command.stdin(Stdio::null()).output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

// One running instance. The first launch listens on a Unix socket in the
// user's runtime folder; later launches send it their files (NUL-separated
// absolute paths; none means "a window, please") and exit.

fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(cache_dir)
        .join(format!("{BUNDLE_ID}.sock"))
}

/// What a launch turned out to be: the first, holding the socket later
/// launches reach it on (none if the socket couldn't be made), or a later one
/// whose files the first has taken, which should exit.
pub enum Launch {
    First(Option<UnixListener>),
    Forwarded,
}

/// Hands `files` to the running instance, or becomes it. Call before the app
/// starts, so later launches find the socket as early as possible.
pub fn claim_instance(files: &[PathBuf]) -> Launch {
    claim(&socket_path(), files)
}

fn claim(socket: &Path, files: &[PathBuf]) -> Launch {
    if let Some(dir) = socket.parent() {
        let _ = fs::create_dir_all(dir);
    }
    // One launch decides at a time; otherwise two started together could
    // both find no instance and both become one. Released on return.
    let lock = fs::File::create(socket.with_extension("lock"));
    if let Ok(lock) = &lock {
        let _ = lock.lock();
    }
    if forward(socket, files) {
        return Launch::Forwarded;
    }
    // Nothing answered: any socket there is a dead process's.
    let _ = fs::remove_file(socket);
    Launch::First(UnixListener::bind(socket).ok())
}

fn forward(socket: &Path, files: &[PathBuf]) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return false;
    };
    let mut message = Vec::new();
    for file in files {
        let file = std::path::absolute(file).unwrap_or_else(|_| file.clone());
        message.extend_from_slice(file.as_os_str().as_bytes());
        message.push(0);
    }
    stream.write_all(&message).is_ok()
}

/// On a thread of its own, passes each later launch's files to `open`.
pub fn listen_for_launches(listener: UnixListener, open: impl Fn(Vec<PathBuf>) + Send + 'static) {
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut message = Vec::new();
            if stream.read_to_end(&mut message).is_ok() {
                let files = message
                    .split(|&byte| byte == 0)
                    .filter(|path| !path.is_empty())
                    .map(|path| PathBuf::from(OsStr::from_bytes(path)));
                open(files.collect());
            }
        }
    });
}

/// Stops being the running instance, so the next launch starts its own: the
/// updater calls this just before it starts the new version.
pub fn stop_listening() {
    let _ = fs::remove_file(socket_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_exec_paths() {
        assert_eq!(
            exec_quote(Path::new("/home/me/Apps/Data-Analyzer.AppImage")),
            r#""/home/me/Apps/Data-Analyzer.AppImage""#
        );
        assert_eq!(
            exec_quote(Path::new(r#"/home/me/my "$apps"/100%\x"#)),
            r#""/home/me/my \\"\\$apps\\"/100%%\\\\x""#
        );
        let entry = desktop_entry(Path::new("/opt/apps/Data Analyzer.AppImage"));
        assert!(entry.contains("\nExec=\"/opt/apps/Data Analyzer.AppImage\" %F\n"));
    }

    #[test]
    fn declares_every_type_it_claims() {
        let types = data_file_types();
        assert!(types.contains(&"text/csv"));
        assert!(types.contains(&"application/vnd.apache.parquet"));
        // Each type our MIME package declares is one the entry opens.
        for declared in MIME_PACKAGE.split("<mime-type type=\"").skip(1) {
            let kind = declared.split('"').next().unwrap();
            assert!(types.contains(&kind), "{kind} isn't in MimeType");
        }
    }

    /// A socket path of its own for one test.
    fn test_socket(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("da-instance-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("test.sock")
    }

    #[test]
    fn a_second_launch_hands_its_files_to_the_first() {
        let socket = test_socket("forward");
        assert!(!forward(&socket, &[]), "nothing is listening yet");

        let Launch::First(Some(listener)) = claim(&socket, &[]) else {
            panic!("the first launch didn't become the instance");
        };
        let (tx, rx) = std::sync::mpsc::channel();
        listen_for_launches(listener, move |files| tx.send(files).unwrap());
        let files = [
            PathBuf::from("/tmp/sales 2024.csv"),
            PathBuf::from("b.parquet"),
        ];
        assert!(matches!(claim(&socket, &files), Launch::Forwarded));
        let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(got[0], files[0]);
        // Relative paths arrive absolute, as the second launch saw them.
        assert_eq!(got[1], std::env::current_dir().unwrap().join("b.parquet"));

        assert!(matches!(claim(&socket, &[]), Launch::Forwarded));
        assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap().is_empty());
        fs::remove_dir_all(socket.parent().unwrap()).unwrap();
    }

    #[test]
    fn launches_started_together_make_one_instance() {
        let socket = test_socket("together");
        // A dead process's socket, as after a crash.
        drop(match claim(&socket, &[]) {
            Launch::First(listener) => listener,
            Launch::Forwarded => unreachable!(),
        });
        let start = std::sync::Barrier::new(8);
        let launches: Vec<Launch> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        claim(&socket, &[])
                    })
                })
                .collect();
            threads.into_iter().map(|t| t.join().unwrap()).collect()
        });
        let first = launches
            .iter()
            .filter(|l| matches!(l, Launch::First(Some(_))))
            .count();
        assert_eq!(first, 1, "more than one launch became the instance");
        fs::remove_dir_all(socket.parent().unwrap()).unwrap();
    }
}
