//! 防抖文件变更检测 (轮询, 无额外依赖).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    mtime: Option<SystemTime>,
    len: Option<u64>,
    /// 廉价内容指纹: 文件系统时间粒度粗时, 同长度修改也能发现.
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

/// 轮询路径; 持续 dirty 满 `debounce` 后报告变更.
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

    /// 替换监视集合 (例如重新发现 `.lua` 之后).
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

    /// 周期性调用; 返回已稳定满 `debounce` 的变更路径.
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

    /// 强制重读戳记且不触发 (例如外部已 reload).
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
        // 立刻 poll: 已 dirty 但防抖未满.
        assert!(w.poll().is_empty());

        thread::sleep(Duration::from_millis(80));
        let changed = w.poll();
        assert_eq!(changed, vec![path]);
        assert!(w.poll().is_empty());
    }
}
