//! Steam 本地 appinfo 缓存里的最小名称读取器.
//!
//! 这里只读 `appcache/appinfo.vdf`, 不把完整二进制 VDF 载入配置状态. Steam 的
//! appinfo 使用键表编号, `common/name` 的键编号在当前格式中是 4; 其余字段不关心.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

const APPINFO_PATH: &str = "appcache/appinfo.vdf";
const COMMON_NAME_SUFFIX: [u8; 10] = [0x00, 0x03, 0x00, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00];
const MAX_NAME_LEN: usize = 256;

/// 从 Steam 本地缓存读取指定 AppId 的显示名称.
pub fn app_names(steam_root: &Path, app_ids: &[u32]) -> BTreeMap<u32, String> {
    let path = steam_root.join(APPINFO_PATH);
    let Ok(bytes) = std::fs::read(path) else {
        return BTreeMap::new();
    };

    let wanted: HashSet<u32> = app_ids.iter().copied().collect();
    let mut names = BTreeMap::new();
    let suffix_len = COMMON_NAME_SUFFIX.len();
    let pattern_len = 4 + suffix_len;
    let Some(last_offset) = bytes.len().checked_sub(pattern_len) else {
        return names;
    };
    if wanted.is_empty() {
        return names;
    }
    for offset in 0..=last_offset {
        if bytes[offset + 4..offset + 4 + suffix_len] != COMMON_NAME_SUFFIX {
            continue;
        }
        let app_id = u32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]);
        if wanted.contains(&app_id) {
            if let Some(name) = read_name(&bytes, offset + pattern_len) {
                names.insert(app_id, name);
            }
        }
    }
    names
}

#[cfg(test)]
fn find_name(bytes: &[u8], app_id: u32) -> Option<String> {
    let mut pattern = [0u8; 14];
    pattern[..4].copy_from_slice(&app_id.to_le_bytes());
    pattern[4..].copy_from_slice(&COMMON_NAME_SUFFIX);
    let offset = bytes
        .windows(pattern.len())
        .position(|window| window == pattern)?;
    read_name(bytes, offset + pattern.len())
}

fn read_name(bytes: &[u8], start: usize) -> Option<String> {
    let relative_end = bytes[start..bytes.len().min(start + MAX_NAME_LEN)]
        .iter()
        .position(|byte| *byte == 0)?;
    let raw = &bytes[start..start + relative_end];
    let Ok(name) = std::str::from_utf8(raw) else {
        return None;
    };
    let name = name.trim();
    if name.is_empty() || name.chars().any(char::is_control) {
        return None;
    }
    Some(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_name_from_binary_appinfo_shape() {
        let mut bytes = 730u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&COMMON_NAME_SUFFIX);
        bytes.extend_from_slice(b"Example Game\0");
        assert_eq!(find_name(&bytes, 730).as_deref(), Some("Example Game"));
    }

    #[test]
    fn ignores_invalid_or_missing_names() {
        let mut bytes = 42u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&COMMON_NAME_SUFFIX);
        bytes.extend_from_slice(b"\xff\0");
        assert!(find_name(&bytes, 42).is_none());
        assert!(find_name(&bytes, 43).is_none());
    }

    #[test]
    fn reads_requested_names_from_steam_root_cache() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(APPINFO_PATH);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = 730u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&COMMON_NAME_SUFFIX);
        bytes.extend_from_slice(b"Example Game\0");
        std::fs::write(path, bytes).unwrap();

        let names = app_names(root.path(), &[730, 731]);
        assert_eq!(names.get(&730).map(String::as_str), Some("Example Game"));
        assert!(!names.contains_key(&731));
    }
}
