//! Debounced file-change detection (poll-based, no extra deps).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    mtime: Option<SystemTime>,
    len: Option<u64>,
    /// Cheap fingerprint so same-length edits still count on coarse FS clocks.
    content_hash: u64,
}

impl FileStamp {
    fn of(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => {
                let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
                Self {
                    mtime,
                    len: Some(bytes.len() as u64),
                    content_hash: fnv1a64(&bytes),
                }
            }
            Err(_) => Self {
                mtime: None,
                len: None,
                content_hash: 0,
            },
        }
    }
}

fn fnv1a64(data: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut h = OFFSET;
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(PRIME);
    }
    h
}

#[derive(Debug, Clone)]
struct Tracked {
    path: PathBuf,
    stamp: FileStamp,
    pending_since: Option<Instant>,
}

/// Polls paths; after `debounce` of continuous "dirty", reports changed paths.
#[derive(Debug)]
pub struct DebouncedWatcher {
    debounce: Duration,
    tracked: Vec<Tracked>,
}

impl DebouncedWatcher {
    pub fn new(paths: impl IntoIterator<Item = PathBuf>, debounce: Duration) -> Self {
        let tracked = paths
            .into_iter()
            .map(|path| Tracked {
                stamp: FileStamp::of(&path),
                path,
                pending_since: None,
            })
            .collect();
        Self { debounce, tracked }
    }

    pub fn watch_file(path: impl Into<PathBuf>, debounce: Duration) -> Self {
        Self::new([path.into()], debounce)
    }

    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.tracked.iter().map(|t| t.path.as_path())
    }

    pub fn path_list(&self) -> Vec<PathBuf> {
        self.tracked.iter().map(|t| t.path.clone()).collect()
    }

    /// Replace the tracked set (e.g. after rediscovering `.lua` files).
    pub fn set_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        self.tracked = paths
            .into_iter()
            .map(|path| Tracked {
                stamp: FileStamp::of(&path),
                path,
                pending_since: None,
            })
            .collect();
    }

    /// Call periodically. Returns paths whose change has been stable for `debounce`.
    pub fn poll(&mut self) -> Vec<PathBuf> {
        let now = Instant::now();
        let mut fired = Vec::new();

        for t in &mut self.tracked {
            let current = FileStamp::of(&t.path);
            if current != t.stamp && t.pending_since.is_none() {
                t.pending_since = Some(now);
            }

            if let Some(since) = t.pending_since {
                let still_dirty = current != t.stamp;
                if still_dirty && now.duration_since(since) >= self.debounce {
                    t.stamp = current;
                    t.pending_since = None;
                    fired.push(t.path.clone());
                } else if !still_dirty {
                    t.pending_since = None;
                }
            }
        }

        fired
    }

    /// Force re-read stamps without firing (e.g. after external reload).
    pub fn resync(&mut self) {
        for t in &mut self.tracked {
            t.stamp = FileStamp::of(&t.path);
            t.pending_since = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::thread;

    #[test]
    fn debounce_fires_after_quiet_period() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("steamtools.toml");
        fs::write(&path, "a = 1\n").unwrap();

        let mut w = DebouncedWatcher::watch_file(&path, Duration::from_millis(50));
        assert!(w.poll().is_empty());

        fs::write(&path, "a = 2\n").unwrap();
        // Immediate poll: dirty but not yet debounced.
        assert!(w.poll().is_empty());

        thread::sleep(Duration::from_millis(80));
        let changed = w.poll();
        assert_eq!(changed, vec![path]);
        assert!(w.poll().is_empty());
    }
}
