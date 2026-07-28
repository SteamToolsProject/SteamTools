//! 宿主级 `steamtools.toml` 结构.

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
    pub catalog: CatalogSection,
    #[serde(default)]
    pub manifest: ManifestSection,
    #[serde(default)]
    pub lua: LuaSection,
    /// 工具 id -> 是否启用; 缺省键走工具默认值.
    #[serde(default)]
    pub tools: ToolsSection,
}

/// 完整 Catalog 的来源模式.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogMode {
    /// 不配置完整目录源, 入库操作明确失败.
    #[default]
    Disabled,
    /// 从 URL 模板拉取 SteamTools wire v1.
    CustomHttp,
    /// 从固定的 `config/lua/catalog.lua` 调用 SteamTools wire v1 扩展.
    Lua,
    /// 内置社区多源聚合.
    Community,
    /// 仅用于显式开发模式的确定性假数据.
    Mock,
}

impl CatalogMode {
    pub const ALL: [Self; 5] = [
        Self::Disabled,
        Self::CustomHttp,
        Self::Lua,
        Self::Community,
        Self::Mock,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::CustomHttp => "custom_http",
            Self::Lua => "lua",
            Self::Community => "community",
            Self::Mock => "mock",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.as_str() == value)
    }
}

/// 完整 Catalog 配置, 与 manifest request-code 源完全独立.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSection {
    #[serde(default)]
    pub mode: CatalogMode,
    #[serde(default)]
    pub url_template: String,
    #[serde(default = "default_timeout_5s")]
    pub timeout_resolve_ms: u32,
    #[serde(default = "default_timeout_5s")]
    pub timeout_connect_ms: u32,
    #[serde(default = "default_timeout_10s")]
    pub timeout_send_ms: u32,
    #[serde(default = "default_timeout_10s")]
    pub timeout_recv_ms: u32,
    #[serde(default = "default_catalog_response_limit")]
    pub max_response_bytes: usize,
}

impl Default for CatalogSection {
    fn default() -> Self {
        Self {
            mode: CatalogMode::Disabled,
            url_template: String::new(),
            timeout_resolve_ms: default_timeout_5s(),
            timeout_connect_ms: default_timeout_5s(),
            timeout_send_ms: default_timeout_10s(),
            timeout_recv_ms: default_timeout_10s(),
            max_response_bytes: default_catalog_response_limit(),
        }
    }
}

fn default_catalog_response_limit() -> usize {
    stt_catalog::CatalogLimits::default().max_wire_bytes
}

impl CatalogSection {
    /// 校验资源上限和当前模式需要的字段.
    pub fn validate(&self) -> Result<()> {
        const MAX_TIMEOUT_MS: u32 = 60_000;
        let timeouts = [
            self.timeout_resolve_ms,
            self.timeout_connect_ms,
            self.timeout_send_ms,
            self.timeout_recv_ms,
        ];
        if timeouts
            .into_iter()
            .any(|timeout| timeout == 0 || timeout > MAX_TIMEOUT_MS)
        {
            return Err(ConfigError::Invalid(
                "catalog timeout must be within 1..=60000 ms".into(),
            ));
        }
        let max_wire_bytes = stt_catalog::CatalogLimits::default().max_wire_bytes;
        if self.max_response_bytes == 0 || self.max_response_bytes > max_wire_bytes {
            return Err(ConfigError::Invalid(format!(
                "catalog.max_response_bytes must be within 1..={max_wire_bytes}"
            )));
        }
        if self.mode == CatalogMode::CustomHttp {
            stt_catalog::validate_url_template(&self.url_template)?;
        }
        Ok(())
    }
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

impl ManifestSection {
    pub fn validate(&self) -> Result<()> {
        const MAX_TIMEOUT_MS: u32 = 60_000;
        if !matches!(self.url.as_str(), "opensteamtool" | "steamrun" | "wudrm") {
            return Err(ConfigError::Invalid(
                "manifest.url must be opensteamtool, steamrun, or wudrm".into(),
            ));
        }
        let timeouts = [
            self.timeout_resolve_ms,
            self.timeout_connect_ms,
            self.timeout_send_ms,
            self.timeout_recv_ms,
        ];
        if timeouts
            .into_iter()
            .any(|timeout| timeout == 0 || timeout > MAX_TIMEOUT_MS)
        {
            return Err(ConfigError::Invalid(
                "manifest timeout must be within 1..=60000 ms".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LuaSection {
    /// 额外 lua 目录; 加载器总会再挂上默认的 `<Steam>/config/lua`.
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
        let config: Self = toml::from_str(s)?;
        config.catalog.validate()?;
        config.manifest.validate()?;
        Ok(config)
    }

    pub fn load_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse_str(&text)
    }

    /// 优先 `steamtools.toml`, 否则读旧的 `opensteamtool.toml`.
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

    /// 合并已知工具默认开关 (给 UI / 导出用).
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
        assert_eq!(c.catalog.mode, CatalogMode::Disabled);
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

    #[test]
    fn manifest_source_and_timeouts_are_bounded() {
        assert!(HostConfig::parse_str("[manifest]\nurl = \"custom\"").is_err());
        assert!(HostConfig::parse_str("[manifest]\nurl = \"wudrm\"\ntimeout_recv_ms = 0").is_err());
        assert!(HostConfig::parse_str(
            "[manifest]\nurl = \"steamrun\"\ntimeout_connect_ms = 60001"
        )
        .is_err());
    }

    #[test]
    fn parse_custom_http_catalog() {
        let config = HostConfig::parse_str(
            r#"
[catalog]
mode = "custom_http"
url_template = "http://127.0.0.1:8081/catalog/{app_id}"
timeout_recv_ms = 2000
"#,
        )
        .unwrap();

        assert_eq!(config.catalog.mode, CatalogMode::CustomHttp);
        assert_eq!(config.catalog.timeout_recv_ms, 2000);
    }

    #[test]
    fn custom_http_requires_valid_template() {
        let error = HostConfig::parse_str("[catalog]\nmode = \"custom_http\"\n").unwrap_err();

        assert!(error.to_string().contains("URL template"));
    }

    #[test]
    fn catalog_timeout_cannot_be_infinite() {
        let error = HostConfig::parse_str("[catalog]\ntimeout_recv_ms = 0\n").unwrap_err();

        assert!(error.to_string().contains("timeout"));
    }
}
