//! `checksums.sha256` 解析 (sha256sum 格式: `<hex>  <name>`).

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksums {
    by_name: HashMap<String, String>,
}

impl Checksums {
    /// 解析清单文本; 空行和未知格式行跳过.
    pub fn parse(text: &str) -> Self {
        let mut by_name = HashMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // sha256sum 输出: `hex  name` 或 `hex *name` (二进制标记).
            let mut parts = line.split_whitespace();
            let (Some(hex), Some(name)) = (parts.next(), parts.next()) else {
                continue;
            };
            if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let name = name.strip_prefix('*').unwrap_or(name);
            by_name.insert(name.to_owned(), hex.to_ascii_lowercase());
        }
        Self { by_name }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.by_name.get(name).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sha256sum_lines() {
        let hex_a = "a".repeat(64);
        let hex_b = "b".repeat(64);
        let text = format!("ab\n{hex_a}  stbase.dll\n{hex_b}  dwmapi.dll");
        let checksums = Checksums::parse(&text);
        // 第一行 hex 不足 64 位被跳过.
        assert_eq!(checksums.len(), 2);
        assert_eq!(checksums.get("stbase.dll"), Some(hex_a.as_str()));
        assert_eq!(checksums.get("dwmapi.dll"), Some(hex_b.as_str()));
    }

    #[test]
    fn handles_binary_marker_and_blank_lines() {
        let text = format!(
            "{}  dwmapi.dll\n\n{} *stbase.dll\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        let checksums = Checksums::parse(&text);
        assert_eq!(checksums.len(), 2);
        assert_eq!(checksums.get("stbase.dll"), Some("b".repeat(64).as_str()));
        assert_eq!(checksums.get("dwmapi.dll"), Some("a".repeat(64).as_str()));
    }

    #[test]
    fn lowercases_hex() {
        let checksums = Checksums::parse(&format!("{}  stbase.dll", "AB".repeat(32)));
        assert_eq!(checksums.get("stbase.dll"), Some("ab".repeat(32).as_str()));
    }

    #[test]
    fn missing_entry_returns_none() {
        let checksums = Checksums::parse(&format!("{}  stbase.dll", "a".repeat(64)));
        assert_eq!(checksums.get("missing.dll"), None);
    }
}
