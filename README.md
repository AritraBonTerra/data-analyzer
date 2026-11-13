<img src="assets/AppIcon.svg" width="96" height="96" alt="">

# Data Analyzer

A free, native app for macOS and Linux for opening and exploring data files: CSV, TSV, Excel (xlsx/xls), Parquet and JSON/NDJSON. Double-click a file and you get a fast table with column stats, sorting, filtering, search and SQL. Multi-GB files open instantly and finish loading in seconds.

![Data Analyzer showing a 30M-row CSV with the stats panel](docs/screenshots/stats-histogram-light.png)

![A report-style CSV opened with its title lines set aside](docs/screenshots/import-strip.png)

![The chart panel drawing revenue by month and region](docs/screenshots/chart-line.png)

## Features
- **Instant preview, background load.** The first rows of a CSV/JSON file show up right away while the full file loads into DuckDB, with progress and a cancel button.
- **Virtualized grid** that scrolls through tens of millions of rows. Column headers show type, null % and a mini distribution.
- **Sort, filter, search.** Click headers to sort (shift-click for a secondary sort). Use column menus for filters and ⌘F to search every column.
- **Stats panel** (⌘I): summary stats, a histogram or top values, and click-to-filter.
- **SQL** (⌘E): the file is the table `data`, and results open in the same grid.
- **Chart panel** (⇧⌘C): bar, line, scatter or histogram of what the grid shows, picked from dropdowns, with click-to-filter.
- On Linux every shortcut uses Ctrl where macOS uses ⌘ (Ctrl+F, Ctrl+I, Ctrl+E, Ctrl+Shift+C…), and the menus are in each window's title bar.
- **Export** the current view to CSV, Parquet or JSON Lines.
- **Values read as written.** Text columns of `$1,234.00`, `12.6%`, `1.424,50`, mixed date spellings or Yes/No load as numbers, dates and booleans, shown the way the file wrote them; a column with one value that doesn't fit stays text. Files in Windows-1252 or UTF-16 open too.
- **Finds the table.** Titles, notes and totals rows around a table are set aside and kept under File notes; multi-row headers are joined; two tables in one file or sheet become two tabs. One line under the toolbar says what was done, and *Adjust import* corrects a wrong guess.
- **Nested data.** A JSON file that wraps its records opens as rows, and nested objects in JSON and Parquet open into columns like `customer.address.city`.
- **Excel sheets** as tabs. **Default app** for data files via *Data Analyzer → Make Default for Data Files…* (on Linux, *Help → Make Default for Data Files…*, which also adds the app to your app menu).
- **Settings** (⌘, or Ctrl+,): light, dark or system appearance, four themes, and every shortcut can be changed. **Zoom** with ⌘+ / ⌘− / ⌘0 makes text, rows and panels bigger or smaller in every window.

Measured on an M4 Pro with a 2.4 GB, 30M-row CSV (release build, warm disk): first rows in 170 ms, full load in 3.5 s, any page in about 4 ms, sort in 2.2 s, search in 1 s.

## Install
**macOS** (Apple Silicon): download the DMG from the [Releases page](https://github.com/AritraBonTerra/data-analyzer/releases) and drag Data Analyzer to Applications.
Releases from 0.2.1 on are signed with a Developer ID and notarized by Apple, so macOS opens them without a warning.

**Linux** (x86_64 or aarch64, glibc 2.35 or newer: Ubuntu 22.04, Debian 12, Fedora 36 and later): download
`Data-Analyzer-x86_64.AppImage` (or `-aarch64`) from the Releases page into a folder you own, such as `~/Applications`, then run it:
```sh
chmod +x Data-Analyzer-x86_64.AppImage && ./Data-Analyzer-x86_64.AppImage
```
*Help → Make Default for Data Files…* adds it to your app menu and makes it open CSV, Excel, Parquet and JSON files.
It needs FUSE (`fusermount3`), which desktop distributions ship; without it, run it with `--appimage-extract-and-run`.

Updates: each time the app opens it asks GitHub whether a newer release exists, and offers to install it. Install downloads the new version,
checks its signature, replaces the app (the `.app`, or the AppImage file) and relaunches with the same files open. Nothing but that one request is sent.
"Check for Updates…" in the app menu (Help on Linux) checks on demand, and `data-analyzer --update` (or `./Data-Analyzer-x86_64.AppImage --update`) does the same from a terminal.

## Layout
- `crates/core` (`da-core`): the data engine. DuckDB for CSV/TSV/Parquet/JSON, calamine for Excel. No UI code.
- `crates/app` (`data-analyzer`): the GPUI interface, built with [gpui-kit](https://github.com/longbridge/gpui-kit). What differs between macOS and Linux is in `src/platform/` (folders, default app, one running instance on Linux) and `src/update/` (how each update step is done).
- `packaging/Info.plist`, `scripts/bundle.sh`: macOS app bundle, file associations, signing and notarization.
- `packaging/linux/`, `scripts/appimage.sh`: the Linux desktop entry, MIME types and AppImage.
- `crates/app/src/update.rs`: the updater. `.github/workflows/`, `scripts/release-*.sh`: checks and releases on GitHub.

## Develop
On Linux, first install the windowing libraries GPUI links: `scripts/linux-deps.sh` (Debian/Ubuntu).
```sh
cargo run -p data-analyzer -- path/to/file.csv   # use --release for big files
cargo test --workspace
cargo clippy --workspace --all-targets && cargo fmt --all
```

Screenshots: `cargo test -p data-analyzer shots -- --ignored --nocapture` renders each
scenario in `crates/app/src/shots.rs` to a PNG offscreen (no window, no screen-recording
permission), into `target/shots` or `$DA_SHOTS_DIR`. `DA_SHOT=<name>` renders just one.
Add a scenario there to look at a UI change.

## Build the app
```sh
scripts/bundle.sh               # macOS: dist/Data Analyzer.app + DMG, signed with your Developer ID
scripts/bundle.sh --notarize    # also notarize + staple (needs a notarytool profile, see the script header)
scripts/appimage.sh             # Linux: dist/Data-Analyzer-<arch>.AppImage
```

## Releasing
Every pull request and every push to `main` runs format, lint and tests on macOS and Linux (`.github/workflows/ci.yml`).
To release, bump `version` in `crates/app/Cargo.toml` (and `crates/core/Cargo.toml`) in a pull request and merge it.
A merge that leaves the version alone releases nothing: each push to `main` runs `.github/workflows/release.yml`,
which stops after its first job when `v<version>` is already released.

For a new version it checks, tests, builds, signs and notarizes the macOS app, and checks, tests and
builds the Linux AppImages (x86_64 and aarch64, each on a runner of that architecture), all on GitHub. It tags the merged commit `v<version>` and publishes a release with
notes generated from the merged pull requests and, per platform, the app and the manifest its updater reads (version,
download link, size, SHA-256 and update signature): `Data-Analyzer-<version>.dmg` with `latest.json`, and
`Data-Analyzer-<arch>.AppImage` with `latest-linux-<arch>.json`. The run is under the Actions tab and takes about 25 minutes.
If any step fails, nothing is published.

Trial run: push a tag like `v0.2.2-dryrun.1` (`v` plus the version, from any branch). It runs every step but publishing, and the DMG, the AppImages and
their manifests appear as artifacts on the workflow run. Delete the tag afterwards
(`git tag -d v0.2.2-dryrun.1 && git push origin --delete v0.2.2-dryrun.1`).

Update key: each DMG and AppImage is signed with an SSH key, and the signature goes in its manifest. The public half is `packaging/update-key.pub`;
the private half is the repo secret `UPDATE_SIGNING_KEY`. GitHub never shows a secret again, so keep a copy of the private key
somewhere safe. If it is lost, make a new pair;
installed apps then need one manual reinstall of a build that carries the new public key.

Apple signing: the release run signs with a Developer ID Application certificate and notarizes through an App Store Connect
API key. Repo secrets: `MACOS_CERT_P12` (the certificate and its private key as a base64 `.p12`), `MACOS_CERT_PASSWORD`,
`NOTARY_KEY` (the `.p8` file), `NOTARY_KEY_ID` and `NOTARY_ISSUER_ID`. When the certificate expires, export the renewed one
and set the first two again: `base64 -i cert.p12 | gh secret set MACOS_CERT_P12`.
