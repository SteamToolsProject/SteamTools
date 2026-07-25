//! Host-level `steamtools.toml` schema.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};
use crate::tools::{default_tool_enabled_map, ToolId};

pub const HOST_TOML_NAME: &str = "steamtools.toml";
pub const LEGACY_TOML_NAME: &str = "opensteamtool.toml";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostConfig {
    #[serde(default)]
    pub log: LogSection,
    #[serde(default)]
    pub manifest: ManifestSection,
    #[serde(default)]
    pub lua: LuaSection,
    /// Tool id → enabled. Missing keys fall back to tool defaults.
    #[serde(default)]
    pub tools: ToolsSection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogSection {
    #[serde(default = "default_log_level")]
    pub level: String,
}

fn default_log_level() -> String {
    "debug".into()
}

impl Default for LogSection {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestSection {
    #[serde(default = "default_manifest_url")]
    pub url: String,
    #[serde(default = "default_timeout_5s")]
    pub timeout_resolve_ms: u32,
    #[serde(default = "default_timeout_5s")]
    pub timeout_connect_ms: u32,
    #[serde(default = "default_timeout_10s")]
    pub timeout_send_ms: u32,
    #[serde(default = "default_timeout_10s")]
    pub timeout_recv_ms: u32,
}

fn default_manifest_url() -> String {
    "opensteamtool".into()
}
fn default_timeout_5s() -> u32 {
    5000
}
fn default_timeout_10s() -> u32 {
    10000
}

impl Default for ManifestSection {
    fn default() -> Self {
        Self {
            url: default_manifest_url(),
            timeout_resolve_ms: default_timeout_5s(),
            timeout_connect_ms: default_timeout_5s(),
            timeout_send_ms: default_timeout_10s(),
            timeout_recv_ms: default_timeout_10s(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LuaSection {
    /// Extra lua dirs; default `<Steam>/config/lua` is always appended by the loader.
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ToolsSection {
    #[serde(default)]
    pub enabled: HashMap<String, bool>,
}

impl HostConfig {
    pub fn parse_str(s: &str) -> Result<Self> {
        Ok(toml::from_str(s)?)
    }

    pub fn load_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse_str(&text)
    }

    /// Prefer `steamtools.toml`, else legacy `opensteamtool.toml`.
    pub fn resolve_path(steam_root: &Path) -> Option<PathBuf> {
        let primary = steam_root.join(HOST_TOML_NAME);
        if primary.is_file() {
            return Some(primary);
        }
        let legacy = steam_root.join(LEGACY_TOML_NAME);
        if legacy.is_file() {
            return Some(legacy);
        }
        None
    }

    pub fn load_from_steam_root(steam_root: &Path) -> Result<Self> {
        match Self::resolve_path(steam_root) {
            Some(p) => Self::load_file(&p),
            None => Ok(Self::default()),
        }
    }

    pub fn is_tool_enabled(&self, id: ToolId) -> bool {
        self.tools
            .enabled
            .get(id.as_str())
            .copied()
            .unwrap_or_else(|| id.default_enabled())
    }

    /// Merge defaults for known tools (for UI / dump).
    pub fn effective_tool_map(&self) -> HashMap<String, bool> {
        let mut map = default_tool_enabled_map();
        for (k, v) in &self.tools.enabled {
            map.insert(k.clone(), *v);
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_defaults() {
        let c = HostConfig::parse_str("").unwrap();
        assert_eq!(c.log.level, "debug");
        assert_eq!(c.manifest.url, "opensteamtool");
        assert!(c.is_tool_enabled(ToolId::CatalogAdd));
        assert!(c.is_tool_enabled(ToolId::LibraryUx));
        assert!(!c.is_tool_enabled(ToolId::StoreAccel));
    }

    #[test]
    fn parse_tools_enabled() {
        let toml = r#"
[tools.enabled]
catalog_add = true
library_ux = false
store_accel = true
"#;
        let c = HostConfig::parse_str(toml).unwrap();
        assert!(c.is_tool_enabled(ToolId::CatalogAdd));
        assert!(!c.is_tool_enabled(ToolId::LibraryUx));
        assert!(c.is_tool_enabled(ToolId::StoreAccel));
    }

    #[test]
    fn parse_manifest_and_lua_paths() {
        let toml = r#"
[manifest]
url = "wudrm"
timeout_recv_ms = 2000

[lua]
paths = ["D:/extra/lua"]
"#;
        let c = HostConfig::parse_str(toml).unwrap();
        assert_eq!(c.manifest.url, "wudrm");
        assert_eq!(c.manifest.timeout_recv_ms, 2000);
        assert_eq!(c.lua.paths, vec!["D:/extra/lua"]);
    }
}
