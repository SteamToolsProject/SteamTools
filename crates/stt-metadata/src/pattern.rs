//! Pattern TOML 子集 + 字节特征扫描.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{MetadataError, Result};
use crate::fnv::fnv1a32_str;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternEntry {
    pub name: String,
    pub rva: Option<u64>,
    pub sig: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PatternMap {
    /// FNV-1a-32(name) → 条目
    by_hash: HashMap<u32, PatternEntry>,
    component: String,
    source: Option<PathBuf>,
}

impl PatternMap {
    pub fn component(&self) -> &str {
        &self.component
    }

    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }

    pub fn get_by_name(&self, name: &str) -> Option<&PatternEntry> {
        self.by_hash.get(&fnv1a32_str(name))
    }

    pub fn get_by_hash(&self, hash: u32) -> Option<&PatternEntry> {
        self.by_hash.get(&hash)
    }

    pub fn parse_str(component: impl Into<String>, text: &str) -> Result<Self> {
        let value: toml::Value = text.parse().map_err(MetadataError::Toml)?;
        let table = value
            .as_table()
            .ok_or_else(|| MetadataError::Invalid("root must be a table".into()))?;

        let mut by_hash = HashMap::new();
        for (key, val) in table {
            let hash = parse_hash_key(key)?;
            let entry_table = val
                .as_table()
                .ok_or_else(|| MetadataError::Invalid(format!("section {key} must be a table")))?;
            let name = entry_table
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let rva = entry_table
                .get("rva")
                .and_then(|v| v.as_str())
                .map(parse_hex_u64)
                .transpose()?
                .filter(|&r| r != 0);
            let sig = entry_table
                .get("sig")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty());

            by_hash.insert(hash, PatternEntry { name, rva, sig });
        }

        Ok(Self {
            by_hash,
            component: component.into(),
            source: None,
        })
    }

    pub fn load_file(component: impl Into<String>, path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| MetadataError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut map = Self::parse_str(component, &text)?;
        map.source = Some(path.to_path_buf());
        Ok(map)
    }
}

fn parse_hash_key(key: &str) -> Result<u32> {
    let s = key.trim();
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u32::from_str_radix(s, 16)
        .map_err(|_| MetadataError::Invalid(format!("bad section key '{key}'")))
}

fn parse_hex_u64(s: &str) -> Result<u64> {
    let s = s.trim();
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    if s.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(s, 16).map_err(|_| MetadataError::Invalid(format!("bad hex '{s}'")))
}

/// 解析后的 IDA 风格特征: `48 89 ?? 5C`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteSig {
    pub bytes: Vec<u8>,
    pub mask: Vec<bool>, // true = must match
}

impl ByteSig {
    pub fn parse(sig: &str) -> Result<Self> {
        let mut bytes = Vec::new();
        let mut mask = Vec::new();
        for tok in sig.split_whitespace() {
            if tok == "??" || tok == "?" {
                bytes.push(0);
                mask.push(false);
            } else {
                let b = u8::from_str_radix(tok, 16)
                    .map_err(|_| MetadataError::Invalid(format!("bad sig token '{tok}'")))?;
                bytes.push(b);
                mask.push(true);
            }
        }
        if bytes.is_empty() {
            return Err(MetadataError::Invalid("empty signature".into()));
        }
        Ok(Self { bytes, mask })
    }

    pub fn find_in(&self, haystack: &[u8]) -> Option<usize> {
        if self.bytes.len() > haystack.len() {
            return None;
        }
        let last = haystack.len() - self.bytes.len();
        'outer: for i in 0..=last {
            for (j, (&b, &m)) in self.bytes.iter().zip(self.mask.iter()).enumerate() {
                if m && haystack[i + j] != b {
                    continue 'outer;
                }
            }
            return Some(i);
        }
        None
    }
}

/// 在模块映像缓冲里解析命名符号 (优先 RVA, 否则特征码).
///
/// 查找键为 `name` 的 FNV-1a-32 (与上游 section 键一致).
pub fn resolve_in_image(
    map: &PatternMap,
    name: &str,
    image: &[u8],
    module_base: usize,
) -> Option<usize> {
    const PATCH_LEN: usize = 12; // detour 补丁写入宽度
    let entry = map.get_by_name(name)?;
    if let Some(rva) = entry.rva {
        let off = rva as usize;
        // 整个补丁写入窗口须落在映像内, 否则放弃 RVA (防越界写).
        if off.saturating_add(PATCH_LEN) <= image.len() {
            return Some(module_base.wrapping_add(off));
        }
        // RVA 越界: 若有特征码则继续扫.
    }
    if let Some(sig) = &entry.sig {
        let parsed = ByteSig::parse(sig).ok()?;
        let off = parsed.find_in(image)?;
        return Some(module_base.wrapping_add(off));
    }
    None
}

/// 仅按 RVA 解析 (映像映射在 `module_base`).
///
/// 仅测试/工具用途, 不做实存边界检查; 生产路径请用 find_in_image / find_by_rva_only.
pub fn resolve_rva(module_base: usize, rva: u64) -> usize {
    module_base.wrapping_add(rva as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fnv::fnv1a32_str;

    #[test]
    fn parse_and_resolve_rva() {
        let hash = fnv1a32_str("DemoFunc");
        let text = format!(
            r#"
[0x{hash:08X}]
name = "DemoFunc"
rva = "0x10"
sig = "90 90"
"#
        );
        let map = PatternMap::parse_str("steamui", &text).unwrap();
        let mut image = vec![0u8; 0x20];
        image[0x10] = 0xC3;
        let addr = resolve_in_image(&map, "DemoFunc", &image, 0x1000).unwrap();
        assert_eq!(addr, 0x1010);
    }

    #[test]
    fn sig_scan_with_wildcard() {
        let sig = ByteSig::parse("48 89 ?? 5C").unwrap();
        let hay = [0x00, 0x48, 0x89, 0xAB, 0x5C, 0x00];
        assert_eq!(sig.find_in(&hay), Some(1));
    }

    #[test]
    fn resolve_in_image_rva_window_fits_at_image_end() {
        // rva = 0x14 (20), 补丁窗口 20..32 恰好贴住映像末尾 (32) -> 接受.
        let hash = fnv1a32_str("DemoFunc");
        let text = format!(
            r#"
[0x{hash:08X}]
name = "DemoFunc"
rva = "0x14"
"#
        );
        let map = PatternMap::parse_str("steamui", &text).unwrap();
        let image = vec![0u8; 0x20];
        assert_eq!(
            resolve_in_image(&map, "DemoFunc", &image, 0x1000),
            Some(0x1014)
        );
    }

    #[test]
    fn resolve_in_image_rva_near_end_falls_back_to_sig() {
        // rva = 0x18 (24), 24 + 12 > 32 -> rva 拒绝; 特征码在 4 处命中 -> 回落成功.
        let hash = fnv1a32_str("DemoFunc");
        let text = format!(
            r#"
[0x{hash:08X}]
name = "DemoFunc"
rva = "0x18"
sig = "C3 C3"
"#
        );
        let map = PatternMap::parse_str("steamui", &text).unwrap();
        let mut image = vec![0u8; 0x20];
        image[4] = 0xC3;
        image[5] = 0xC3;
        assert_eq!(
            resolve_in_image(&map, "DemoFunc", &image, 0x1000),
            Some(0x1004)
        );
    }

    #[test]
    fn resolve_in_image_rva_near_end_without_sig_is_none() {
        // rva = 0x18 (24), 24 + 12 > 32 -> 拒绝; 无特征码 -> None (不 panic).
        let hash = fnv1a32_str("DemoFunc");
        let text = format!(
            r#"
[0x{hash:08X}]
name = "DemoFunc"
rva = "0x18"
"#
        );
        let map = PatternMap::parse_str("steamui", &text).unwrap();
        let image = vec![0u8; 0x20];
        assert_eq!(resolve_in_image(&map, "DemoFunc", &image, 0x1000), None);
    }
}
