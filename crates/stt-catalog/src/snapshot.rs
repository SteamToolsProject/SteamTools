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

/// key 源: 前两项双镜像对账; 之后单源回退 (github_raw → sudama → catmisteam).
/// catmisteam 体量更小 (调研 ~17.5 万), 只作末位回退, 可能有个别独有条目.
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
    ("catmisteam", "https://catmisteam.com/depotkeys.json"),
];

/// token 源: sudama 优先 (8173 条最全); ManifestHub 家族 (5090 条) 是 sudama
/// 的子集 (实测 0 独有), 只作 sudama 不可达时的 GitHub 回退.
const TOKEN_SOURCES: &[(&str, &str)] = &[
    ("sudama", "https://api.993499094.xyz/appaccesstokens.json"),
    (
        "manifesthub_jsdmirror",
        "https://cdn.jsdmirror.com/gh/steamtools-games/ManifestHub3@main/appaccesstokens.json",
    ),
    (
        "manifesthub_ghfast",
        "https://ghfast.top/https://raw.githubusercontent.com/SteamAutoCracks/ManifestHub/main/appaccesstokens.json",
    ),
    (
        "manifesthub_raw",
        "https://raw.githubusercontent.com/SteamAutoCracks/ManifestHub/main/appaccesstokens.json",
    ),
];

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

    // depot key 走双镜像对账; token 只有单源, 保持按序取用.
    let candidate = if matches!(kind, SnapshotKind::DepotKey) {
        reconciled_candidate(max_bytes, max_entries, sources, timeouts)
    } else {
        first_valid_candidate(kind, max_bytes, max_entries, sources, timeouts)
    };
    let Some((source, body)) = candidate else {
        return CommunitySnapshotState::Unavailable;
    };
    let Ok(entries) = validate_snapshot(&body, kind, max_entries) else {
        return CommunitySnapshotState::Unavailable;
    };
    if write_atomic(&path, &body).is_ok() {
        CommunitySnapshotState::Downloaded { source, entries }
    } else {
        CommunitySnapshotState::Unavailable
    }
}

/// 双镜像策略: 两个镜像都成功则逐 key 对账, 仅两源一致的 key 保留; 单镜像
/// 成功降级用该源; 都失败时回退到镜像之后的单源链 (github_raw 信任原站, 不对账).
/// 约定: sources 前两项是镜像, 之后的源作为单源回退链.
fn reconciled_candidate(
    max_bytes: usize,
    max_entries: usize,
    sources: &'static [(&'static str, &'static str)],
    timeouts: WinHttpTimeouts,
) -> Option<(&'static str, Vec<u8>)> {
    reconciled_candidate_with(max_entries, sources, |_, url| {
        fetch_valid_body(
            SnapshotKind::DepotKey,
            url,
            max_bytes,
            max_entries,
            timeouts,
        )
    })
}

fn reconciled_candidate_with<F>(
    max_entries: usize,
    sources: &'static [(&'static str, &'static str)],
    mut fetch: F,
) -> Option<(&'static str, Vec<u8>)>
where
    F: FnMut(&'static str, &'static str) -> Option<Vec<u8>>,
{
    let mirrors = sources.get(..2).unwrap_or(&[]);
    let mut valid: Vec<(&'static str, Vec<u8>)> = Vec::new();
    for &(source, url) in mirrors {
        if let Some(body) = fetch(source, url) {
            valid.push((source, body));
        }
    }
    if valid.len() == 2 {
        let (_, second) = valid.pop().unwrap();
        let (_, first) = valid.pop().unwrap();
        if let Some((body, dropped)) = reconcile_key_snapshots(&first, &second, max_entries) {
            diagnostic(format!(
                "depot keys reconciled from two mirrors, dropped {dropped} keys"
            ));
            return Some(("mirrors", body));
        }
        diagnostic("depot keys: mirrors disagree entirely, trusting origin");
    } else if valid.len() == 1 {
        let (source, body) = valid.pop().unwrap();
        diagnostic(format!("depot keys: single mirror {source} succeeded"));
        return Some((source, body));
    } else {
        diagnostic("depot keys: both mirrors failed, trusting origin");
    }
    // 镜像不可用时的单源回退链 (github_raw 等), 信任原站.
    for &(source, url) in sources.get(2..).unwrap_or(&[]) {
        if let Some(body) = fetch(source, url) {
            return Some((source, body));
        }
    }
    None
}

/// 单源策略: 按顺序返回第一个校验通过的源.
fn first_valid_candidate(
    kind: SnapshotKind,
    max_bytes: usize,
    max_entries: usize,
    sources: &'static [(&'static str, &'static str)],
    timeouts: WinHttpTimeouts,
) -> Option<(&'static str, Vec<u8>)> {
    for &(source, url) in sources {
        if let Some(body) = fetch_valid_body(kind, url, max_bytes, max_entries, timeouts) {
            return Some((source, body));
        }
    }
    None
}

/// 取回单份快照并校验, 失败返回 None (限长与条目上限与原逻辑一致).
fn fetch_valid_body(
    kind: SnapshotKind,
    url: &str,
    max_bytes: usize,
    max_entries: usize,
    timeouts: WinHttpTimeouts,
) -> Option<Vec<u8>> {
    let response = winhttp_get(
        url,
        WinHttpGetOptions {
            timeouts,
            max_body_bytes: max_bytes,
        },
    )
    .ok()?;
    if !(200..300).contains(&response.status) {
        return None;
    }
    validate_snapshot(&response.body, kind, max_entries).ok()?;
    Some(response.body)
}

/// 逐 key 对账两个镜像正文: 两源键值完全一致的 key 保留, 其余丢弃.
/// 返回 (对账后正文, 丢弃的 key 数); 对账结果为空时返回 None 由调用方回退.
fn reconcile_key_snapshots(
    first: &[u8],
    second: &[u8],
    max_entries: usize,
) -> Option<(Vec<u8>, usize)> {
    let first: std::collections::HashMap<String, String> = serde_json::from_slice(first).ok()?;
    let second: std::collections::HashMap<String, String> = serde_json::from_slice(second).ok()?;
    let mut kept: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (key, value) in &first {
        if second.get(key) == Some(value) {
            kept.insert(key.clone(), value.clone());
        }
    }
    if kept.is_empty() || kept.len() > max_entries {
        return None;
    }
    let dropped = first
        .len()
        .saturating_sub(kept.len())
        .saturating_add(second.len().saturating_sub(kept.len()));
    let body = serde_json::to_vec(&kept).ok()?;
    Some((body, dropped))
}

// 本 crate 无日志依赖, 诊断走 stderr (Steam 无控制台时自动丢弃), 前缀便于过滤.
fn diagnostic(message: impl std::fmt::Display) {
    eprintln!("steamtools snapshot: {message}");
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
            ["jsdmirror", "ghfast", "github_raw", "sudama", "catmisteam"]
        );
    }

    /// sudama 最全 (8173), 必须排最前; ManifestHub 家族只作回退.
    #[test]
    fn token_snapshot_sudama_takes_priority_over_manifesthub_fallback() {
        assert_eq!(
            TOKEN_SOURCES
                .iter()
                .map(|(source, _)| *source)
                .collect::<Vec<_>>(),
            [
                "sudama",
                "manifesthub_jsdmirror",
                "manifesthub_ghfast",
                "manifesthub_raw"
            ]
        );
    }

    #[test]
    fn mirrors_agree_keeps_all_keys() {
        let body = format!(
            r#"{{"42":"{}","43":"{}"}}"#,
            "ab".repeat(32),
            "cd".repeat(32)
        );

        let (source, merged) = reconciled_candidate_with(10, KEY_SOURCES, |source, _| {
            (source == "jsdmirror" || source == "ghfast").then(|| body.as_bytes().to_vec())
        })
        .unwrap();

        assert_eq!(source, "mirrors");
        let entries = validate_snapshot(&merged, SnapshotKind::DepotKey, 10).unwrap();
        assert_eq!(entries, 2);
    }

    #[test]
    fn mirrors_disagree_drops_differing_keys() {
        let first = format!(
            r#"{{"42":"{}","43":"{}"}}"#,
            "ab".repeat(32),
            "cd".repeat(32)
        );
        let second = format!(
            r#"{{"42":"{}","43":"{}"}}"#,
            "ab".repeat(32),
            "ef".repeat(32)
        );

        let (source, merged) = reconciled_candidate_with(10, KEY_SOURCES, |source, _| {
            let body = if source == "jsdmirror" {
                &first
            } else {
                &second
            };
            (source == "jsdmirror" || source == "ghfast").then(|| body.as_bytes().to_vec())
        })
        .unwrap();

        assert_eq!(source, "mirrors");
        let kept: std::collections::HashMap<String, String> =
            serde_json::from_slice(&merged).unwrap();
        assert!(kept.contains_key("42"));
        assert!(!kept.contains_key("43"));
    }

    #[test]
    fn single_mirror_success_is_used() {
        let body = format!(r#"{{"42":"{}"}}"#, "ab".repeat(32));

        let (source, _) = reconciled_candidate_with(10, KEY_SOURCES, |source, _| {
            (source == "ghfast").then(|| body.as_bytes().to_vec())
        })
        .unwrap();

        assert_eq!(source, "ghfast");
    }

    #[test]
    fn both_mirrors_fail_falls_back_to_github_raw() {
        let body = format!(r#"{{"42":"{}"}}"#, "ab".repeat(32));

        let (source, _) = reconciled_candidate_with(10, KEY_SOURCES, |source, _| {
            (source == "github_raw").then(|| body.as_bytes().to_vec())
        })
        .unwrap();

        assert_eq!(source, "github_raw");
    }

    #[test]
    fn all_key_sources_fail_is_unavailable() {
        assert!(reconciled_candidate_with(10, KEY_SOURCES, |_, _| None).is_none());
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
