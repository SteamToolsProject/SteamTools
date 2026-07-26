//! Pattern TOML subset + byte signature scan.

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
    /// FNV-1a-32(name) → entry
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

/// Parsed IDA-style signature: `48 89 ?? 5C`.
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

/// Resolve a named symbol inside a module image buffer (RVA preferred, else sig).
///
/// Lookup key is FNV-1a-32 of `name` (same as upstream section keys).
pub fn resolve_in_image(
    map: &PatternMap,
    name: &str,
    image: &[u8],
    module_base: usize,
) -> Option<usize> {
    let entry = map.get_by_name(name)?;
    if let Some(rva) = entry.rva {
        let off = rva as usize;
        if off < image.len() {
            return Some(module_base.wrapping_add(off));
        }
        // RVA out of image: fall through to signature if present.
    }
    if let Some(sig) = &entry.sig {
        let parsed = ByteSig::parse(sig).ok()?;
        let off = parsed.find_in(image)?;
        return Some(module_base.wrapping_add(off));
    }
    None
}

/// RVA-only resolve when the image is mapped at `module_base` (no bounds check on live memory).
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
}
