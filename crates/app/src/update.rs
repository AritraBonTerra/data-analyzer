//! The app updates itself from its GitHub releases.
//!
//! The engine (first half) has no UI types. Its steps are plain blocking
//! functions: [`check`] reads the newest release's manifest for this
//! platform, [`download`] fetches its asset (a DMG on macOS, an AppImage on
//! Linux), [`verify`] checks size, SHA-256 and the SSH signature, [`stage`]
//! puts a checked copy of the new app beside the installed one ([`prepare`]
//! runs those three), [`swap`] puts it in place of the installed app and
//! [`relaunch`] starts it. [`clean_up`] removes what dead processes left.
//! `data-analyzer --update` runs them in a row ([`run_cli`]). Nothing
//! persists between launches. How each step is done on each platform is in
//! `update/macos.rs` and `update/linux.rs`.
//!
//! The UI half ([`Updater`]) runs the same steps on the background executor
//! and shows one prompt card for the whole app, in the active window.
//!
//! Where updates come from and which key signs them are fixed at compile time
//! (`DA_UPDATE_BASE`, `DA_UPDATE_KEY`); nothing at run time can change them.
//! The updater is inert unless the app runs installed: from a `.app` bundle
//! on macOS, from an AppImage on Linux.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, Result, ensure};
use futures::StreamExt;
use gpui_kit::component::WindowExt;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::platform;
use crate::theme::{self, tabular};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as os;

pub use os::running_app;

/// The running app's version, as in `crates/app/Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The SSH signature namespace release assets are signed under.
const NAMESPACE: &str = "data-analyzer-update";

/// Where updates come from and the public key their assets are signed with.
/// The app only ever uses [`Source::built_in`]; tests make their own.
#[derive(Clone, Debug)]
pub struct Source {
    /// A releases URL: the manifest is `<base>/latest/download/<manifest>`
    /// and every asset must be under `<base>/download/`.
    pub base: String,
    /// An OpenSSH public key line: `<type> <base64> [comment]`.
    pub key: String,
}

impl Source {
    /// The compiled-in source: GitHub and `packaging/update-key.pub`, unless
    /// the build set `DA_UPDATE_BASE` or `DA_UPDATE_KEY` (the key's text).
    pub fn built_in() -> Self {
        Self {
            base: option_env!("DA_UPDATE_BASE")
                .unwrap_or("https://github.com/AritraBonTerra/data-analyzer/releases")
                .into(),
            key: option_env!("DA_UPDATE_KEY")
                .unwrap_or(include_str!("../../../packaging/update-key.pub"))
                .into(),
        }
    }

    /// This platform's manifest: `latest.json` on macOS,
    /// `latest-linux-<arch>.json` on Linux.
    fn manifest_url(&self) -> String {
        format!("{}/latest/download/{}", self.base, os::MANIFEST)
    }

    /// curl's `--proto`: HTTPS only, unless the base itself is a local test server.
    fn protocols(&self) -> &'static str {
        if self.base.starts_with("http://127.0.0.1:") {
            "=http"
        } else {
            "=https"
        }
    }
}

/// A release's manifest. Missing keys are rejected; keys a later release
/// adds are ignored, so older apps can still read it. The asset's key names
/// its kind: `dmg` on macOS (as every macOS release so far wrote it),
/// `appimage` on Linux.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct Manifest {
    pub version: String,
    #[cfg_attr(target_os = "macos", serde(rename = "dmg"))]
    #[cfg_attr(target_os = "linux", serde(rename = "appimage"))]
    pub asset: String,
    pub size: u64,
    pub sha256: String,
    pub signature: String,
    pub notes: String,
}

impl Manifest {
    /// Parses a manifest and checks every field against `source`.
    pub fn parse(text: &str, source: &Source) -> Result<Self> {
        let manifest: Self =
            serde_json::from_str(text).context("The update manifest is malformed")?;
        semver::Version::parse(&manifest.version).with_context(|| {
            format!(
                "The update manifest's version {:?} isn't a version.",
                manifest.version
            )
        })?;
        ensure!(
            manifest
                .asset
                .starts_with(&format!("{}/download/", source.base)),
            "The update manifest points somewhere else: {}",
            manifest.asset
        );
        ensure!(
            manifest.notes.starts_with(&format!("{}/", source.base)),
            "The update manifest's release page is somewhere else: {}",
            manifest.notes
        );
        ensure!(
            manifest.size > 0,
            "The update manifest lists an empty file."
        );
        ensure!(
            manifest.sha256.len() == 64
                && manifest
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "The update manifest's checksum isn't SHA-256."
        );
        ensure!(
            manifest
                .signature
                .starts_with("-----BEGIN SSH SIGNATURE-----"),
            "The update manifest's signature isn't an SSH signature."
        );
        Ok(manifest)
    }
}

/// Whether version `candidate` is newer than `current` (semver order).
pub fn is_newer(candidate: &str, current: &str) -> Result<bool> {
    Ok(semver::Version::parse(candidate)? > semver::Version::parse(current)?)
}

/// The manifest in `text` if it offers a version newer than `current`.
pub fn newer(text: &str, source: &Source, current: &str) -> Result<Option<Manifest>> {
    let manifest = Manifest::parse(text, source)?;
    Ok(is_newer(&manifest.version, current)?.then_some(manifest))
}

/// Reads the newest release's manifest (10 second limit). Returns it if it
/// offers a version newer than this app.
pub fn check(source: &Source) -> Result<Option<Manifest>> {
    newer(&os::fetch_manifest(source)?, source, VERSION)
}

/// The error a cancelled install ends with.
const CANCELLED: &str = "Cancelled.";

/// Downloads the manifest's asset to `dest`. About five times a second it
/// calls `progress` with the bytes so far; when that returns `false`, the
/// download stops and the partial file is deleted.
pub fn download(
    source: &Source,
    manifest: &Manifest,
    dest: &Path,
    progress: impl FnMut(u64) -> bool,
) -> Result<()> {
    os::download(source, manifest, dest, progress)
}

/// Checks the downloaded asset: its size and SHA-256 against the manifest,
/// then its signature against `source`'s key.
pub fn verify(source: &Source, manifest: &Manifest, file: &Path) -> Result<()> {
    let size = fs::metadata(file)?.len();
    ensure!(
        size == manifest.size,
        "The download is {size} bytes, not the {} the release lists.",
        manifest.size
    );
    ensure!(
        sha256(file)? == manifest.sha256,
        "The download's checksum doesn't match the release."
    );
    os::verify_signature(source, &manifest.signature, file)
}

/// The lowercase hex SHA-256 of a file.
fn sha256(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 16];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Puts a checked copy of the app in the verified asset `file` at `staged`,
/// using `scratch` for any working files. The copy must report `version`. On
/// any failure no staged copy is left.
pub fn stage(version: &str, file: &Path, scratch: &Path, staged: &Path) -> Result<()> {
    os::stage(version, file, scratch, staged)
}

/// The scratch folder and the staged sibling of one install, tagged
/// `<pid>-<n>`: `<updates>/<tag>/` and `.<app name>.update-<tag>` beside
/// `app`, on the same volume so the swap is a rename. No two processes, and
/// no two installs in one process, share them.
pub fn install_paths(updates: &Path, app: &Path, n: u64) -> (PathBuf, PathBuf) {
    let tag = format!("{}-{n}", std::process::id());
    let name = app.file_name().unwrap_or_default().to_string_lossy();
    let staged = app.with_file_name(format!(".{name}.update-{tag}"));
    (updates.join(tag), staged)
}

/// Removes a staged app (a folder on macOS, a file on Linux) or a leftover.
fn remove(path: &Path) {
    if path.is_dir() {
        let _ = fs::remove_dir_all(path);
    } else {
        let _ = fs::remove_file(path);
    }
}

/// Downloads, verifies and stages one update, logging each step. `cancel`
/// is checked during the download and between steps. The scratch folder
/// `dir` is always removed; `staged` is left only on success.
pub fn prepare(
    source: &Source,
    manifest: &Manifest,
    dir: &Path,
    staged: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
    log: impl Fn(&str),
) -> Result<()> {
    let go_on = || {
        ensure!(!cancel.load(Ordering::Relaxed), CANCELLED);
        Ok(())
    };
    let file = dir.join(os::DOWNLOAD);
    let mut steps = || -> Result<()> {
        log(&format!("Downloading {}", manifest.asset));
        download(source, manifest, &file, |done| {
            progress(done);
            !cancel.load(Ordering::Relaxed)
        })?;
        go_on()?;
        log("Verifying checksum and signature");
        verify(source, manifest, &file)?;
        go_on()?;
        log(&format!("Staging {}", staged.display()));
        stage(&manifest.version, &file, dir, staged)?;
        go_on()
    };
    let result = steps();
    os::remove_scratch(dir);
    if result.is_err() {
        remove(staged);
    }
    result
}

/// Puts the staged copy in place of `app`, if it is newer than the app on
/// disk right now (another instance may have updated it already). `app`
/// always holds a whole app, old or new, and a failure moves nothing; on
/// macOS the old app is left at `staged` for [`clean_up`].
pub fn swap(app: &Path, staged: &Path) -> Result<()> {
    let installed = os::installed_version(app)?;
    let update = os::installed_version(staged)?;
    ensure!(
        is_newer(&update, &installed)?,
        "Data Analyzer is already {installed}."
    );
    os::replace(staged, app).context("Couldn't move the update into place")
}

/// Starts a new instance of `app` with `files` open and returns once it has
/// started.
pub fn relaunch(app: &Path, files: &[PathBuf]) -> Result<()> {
    os::relaunch(app, files)
}

/// Deletes what updaters in processes that are no longer running left
/// behind: scratch folders under `updates` and staged siblings of `app`,
/// including the old app a macOS swap leaves there. Call once per launch.
pub fn clean_up(updates: &Path, app: &Path) {
    let dead = |tag: &str| {
        tag.split('-')
            .next()
            .and_then(|pid| pid.parse::<i32>().ok())
            .is_some_and(|pid| pid > 0 && !process_alive(pid))
    };
    for entry in fs::read_dir(updates).into_iter().flatten().flatten() {
        if dead(&entry.file_name().to_string_lossy()) {
            os::remove_scratch(&entry.path());
        }
    }
    let (Some(folder), Some(name)) = (app.parent(), app.file_name()) else {
        return;
    };
    let prefix = format!(".{}.update-", name.to_string_lossy());
    for entry in fs::read_dir(folder).into_iter().flatten().flatten() {
        let file = entry.file_name();
        if let Some(tag) = file.to_string_lossy().strip_prefix(&prefix)
            && dead(tag)
        {
            remove(&entry.path());
        }
    }
}

/// Whether a process with this pid exists. EPERM means it exists.
fn process_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    alive || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Whether a file can be created beside `app`, as staging and swapping need.
fn folder_writable(app: &Path) -> bool {
    let Some(folder) = app.parent() else {
        return false;
    };
    let probe = folder.join(format!(".data-analyzer-probe-{}", std::process::id()));
    let writable = fs::File::create_new(&probe).is_ok();
    let _ = fs::remove_file(&probe);
    writable
}

/// Whether `app` can be replaced where it is.
pub fn can_update_in_place(app: &Path) -> bool {
    os::can_update_in_place(app)
}

/// The folder that holds each install's scratch folder.
fn updates_dir() -> PathBuf {
    platform::cache_dir().join("updates")
}

/// `data-analyzer --update`: check, download, verify, stage and swap with no
/// window, one line per step. Returns the process exit code: 0 when updated
/// or already up to date.
pub fn run_cli() -> i32 {
    match update_from_cli() {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("{err:#}");
            1
        }
    }
}

fn update_from_cli() -> Result<()> {
    let app = running_app().context("Updates only work in the installed app.")?;
    let source = Source::built_in();
    clean_up(&updates_dir(), &app);
    println!("Checking {}", source.manifest_url());
    let Some(manifest) = check(&source)? else {
        println!("Up to date: {VERSION} is the newest version.");
        return Ok(());
    };
    ensure!(can_update_in_place(&app), os::MOVE_FIRST);
    let (dir, staged) = install_paths(&updates_dir(), &app, 0);
    let never = AtomicBool::new(false);
    prepare(
        &source,
        &manifest,
        &dir,
        &staged,
        &never,
        |_| {},
        |step| println!("{step}"),
    )?;
    swap(&app, &staged)?;
    println!("Updated {} to {}", app.display(), manifest.version);
    Ok(())
}

// The UI half.

/// The app's one updater: what its prompt card shows and the work behind it.
/// Lives in the [`Global`] [`UpdaterGlobal`]; absent in developer builds.
pub struct Updater {
    source: Source,
    app: PathBuf,
    /// Holds each install's scratch folder: [`updates_dir`] in the app.
    updates: PathBuf,
    state: State,
    /// Bumped by every check, install and cancel, so stale results are
    /// dropped; also tags each install's paths.
    generation: u64,
    /// Set to stop the running install.
    cancel: Arc<AtomicBool>,
    /// The window whose notification list holds the card, while it shows.
    shown_in: Option<AnyWindowHandle>,
}

/// What the card shows. Nothing of it outlives the process.
#[derive(Clone, Debug, PartialEq)]
enum State {
    /// Nothing found yet, or nothing to offer.
    Idle,
    /// A newer version; `in_place` is false when the app must be moved first.
    Available {
        manifest: Manifest,
        in_place: bool,
    },
    Downloading {
        manifest: Manifest,
        done: u64,
    },
    /// Downloaded, checked and staged at this path; waits for Relaunch.
    Ready(PathBuf),
    /// Relaunch was clicked: swapping and starting the new version.
    Relaunching,
}

struct UpdaterGlobal(Entity<Updater>);

impl Global for UpdaterGlobal {}

/// Starts the updater in a bundled app: cleans up after updaters that are no
/// longer running and checks once, silently, in the background. Does
/// nothing in a developer build.
pub fn init(cx: &mut App) {
    let Some(app) = running_app() else {
        return;
    };
    let (updates, stale) = (updates_dir(), app.clone());
    cx.background_spawn(async move { clean_up(&updates, &stale) })
        .detach();
    let updater = cx.new(|_| Updater::new(Source::built_in(), app, updates_dir()));
    updater.update(cx, |updater, cx| updater.check(false, cx));
    cx.set_global(UpdaterGlobal(updater));
}

/// The app menu's Check for Updates…: always answers, with the card, "You're
/// up to date", or a short error, opening a window first if none is open.
pub fn check_from_menu(cx: &mut App) {
    if cx.global::<crate::Windows>().0.is_empty() {
        crate::new_window(None, cx);
    }
    match cx.try_global::<UpdaterGlobal>().map(|g| g.0.clone()) {
        Some(updater) => updater.update(cx, |updater, cx| updater.check(true, cx)),
        None => notify(
            Notification::info("Updates only work in the installed app."),
            cx,
        ),
    }
}

/// The menu's answer when there is nothing newer.
fn up_to_date() -> Notification {
    Notification::new()
        .title("You're up to date.")
        .message(format!("Data Analyzer {VERSION} is the newest version."))
}

/// Pushes a passing notification to the active window. Deferred, like every
/// change to a window here: a button's click handler runs inside its
/// window's update, where updating that window again fails.
fn notify(note: Notification, cx: &mut App) {
    cx.defer(move |cx| {
        if let Some(window) = target_window(cx) {
            let _ = window.update(cx, |_, window, cx| window.push_notification(note, cx));
        }
    });
}

/// The active window, or any open one.
fn target_window(cx: &App) -> Option<AnyWindowHandle> {
    cx.active_window()
        .or_else(|| cx.global::<crate::Windows>().0.first().map(|(h, _)| *h))
}

impl Updater {
    fn new(source: Source, app: PathBuf, updates: PathBuf) -> Self {
        Self {
            source,
            app,
            updates,
            state: State::Idle,
            generation: 0,
            cancel: Arc::default(),
            shown_in: None,
        }
    }

    /// Checks for a newer version. `announce` is set by the menu: then the
    /// card shows again if it has something, and "up to date" or an error
    /// gets a notification. The launch check is silent unless it finds one.
    fn check(&mut self, announce: bool, cx: &mut Context<Self>) {
        if announce
            && matches!(
                self.state,
                State::Downloading { .. } | State::Ready(_) | State::Relaunching
            )
        {
            self.show(cx);
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        let (source, app) = (self.source.clone(), self.app.clone());
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_spawn(async move {
                    Ok::<_, anyhow::Error>((check(&source)?, can_update_in_place(&app)))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match found {
                    Ok((Some(manifest), in_place)) => {
                        this.state = State::Available { manifest, in_place };
                        this.show(cx);
                    }
                    Ok((None, _)) => {
                        this.state = State::Idle;
                        this.hide(cx);
                        if announce {
                            notify(up_to_date(), cx);
                        }
                    }
                    Err(err) if announce => notify(Notification::error(format!("{err:#}")), cx),
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Install: download, verify and stage in the background, with progress.
    fn install(&mut self, cx: &mut Context<Self>) {
        let State::Available { manifest, .. } = self.state.clone() else {
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        self.cancel = Arc::default();
        self.state = State::Downloading {
            manifest: manifest.clone(),
            done: 0,
        };
        cx.notify();

        let (tx, mut rx) = futures::channel::mpsc::unbounded::<u64>();
        cx.spawn(async move |this, cx| {
            while let Some(done) = rx.next().await {
                let _ = this.update(cx, |this, cx| {
                    if let State::Downloading { done: shown, .. } = &mut this.state
                        && this.generation == generation
                    {
                        *shown = done;
                        cx.notify();
                    }
                });
            }
        })
        .detach();

        let (source, cancel) = (self.source.clone(), self.cancel.clone());
        let (dir, staged) = install_paths(&self.updates, &self.app, generation);
        cx.spawn(async move |this, cx| {
            let prepared = cx
                .background_spawn({
                    let (manifest, staged) = (manifest.clone(), staged.clone());
                    async move {
                        let progress = |done| {
                            let _ = tx.unbounded_send(done);
                        };
                        prepare(&source, &manifest, &dir, &staged, &cancel, progress, |_| {})
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match prepared {
                    Ok(()) => {
                        this.state = State::Ready(staged);
                        this.show(cx);
                    }
                    Err(err) => {
                        this.state = State::Idle;
                        this.hide(cx);
                        notify(Notification::error(format!("Couldn't update. {err:#}")), cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Cancel: stops the install and puts the card away. The install removes
    /// its own files as it stops.
    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let State::Downloading { manifest, .. } = self.state.clone() {
            self.cancel.store(true, Ordering::Relaxed);
            self.generation += 1;
            self.state = State::Available {
                manifest,
                in_place: true,
            };
        }
        self.hide(cx);
    }

    /// Relaunch: swaps the staged app in, starts it with this app's open
    /// files, and quits. A second click finds the state busy and does nothing.
    fn relaunch(&mut self, cx: &mut Context<Self>) {
        let State::Ready(staged) = self.state.clone() else {
            return;
        };
        self.state = State::Relaunching;
        cx.notify();
        let app = self.app.clone();
        let files: Vec<PathBuf> = cx
            .global::<crate::Windows>()
            .0
            .iter()
            .filter_map(|(_, workspace)| workspace.read(cx).path(cx))
            .collect();
        cx.spawn(async move |this, cx| {
            let swapped = cx
                .background_spawn(async move {
                    let swapped = swap(&app, &staged).and_then(|()| relaunch(&app, &files));
                    if swapped.is_err() {
                        remove(&staged);
                    }
                    swapped
                })
                .await;
            let _ = this.update(cx, |this, cx| match swapped {
                Ok(()) => cx.quit(),
                Err(err) => {
                    this.state = State::Idle;
                    this.hide(cx);
                    notify(Notification::error(format!("Couldn't update. {err:#}")), cx);
                }
            });
        })
        .detach();
    }

    /// Puts the card in the active window, unless it is already there.
    fn show(&mut self, cx: &mut Context<Self>) {
        let Some(target) = target_window(cx) else {
            return;
        };
        if self.shown_in == Some(target) {
            cx.notify();
            return;
        }
        self.hide(cx);
        let card = cx.entity();
        let weak = card.downgrade();
        let note = Notification::new()
            .id::<Updater>()
            .autohide(false)
            .content(move |_, _, _| card.clone().into_any_element())
            // The card's close button: same as Later, and Cancel while downloading.
            .on_close(move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    if this.shown_in == Some(target) {
                        this.shown_in = None;
                        this.cancel(cx);
                    }
                });
            });
        self.shown_in = Some(target);
        cx.defer(move |cx| {
            let _ = target.update(cx, |_, window, cx| window.push_notification(note, cx));
        });
    }

    /// Takes the card away. The state stays, so the menu can bring it back.
    fn hide(&mut self, cx: &mut Context<Self>) {
        if let Some(window) = self.shown_in.take() {
            cx.defer(move |cx| {
                let _ = window.update(cx, |_, window, cx| {
                    window.remove_notification::<Updater>(cx)
                });
            });
        }
    }
}

/// Bytes as whole megabytes, as the card shows them.
fn megabytes(bytes: u64) -> u64 {
    (bytes + 500_000) / 1_000_000
}

/// A small card button. Its id is also its debug selector, so tests can
/// find and click it.
fn card_button(id: &'static str, label: &'static str) -> Button {
    Button::new(id)
        .debug_selector(move || id.into())
        .small()
        .label(label)
}

impl Render for Updater {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let palette = theme::palette(cx);
        let later = card_button("update-later", "Later")
            .outline()
            .on_click(cx.listener(|this, _, _, cx| this.hide(cx)));

        let (title, line, progress, buttons): (String, String, Option<f32>, Vec<Button>) =
            match &self.state {
                State::Idle => return div().into_any_element(),
                State::Available { manifest, in_place } => {
                    let notes = manifest.notes.clone();
                    let open_notes = move |button: Button| {
                        let notes = notes.clone();
                        button.on_click(move |_, _, cx| cx.open_url(&notes))
                    };
                    let title = format!("Data Analyzer {} is available", manifest.version);
                    if *in_place {
                        let install = card_button("update-install", "Install")
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| this.install(cx)));
                        (
                            title,
                            format!("You have {VERSION}."),
                            None,
                            vec![
                                install,
                                later,
                                open_notes(card_button("update-notes", "What's new").outline()),
                            ],
                        )
                    } else {
                        let page =
                            open_notes(card_button("update-notes", "Release page").primary());
                        (title, os::MOVE_FIRST.into(), None, vec![page, later])
                    }
                }
                State::Downloading { manifest, done } => {
                    let cancel = card_button("update-cancel", "Cancel")
                        .outline()
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx)));
                    (
                        format!("Downloading {}", manifest.version),
                        format!("{} of {} MB", megabytes(*done), megabytes(manifest.size)),
                        Some(*done as f32 / manifest.size as f32),
                        vec![cancel],
                    )
                }
                State::Ready(_) => {
                    let relaunch = card_button("update-relaunch", "Relaunch")
                        .primary()
                        .on_click(cx.listener(|this, _, _, cx| this.relaunch(cx)));
                    (
                        "Update ready".into(),
                        "Relaunch to finish. Your open files reopen.".into(),
                        None,
                        vec![relaunch, later],
                    )
                }
                State::Relaunching => (
                    "Update ready".into(),
                    "Relaunching…".into(),
                    None,
                    Vec::new(),
                ),
            };

        v_flex()
            .w_full()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .font_features(tabular())
                    .child(line),
            )
            .when_some(progress, |this, progress| {
                this.child(
                    div()
                        .mt_2p5()
                        .h(px(5.))
                        .w_full()
                        .rounded_full()
                        .bg(palette.chart_track)
                        .child(
                            div()
                                .h_full()
                                .w(relative(progress.clamp(0.02, 1.0)))
                                .rounded_full()
                                .bg(palette.accent),
                        ),
                )
            })
            .when(!buttons.is_empty(), |this| {
                this.child(h_flex().mt_2p5().gap_2().children(buttons))
            })
            .into_any_element()
    }
}

/// Shows the card in a given state, or the menu's "up to date" answer, as
/// the screenshots need them.
#[cfg(all(test, target_os = "macos"))]
pub fn show_for_shots(case: &'static str, cx: &mut App) {
    let manifest = Manifest {
        version: "0.2.1".into(),
        asset: String::new(),
        size: 33_400_000,
        sha256: String::new(),
        signature: String::new(),
        notes: String::new(),
    };
    let state = match case {
        "available" => State::Available {
            manifest,
            in_place: true,
        },
        "cannot" => State::Available {
            manifest,
            in_place: false,
        },
        "downloading" => State::Downloading {
            manifest,
            done: 13_100_000,
        },
        "ready" => State::Ready(PathBuf::new()),
        "uptodate" => return notify(up_to_date(), cx),
        _ => panic!("no update card {case:?}"),
    };
    let updater = cx.new(|_| Updater::new(Source::built_in(), PathBuf::new(), PathBuf::new()));
    updater.update(cx, |updater, cx| {
        updater.state = state;
        updater.show(cx);
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};

    // Named, not `super::*`: a glob would bring in gpui's `test` attribute.
    use super::os::stand_in;
    use super::{
        Manifest, Source, VERSION, check, clean_up, install_paths, is_newer, newer, os, prepare,
        sha256, swap, verify,
    };

    /// The manifest key the asset's URL is under on this platform.
    const ASSET: &str = if cfg!(target_os = "macos") {
        "dmg"
    } else {
        "appimage"
    };

    /// A fresh scratch folder for one test.
    pub fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("da-update-{}-{name}", std::process::id()));
        let _ = make_writable(&dir);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub fn make_writable(dir: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755))
    }

    pub fn sh(command: &mut Command) {
        let output = command.output().unwrap();
        assert!(output.status.success(), "{command:?}: {output:?}");
    }

    /// A throwaway ed25519 key pair; returns (private key path, public key text).
    fn key_pair(dir: &Path, name: &str) -> (PathBuf, String) {
        let path = dir.join(name);
        sh(Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "test", "-f"])
            .arg(&path));
        let public = fs::read_to_string(path.with_extension("pub")).unwrap();
        (path, public)
    }

    /// Signs `file` the way the release workflow does and returns the signature text.
    fn sign(key: &Path, file: &Path) -> String {
        sh(Command::new("ssh-keygen")
            .args(["-Y", "sign", "-n", "data-analyzer-update", "-f"])
            .arg(key)
            .arg(file));
        let sig = PathBuf::from(format!("{}.sig", file.display()));
        let text = fs::read_to_string(&sig).unwrap();
        fs::remove_file(sig).unwrap();
        text
    }

    pub fn source(base: &str, key: &str) -> Source {
        Source {
            base: base.into(),
            key: key.into(),
        }
    }

    pub const BASE: &str = "https://github.com/AritraBonTerra/data-analyzer/releases";

    pub fn manifest_json(version: &str) -> serde_json::Value {
        serde_json::to_value(Manifest {
            version: version.into(),
            asset: format!("{BASE}/download/v{version}/Data-Analyzer-{version}.dmg"),
            size: 34029918,
            sha256: "a".repeat(64),
            signature: "-----BEGIN SSH SIGNATURE-----\nAAAA\n-----END SSH SIGNATURE-----\n".into(),
            notes: format!("{BASE}/tag/v{version}"),
        })
        .unwrap()
    }

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.2.1", "0.2.0").unwrap());
        assert!(is_newer("0.10.0", "0.9.9").unwrap());
        assert!(is_newer("1.0.0", "0.99.0").unwrap());
        assert!(!is_newer("0.2.0", "0.2.0").unwrap());
        assert!(!is_newer("0.1.9", "0.2.0").unwrap());
        assert!(!is_newer("0.2.1-beta.1", "0.2.1").unwrap());
        assert!(is_newer("x", "0.2.0").is_err());
    }

    #[test]
    fn reads_the_manifest() {
        let source = source(BASE, "");
        let text = manifest_json("0.2.1").to_string();
        let manifest = newer(&text, &source, "0.2.0").unwrap().unwrap();
        assert_eq!(manifest.size, 34029918);

        // Not newer: nothing to offer.
        assert_eq!(newer(&text, &source, "0.2.1").unwrap(), None);
        assert_eq!(newer(&text, &source, "0.3.0").unwrap(), None);

        let mut missing = manifest_json("0.2.1");
        missing.as_object_mut().unwrap().remove("sha256");
        let err = Manifest::parse(&missing.to_string(), &source).unwrap_err();
        assert!(format!("{err:#}").contains("sha256"), "{err:#}");

        // Another platform's manifest has no asset for this one.
        let mut other = manifest_json("0.2.1");
        let url = other.as_object_mut().unwrap().remove(ASSET).unwrap();
        other[if ASSET == "dmg" { "appimage" } else { "dmg" }] = url;
        assert!(Manifest::parse(&other.to_string(), &source).is_err());

        // A key a later release adds is ignored.
        let mut extra = manifest_json("0.2.1");
        extra["arch"] = "arm64".into();
        assert_eq!(
            Manifest::parse(&extra.to_string(), &source).unwrap(),
            manifest
        );

        for (key, value) in [
            (ASSET, "https://example.com/releases/download/v0.2.1/x.dmg"),
            // A lookalike host that starts with the base's text.
            (
                ASSET,
                "https://github.com/AritraBonTerra/data-analyzer/releases.evil/download/x.dmg",
            ),
            (
                ASSET,
                "http://github.com/AritraBonTerra/data-analyzer/releases/download/x.dmg",
            ),
            ("notes", "file:///Applications/Calculator.app"),
            ("version", "latest"),
            ("sha256", "ABC"),
            ("signature", "trust me"),
        ] {
            let mut bad = manifest_json("0.2.1");
            bad[key] = value.into();
            assert!(
                Manifest::parse(&bad.to_string(), &source).is_err(),
                "{key} = {value} was accepted"
            );
        }
    }

    #[test]
    fn local_bases_only_allow_local_http() {
        assert_eq!(source(BASE, "").protocols(), "=https");
        assert_eq!(
            source("http://127.0.0.1:8000/releases", "").protocols(),
            "=http"
        );
        assert_eq!(
            source("http://example.com/releases", "").protocols(),
            "=https"
        );
    }

    /// A signed file and a manifest that matches it.
    fn signed_file(dir: &Path, key: &Path) -> (PathBuf, Manifest) {
        let file = dir.join("Data-Analyzer-0.2.1.dmg");
        fs::write(&file, b"pretend this is a disk image").unwrap();
        let manifest = Manifest {
            version: "0.2.1".into(),
            asset: format!("{BASE}/download/v0.2.1/Data-Analyzer-0.2.1.dmg"),
            size: fs::metadata(&file).unwrap().len(),
            sha256: sha256(&file).unwrap(),
            signature: sign(key, &file),
            notes: format!("{BASE}/tag/v0.2.1"),
        };
        (file, manifest)
    }

    #[test]
    fn verifies_size_checksum_and_signature() {
        let dir = scratch("verify");
        let (key, public) = key_pair(&dir, "key");
        let (_, other) = key_pair(&dir, "other");
        let (file, manifest) = signed_file(&dir, &key);
        let good = source(BASE, &public);

        verify(&good, &manifest, &file).unwrap();

        let err = verify(&source(BASE, &other), &manifest, &file).unwrap_err();
        assert!(err.to_string().contains("isn't signed"), "{err}");

        // Signed by the right key, but for another purpose.
        let wrong_namespace = {
            sh(Command::new("ssh-keygen")
                .args(["-Y", "sign", "-n", "file", "-f"])
                .arg(&key)
                .arg(&file));
            let sig = PathBuf::from(format!("{}.sig", file.display()));
            let text = fs::read_to_string(&sig).unwrap();
            fs::remove_file(sig).unwrap();
            Manifest {
                signature: text,
                ..manifest.clone()
            }
        };
        let err = verify(&good, &wrong_namespace, &file).unwrap_err();
        assert!(err.to_string().contains("isn't signed"), "{err}");

        let wrong_sum = Manifest {
            sha256: "0".repeat(64),
            ..manifest.clone()
        };
        let err = verify(&good, &wrong_sum, &file).unwrap_err();
        assert!(err.to_string().contains("checksum"), "{err}");

        let wrong_size = Manifest {
            size: manifest.size + 1,
            ..manifest.clone()
        };
        assert!(verify(&good, &wrong_size, &file).is_err());

        // Tampered after signing, with a manifest that matches the new bytes:
        // only the signature catches it.
        fs::write(&file, b"pretend this is a disk imagE").unwrap();
        let tampered = Manifest {
            sha256: sha256(&file).unwrap(),
            ..manifest
        };
        let err = verify(&good, &tampered, &file).unwrap_err();
        assert!(err.to_string().contains("isn't signed"), "{err}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn swaps_only_a_newer_copy() {
        let dir = scratch("swap");
        let app = dir.join(stand_in::APP);
        let staged = dir.join(format!(".{}.update-1-1", stand_in::APP));
        stand_in::app(&app, "0.2.1");
        stand_in::app(&staged, "0.2.1");

        // Another instance already put 0.2.1 in place: refuse, change nothing.
        let err = swap(&app, &staged).unwrap_err();
        assert_eq!(err.to_string(), "Data Analyzer is already 0.2.1.");
        assert_eq!(os::installed_version(&app).unwrap(), "0.2.1");
        assert!(staged.exists());

        super::remove(&staged);
        stand_in::app(&staged, "0.3.0");
        swap(&app, &staged).unwrap();
        assert_eq!(os::installed_version(&app).unwrap(), "0.3.0");
        // macOS: the old app waits at the staged path for the next launch's
        // clean-up. Linux: the rename leaves nothing there.
        if cfg!(target_os = "macos") {
            assert_eq!(os::installed_version(&staged).unwrap(), "0.2.1");
        } else {
            assert!(!staged.exists());
        }
        fs::remove_dir_all(dir).unwrap();
    }

    /// A folder holding one file, `v`, with this text.
    pub fn marked(dir: &Path, text: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("v"), text).unwrap();
    }

    /// The pid of a process that has exited.
    pub fn dead_pid() -> u32 {
        let mut child = Command::new("true").spawn().unwrap();
        child.wait().unwrap();
        child.id()
    }

    #[test]
    fn cleans_up_after_processes_that_are_gone() {
        let dir = scratch("clean-up");
        let (dead, live) = (dead_pid(), std::process::id());
        let updates = dir.join("updates");
        let app = dir.join("Applications").join(stand_in::APP);
        stand_in::app(&app, "0.2.1");
        for pid in [dead, live] {
            marked(&updates.join(format!("{pid}-3")), "scratch");
            stand_in::app(
                &app.with_file_name(format!(".{}.update-{pid}-3", stand_in::APP)),
                "0.3.0",
            );
        }

        clean_up(&updates, &app);

        let staged = |pid| app.with_file_name(format!(".{}.update-{pid}-3", stand_in::APP));
        assert!(!updates.join(format!("{dead}-3")).exists());
        assert!(!staged(dead).exists());
        assert!(updates.join(format!("{live}-3")).exists());
        assert!(staged(live).exists());
        assert_eq!(os::installed_version(&app).unwrap(), "0.2.1");
        fs::remove_dir_all(dir).unwrap();
    }

    /// A release served the way GitHub lays it out, from a scratch folder.
    pub struct Release {
        pub dir: PathBuf,
        pub source: Source,
        pub manifest: Manifest,
    }

    /// Builds a signed release asset holding a stand-in app at `version`, a
    /// manifest for it, and serves both on 127.0.0.1.
    pub fn release(dir: &Path, version: &str) -> Release {
        let (key, public) = key_pair(dir, "key");
        let content = dir.join("content");
        let app = content.join(stand_in::APP);
        stand_in::app(&app, version);
        let tag = format!("v{version}");
        let served = dir.join("served");
        let assets = served.join("releases/download").join(&tag);
        fs::create_dir_all(&assets).unwrap();
        let name = format!("Data-Analyzer-{version}.{ASSET}");
        let asset = stand_in::package(&app, &assets.join(&name));
        let port = serve(served.clone());
        let base = format!("http://127.0.0.1:{port}/releases");
        let manifest = Manifest {
            version: version.into(),
            asset: format!("{base}/download/{tag}/{name}"),
            size: fs::metadata(&asset).unwrap().len(),
            sha256: sha256(&asset).unwrap(),
            signature: sign(&key, &asset),
            notes: format!("{base}/tag/{tag}"),
        };
        let latest = served.join("releases/latest/download");
        fs::create_dir_all(&latest).unwrap();
        fs::write(
            latest.join(os::MANIFEST),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        Release {
            dir: dir.to_path_buf(),
            source: source(&base, &public),
            manifest,
        }
    }

    /// A minimal static HTTP server on a free local port, for as long as the
    /// test process runs. Serves GET requests for files under `root`.
    fn serve(root: PathBuf) -> u16 {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(&stream);
                let mut request = String::new();
                if reader.read_line(&mut request).is_err() {
                    continue;
                }
                // Read the headers too, so closing doesn't reset the connection.
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                let mut stream = &stream;
                match fs::read(root.join(path.trim_start_matches('/'))) {
                    Ok(body) => {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(&body);
                    }
                    Err(_) => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                }
            }
        });
        port
    }

    /// A server that sends a response's headers and first bytes, then stalls.
    fn stalled_server() -> String {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                let mut reader = BufReader::new(&stream);
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let mut stream = &stream;
                let _ =
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\nfirst");
                std::thread::sleep(std::time::Duration::from_secs(120));
            }
        });
        format!("http://127.0.0.1:{port}/releases")
    }

    #[test]
    fn cancels_a_stalled_download() {
        let dir = scratch("stalled");
        let base = stalled_server();
        let manifest = Manifest {
            asset: format!("{base}/download/v9.0.0/{}", os::DOWNLOAD),
            size: 1_000_000,
            ..serde_json::from_value(manifest_json("9.0.0")).unwrap()
        };
        let dest = dir.join(os::DOWNLOAD);
        let started = std::time::Instant::now();
        let mut calls = 0;
        let err = super::download(&source(&base, ""), &manifest, &dest, |_| {
            calls += 1;
            calls < 5
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "Cancelled.");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "Cancel waited for the stalled connection"
        );
        assert!(!dest.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prepares_from_a_server_and_stops_between_steps() {
        let dir = scratch("engine");
        let release = release(&dir, "9.0.0");
        let app = dir.join("Applications").join(stand_in::APP);
        stand_in::app(&app, VERSION);
        let updates = dir.join("updates");

        let manifest = check(&release.source).unwrap().expect("no update offered");
        assert_eq!(manifest, release.manifest);

        let (scratch_dir, staged) = install_paths(&updates, &app, 1);
        let never = AtomicBool::new(false);
        let mut seen = 0;
        prepare(
            &release.source,
            &manifest,
            &scratch_dir,
            &staged,
            &never,
            |done| seen = done,
            |_| {},
        )
        .unwrap();
        assert_eq!(os::installed_version(&staged).unwrap(), "9.0.0");
        assert!(!scratch_dir.exists());
        super::remove(&staged);

        // Cancelled as each step starts: it stops after the step before,
        // and leaves neither scratch folder nor staged copy.
        for step in ["Verifying", "Staging"] {
            let (scratch_dir, staged) = install_paths(&updates, &app, 2);
            let cancel = AtomicBool::new(false);
            let err = prepare(
                &release.source,
                &manifest,
                &scratch_dir,
                &staged,
                &cancel,
                |_| {},
                |line| {
                    if line.starts_with(step) {
                        cancel.store(true, Ordering::Relaxed);
                    }
                },
            )
            .unwrap_err();
            assert_eq!(err.to_string(), "Cancelled.", "at {step}");
            assert!(!scratch_dir.exists(), "at {step}");
            assert!(!staged.exists(), "at {step}");
        }

        // A version the asset doesn't hold is caught at staging.
        let (scratch_dir, staged) = install_paths(&updates, &app, 3);
        let wrong = Manifest {
            version: "9.0.1".into(),
            ..manifest
        };
        let err = prepare(
            &release.source,
            &wrong,
            &scratch_dir,
            &staged,
            &never,
            |_| {},
            |_| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("holds version 9.0.0"), "{err}");
        assert!(!staged.exists() && !scratch_dir.exists());
        fs::remove_dir_all(release.dir).unwrap();
    }

    /// The card's Install and Relaunch handlers, driven against a local
    /// server and a stand-in installed app, as the buttons call them.
    #[cfg(target_os = "macos")]
    #[test]
    fn install_and_relaunch_handlers() {
        let dir = scratch("handlers");
        let release = release(&dir, "9.0.0");
        let app = dir.join("Applications").join(stand_in::APP);
        stand_in::app(&app, VERSION);

        use super::{State, Updater};
        use gpui_kit::AppContext;
        use std::sync::Arc;
        use std::time::Duration;
        let mut cx =
            gpui_kit::HeadlessAppContext::new(Arc::new(gpui_apple::AppleTextSystem::new()));
        let updater = cx.update(|cx| {
            cx.set_global(crate::Windows::default());
            cx.new(|_| Updater::new(release.source.clone(), app.clone(), dir.join("updates")))
        });
        // Runs tasks until the state passes `done`; the work runs on real processes.
        let settle = |cx: &mut gpui_kit::HeadlessAppContext, done: &dyn Fn(&State) -> bool| {
            for _ in 0..300 {
                cx.run_until_parked();
                let state = cx.update(|cx| updater.read(cx).state.clone());
                if done(&state) {
                    return state;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            panic!(
                "stuck at {:?}",
                cx.update(|cx| updater.read(cx).state.clone())
            );
        };

        cx.update(|cx| updater.update(cx, |u, cx| u.check(false, cx)));
        let state = settle(&mut cx, &|s| s != &State::Idle);
        assert_eq!(
            state,
            State::Available {
                manifest: release.manifest.clone(),
                in_place: true
            }
        );

        cx.update(|cx| updater.update(cx, |u, cx| u.install(cx)));
        let State::Ready(staged) = settle(&mut cx, &|s| matches!(s, State::Ready(_))) else {
            unreachable!()
        };
        assert!(staged.is_dir());
        assert_eq!(
            fs::read_dir(dir.join("updates")).unwrap().count(),
            0,
            "the download wasn't cleaned up"
        );

        // A second click while the first is under way does nothing.
        cx.update(|cx| {
            updater.update(cx, |u, cx| {
                u.relaunch(cx);
                assert_eq!(u.state, State::Relaunching);
                u.relaunch(cx);
            })
        });
        // The new version's script writes beside the folder it was built in.
        let launched = dir.join("content/launched.txt");
        for _ in 0..100 {
            cx.run_until_parked();
            if launched.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(launched.exists(), "the new version didn't start");
        assert_eq!(os::installed_version(&app).unwrap(), "9.0.0");
        assert_eq!(os::installed_version(&staged).unwrap(), VERSION);

        // Open files reach the new instance as arguments.
        fs::remove_file(&launched).unwrap();
        let files = [
            PathBuf::from("/tmp/sales 2024.csv"),
            PathBuf::from("/tmp/b.parquet"),
        ];
        super::relaunch(&app, &files).unwrap();
        let mut text = String::new();
        for _ in 0..50 {
            text = fs::read_to_string(&launched).unwrap_or_default();
            if !text.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(text, "/tmp/sales 2024.csv\n/tmp/b.parquet\n");
        fs::remove_dir_all(release.dir).unwrap();
    }

    /// Real clicks on the rendered card, through the window's event dispatch:
    /// Later takes the card away, and Install runs the install.
    /// Debug builds only: GPUI records the selectors the clicks aim at only
    /// there, so `cargo test --release` (the release workflow) skips this.
    #[cfg(all(target_os = "macos", debug_assertions))]
    #[test]
    fn clicks_on_the_card() {
        use super::{State, Updater};
        use gpui_kit::{AppContext, Modifiers, TestAppContext, TestDispatcher, VisualTestContext};
        use std::sync::Arc;

        let dir = scratch("clicks");
        let cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            Arc::new(gpui_apple::AppleTextSystem::new()),
        );
        let app_cx = cx.clone();
        let window = app_cx.update(|cx| {
            gpui_kit::init(cx);
            crate::document::init(cx);
            cx.set_global(crate::Windows::default());
            cx.set_global(crate::recent::RecentStore::load(dir.join("recent.json")));
            let opened = gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| crate::workspace::Workspace::new(None, window, cx))
            })
            .unwrap();
            cx.global_mut::<crate::Windows>().0.push(opened.clone());
            opened.0
        });
        // Nothing listens there: the download fails at once.
        let nowhere = source("http://127.0.0.1:1/releases", "");
        let manifest: Manifest = Manifest::parse(
            &manifest_json("0.2.1")
                .to_string()
                .replace(BASE, "http://127.0.0.1:1/releases"),
            &nowhere,
        )
        .unwrap();
        let updater = app_cx.update(|cx| {
            cx.new(|_| Updater::new(nowhere, dir.join("Data Analyzer.app"), dir.join("updates")))
        });
        let mut cx = VisualTestContext::from_window(window, &cx);
        let offer = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                updater.update(cx, |u, cx| {
                    u.state = State::Available {
                        manifest: manifest.clone(),
                        in_place: true,
                    };
                    u.show(cx);
                })
            });
            settle(cx);
        };
        let click = |cx: &mut VisualTestContext, selector: &'static str| {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("no {selector} on screen"));
            cx.simulate_click(bounds.center(), Modifiers::none());
        };

        offer(&mut cx);
        click(&mut cx, "update-later");
        settle(&mut cx);
        assert_eq!(app_cx.update(|cx| updater.read(cx).shown_in), None);
        assert!(cx.debug_bounds("update-later").is_none(), "the card stayed");

        offer(&mut cx);
        // The click runs the install to its end here: the download fails
        // against a closed port and the offer is gone. Unclicked, it would stay.
        click(&mut cx, "update-install");
        settle(&mut cx);
        assert_eq!(
            app_cx.update(|cx| updater.read(cx).state.clone()),
            State::Idle
        );
        fs::remove_dir_all(dir).unwrap();
    }

    /// Runs tasks, lets a second pass (toast animations), and draws a frame.
    /// Used by the click test only, so it shares that test's condition.
    #[cfg(all(target_os = "macos", debug_assertions))]
    fn settle(cx: &mut gpui_kit::VisualTestContext) {
        use std::time::Duration;
        for _ in 0..10 {
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
        }
    }
}
