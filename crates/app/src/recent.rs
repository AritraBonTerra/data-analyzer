//! The Open Recent list, persisted as JSON in Application Support.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How many files the list remembers.
const CAPACITY: usize = 12;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RecentFiles {
    pub files: Vec<RecentFile>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecentFile {
    pub path: PathBuf,
    /// Unix seconds; newest first in the list.
    pub opened_at: u64,
}

impl RecentFiles {
    /// Reads the list from `file`, or starts empty if it's missing or unreadable.
    pub fn load(file: &Path) -> Self {
        std::fs::read(file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, file: &Path) -> anyhow::Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(file, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    /// Moves `path` to the front, dropping duplicates and the oldest beyond capacity.
    pub fn push(&mut self, path: &Path) {
        self.push_at(path, now());
    }

    fn push_at(&mut self, path: &Path, opened_at: u64) {
        self.files.retain(|f| f.path != path);
        self.files.insert(
            0,
            RecentFile {
                path: path.to_path_buf(),
                opened_at,
            },
        );
        self.files.truncate(CAPACITY);
    }

    pub fn remove(&mut self, path: &Path) {
        self.files.retain(|f| f.path != path);
    }

    pub fn clear(&mut self) {
        self.files.clear();
    }

    /// Entries whose file still exists, newest first.
    pub fn existing(&self) -> impl Iterator<Item = &RecentFile> {
        self.files.iter().filter(|f| f.path.exists())
    }
}

/// App-wide recent list: loaded once at startup and saved after every change.
pub struct RecentStore {
    pub files: RecentFiles,
    file: PathBuf,
}

impl gpui_kit::Global for RecentStore {}

impl RecentStore {
    pub fn load(file: PathBuf) -> Self {
        Self {
            files: RecentFiles::load(&file),
            file,
        }
    }

    /// Applies `edit` and writes the list back to disk.
    pub fn edit(&mut self, edit: impl FnOnce(&mut RecentFiles)) {
        edit(&mut self.files);
        if let Err(err) = self.files.save(&self.file) {
            eprintln!("couldn't save recent files: {err:#}");
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_dedups_and_caps() {
        let mut recent = RecentFiles::default();
        for i in 0..20 {
            recent.push_at(Path::new(&format!("/f/{i}.csv")), i);
        }
        assert_eq!(recent.files.len(), CAPACITY);
        assert_eq!(recent.files[0].path, Path::new("/f/19.csv"));

        recent.push_at(Path::new("/f/10.csv"), 99);
        assert_eq!(recent.files[0].path, Path::new("/f/10.csv"));
        assert_eq!(
            recent
                .files
                .iter()
                .filter(|f| f.path.ends_with("10.csv"))
                .count(),
            1
        );
        assert_eq!(recent.files.len(), CAPACITY);
    }

    #[test]
    fn round_trips_through_json() {
        let dir = std::env::temp_dir().join(format!("da-recent-{}", std::process::id()));
        let file = dir.join("nested/recent.json");
        let mut recent = RecentFiles::default();
        recent.push_at(Path::new("/tmp/a.csv"), 1);
        recent.push_at(Path::new("/tmp/b.csv"), 2);
        recent.save(&file).unwrap();
        assert_eq!(RecentFiles::load(&file), recent);
        assert_eq!(
            RecentFiles::load(&dir.join("missing.json")),
            RecentFiles::default()
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
