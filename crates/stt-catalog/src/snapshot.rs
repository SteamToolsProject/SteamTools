//! Community 大快照的后台缓存准备.

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::de::{DeserializeSeed, MapAccess, Visitor};
use stt_platform::{winhttp_get, WinHttpGetOptions, WinHttpTimeouts};

const DEPOT_KEYS_FILE: &str = "depotkeys.json";
const APP_TOKENS_FILE: &str = "appaccesstokens.json";
const MAX_KEY_BYTES: usize = 32 * 1024 * 1024;
const MAX_TOKEN_BYTES: usize = 4 * 1024 * 1024;
const MAX_KEY_ENTRIES: usize = 500_000;
const MAX_TOKEN_ENTRIES: usize = 100_000;

const KEY_SOURCES: &[(&str, &str)] = &[
    (
        "jsdmirror",
        "https://cdn.jsdmirror.com/gh/AQiaoYo/ManifestHub@main/depotkeys.json",
    ),
    (
        "ghfast",
        "https://ghfast.top/https://raw.githubusercontent.com/AQiaoYo/ManifestHub/main/depotkeys.json",
    ),
    (
        "github_raw",
        "https://raw.githubusercontent.com/AQiaoYo/ManifestHub/main/depotkeys.json",
    ),
    ("sudama", "https://api.993499094.xyz/depotkeys.json"),
];

const TOKEN_SOURCES: &[(&str, &str)] =
    &[("sudama", "https://api.993499094.xyz/appaccesstokens.json")];

/// 单份 Community 快照的准备状态.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommunitySnapshotState {
    Cached {
        entries: usize,
    },
    Downloaded {
        source: &'static str,
        entries: usize,
    },
    Unavailable,
}

/// key/token 两份快照的后台准备结果.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommunitySnapshotReport {
    pub depot_keys: CommunitySnapshotState,
    pub access_tokens: CommunitySnapshotState,
}

/// 校验已有缓存, 缺失时按内置顺序下载并原子落盘.
///
/// # Errors
///
/// 失败会收敛为对应快照的 [`CommunitySnapshotState::Unavailable`], 不暴露正文.
pub fn ensure_community_snapshots(
    cache_dir: &Path,
    timeouts: WinHttpTimeouts,
) -> CommunitySnapshotReport {
    CommunitySnapshotReport {
        depot_keys: ensure_snapshot(
            cache_dir.join(DEPOT_KEYS_FILE),
            SnapshotKind::DepotKey,
            MAX_KEY_BYTES,
            MAX_KEY_ENTRIES,
            KEY_SOURCES,
            timeouts,
        ),
        access_tokens: ensure_snapshot(
            cache_dir.join(APP_TOKENS_FILE),
            SnapshotKind::AccessToken,
            MAX_TOKEN_BYTES,
            MAX_TOKEN_ENTRIES,
            TOKEN_SOURCES,
            timeouts,
        ),
    }
}

#[derive(Debug, Clone, Copy)]
enum SnapshotKind {
    DepotKey,
    AccessToken,
}

fn ensure_snapshot(
    path: PathBuf,
    kind: SnapshotKind,
    max_bytes: usize,
    max_entries: usize,
    sources: &'static [(&'static str, &'static str)],
    timeouts: WinHttpTimeouts,
) -> CommunitySnapshotState {
    if let Ok(body) = read_bounded(&path, max_bytes) {
        if let Ok(entries) = validate_snapshot(&body, kind, max_entries) {
            return CommunitySnapshotState::Cached { entries };
        }
    }

    for &(source, url) in sources {
        let response = match winhttp_get(
            url,
            WinHttpGetOptions {
                timeouts,
                max_body_bytes: max_bytes,
            },
        ) {
            Ok(response) if (200..300).contains(&response.status) => response,
            _ => continue,
        };
        let Ok(entries) = validate_snapshot(&response.body, kind, max_entries) else {
            continue;
        };
        if write_atomic(&path, &response.body).is_ok() {
            return CommunitySnapshotState::Downloaded { source, entries };
        }
    }
    CommunitySnapshotState::Unavailable
}

fn read_bounded(path: &Path, limit: usize) -> std::io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut body = Vec::new();
    file.take(limit.saturating_add(1) as u64)
        .read_to_end(&mut body)?;
    if body.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "snapshot exceeds limit",
        ));
    }
    Ok(body)
}

fn write_atomic(path: &Path, body: &[u8]) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "snapshot path has no parent",
        ));
    };
    std::fs::create_dir_all(parent)?;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".next");
    let temporary = path.with_file_name(name);
    std::fs::write(&temporary, body)?;
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error)
            if cfg!(windows)
                && path.exists()
                && error.kind() == std::io::ErrorKind::AlreadyExists =>
        {
            // Windows 的 rename 不能覆盖已有文件, 先移除旧缓存再完成替换.
            if let Err(remove_error) = std::fs::remove_file(path) {
                let _ = std::fs::remove_file(&temporary);
                return Err(remove_error);
            }
            std::fs::rename(&temporary, path).inspect_err(|_| {
                let _ = std::fs::remove_file(&temporary);
            })
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}

fn validate_snapshot(
    body: &[u8],
    kind: SnapshotKind,
    max_entries: usize,
) -> Result<usize, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let entries = SnapshotValidationSeed { kind, max_entries }.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(entries)
}

struct SnapshotValidationSeed {
    kind: SnapshotKind,
    max_entries: usize,
}

impl<'de> DeserializeSeed<'de> for SnapshotValidationSeed {
    type Value = usize;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(SnapshotValidationVisitor {
            kind: self.kind,
            max_entries: self.max_entries,
        })
    }
}

struct SnapshotValidationVisitor {
    kind: SnapshotKind,
    max_entries: usize,
}

impl<'de> Visitor<'de> for SnapshotValidationVisitor {
    type Value = usize;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a snapshot object keyed by decimal Steam IDs")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut seen = HashSet::new();
        while let Some((id, value)) = map.next_entry::<String, String>()? {
            let id = id
                .parse::<u32>()
                .map_err(|_| serde::de::Error::custom("invalid Steam ID"))?;
            if id == 0 {
                if matches!(self.kind, SnapshotKind::AccessToken) {
                    continue;
                }
                return Err(serde::de::Error::custom("zero Steam ID"));
            }
            if !seen.insert(id) {
                return Err(serde::de::Error::custom("duplicate Steam ID"));
            }
            if seen.len() > self.max_entries {
                return Err(serde::de::Error::custom("snapshot has too many entries"));
            }
            let valid = match self.kind {
                SnapshotKind::DepotKey => {
                    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                }
                SnapshotKind::AccessToken => {
                    !value.is_empty()
                        && value.bytes().all(|byte| byte.is_ascii_digit())
                        && value.parse::<u64>().is_ok()
                }
            };
            if !valid {
                return Err(serde::de::Error::custom("invalid snapshot value"));
            }
        }
        if seen.is_empty() {
            return Err(serde::de::Error::custom("snapshot is empty"));
        }
        Ok(seen.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_snapshot_sources_follow_measured_availability_order() {
        assert_eq!(
            KEY_SOURCES
                .iter()
                .map(|(source, _)| *source)
                .collect::<Vec<_>>(),
            ["jsdmirror", "ghfast", "github_raw", "sudama"]
        );
    }

    #[test]
    fn validates_depot_key_snapshot_without_exposing_values() {
        let body = format!(r#"{{"43":"{}"}}"#, "ab".repeat(32));

        let entries = validate_snapshot(body.as_bytes(), SnapshotKind::DepotKey, 10).unwrap();

        assert_eq!(entries, 1);
    }

    #[test]
    fn rejects_invalid_or_duplicate_snapshot_entries() {
        let invalid = br#"{"43":"bad"}"#;
        let duplicate = br#"{"42":"1","42":"2"}"#;

        assert!(validate_snapshot(invalid, SnapshotKind::DepotKey, 10).is_err());
        assert!(validate_snapshot(duplicate, SnapshotKind::AccessToken, 10).is_err());
    }

    #[test]
    fn rejects_snapshot_entry_count_over_limit() {
        let body = br#"{"42":"1","43":"2"}"#;

        let error = validate_snapshot(body, SnapshotKind::AccessToken, 1).unwrap_err();

        assert!(error.to_string().contains("too many"));
    }

    #[test]
    fn accepts_zero_token_as_an_explicit_miss() {
        let body = br#"{"42":"0"}"#;

        let entries = validate_snapshot(body, SnapshotKind::AccessToken, 1).unwrap();

        assert_eq!(entries, 1);
    }

    #[test]
    fn ignores_zero_app_id_token_placeholder() {
        let body = br#"{"0":"123","42":"1"}"#;

        let entries = validate_snapshot(body, SnapshotKind::AccessToken, 1).unwrap();

        assert_eq!(entries, 1);
    }

    #[test]
    fn replaces_existing_snapshot_file() {
        let directory =
            std::env::temp_dir().join(format!("steamtools-snapshot-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(DEPOT_KEYS_FILE);
        std::fs::write(&path, b"old").unwrap();

        write_atomic(&path, b"new").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(!path.with_file_name("depotkeys.json.next").exists());
        let _ = std::fs::remove_dir_all(directory);
    }
}
