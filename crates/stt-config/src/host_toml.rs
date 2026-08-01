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
    #[serde(default)]
    pub store_accel: StoreAccelSection,
    #[serde(default)]
    pub update: UpdateSection,
    /// 工具 id -> 是否启用; 缺省键走工具默认值.
    #[serde(default)]
    pub tools: ToolsSection,
}

/// 商店网页访问 helper 的受限网络配置.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreAccelSection {
    #[serde(default = "default_store_accel_listen_port")]
    pub listen_port: u16,
    /// 只能是字面 IP, 不能是主机名, 避免 helper 自身落回系统 DNS.
    #[serde(default = "default_store_accel_resolver")]
    pub resolver: String,
    #[serde(default = "default_store_accel_dns_timeout_ms")]
    pub dns_timeout_ms: u32,
    #[serde(default = "default_store_accel_connect_timeout_ms")]
    pub connect_timeout_ms: u32,
    #[serde(default = "default_store_accel_max_connections")]
    pub max_connections: usize,
    #[serde(default)]
    pub egress: StoreAccelEgress,
    /// `http_connect` 出口的固定 IPv4 地址和端口, 由用户自有中继提供.
    #[serde(default)]
    pub upstream: String,
    /// 可选的用户本机 Clash HTTP 代理, 只允许 loopback 地址; 本地候选失败后最多回退一次.
    #[serde(default)]
    pub clash_fallback: String,
}

/// helper 如何从本机到达 Steam 网页域名.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreAccelEgress {
    /// 未配置出口时不启动 helper, 不能改系统 PAC.
    #[default]
    Disabled,
    /// 本机用受信 DNS 得到目标 IP 后直接连 Steam.
    DirectDns,
    /// 本地候选 IP 优选, 动态请求可回退到用户本机代理.
    LocalCdn,
    /// 连接用户提供的 HTTP CONNECT 中继, 由中继解析并出站.
    HttpConnect,
}

fn default_store_accel_listen_port() -> u16 {
    18_942
}

fn default_store_accel_resolver() -> String {
    "1.1.1.1:53".into()
}

fn default_store_accel_dns_timeout_ms() -> u32 {
    2_000
}

fn default_store_accel_connect_timeout_ms() -> u32 {
    5_000
}

fn default_store_accel_max_connections() -> usize {
    32
}

impl Default for StoreAccelSection {
    fn default() -> Self {
        Self {
            listen_port: default_store_accel_listen_port(),
            resolver: default_store_accel_resolver(),
            dns_timeout_ms: default_store_accel_dns_timeout_ms(),
            connect_timeout_ms: default_store_accel_connect_timeout_ms(),
            max_connections: default_store_accel_max_connections(),
            egress: StoreAccelEgress::Disabled,
            upstream: String::new(),
            clash_fallback: String::new(),
        }
    }
}

impl StoreAccelSection {
    /// 配置只接受固定 IPv4 DNS 地址, 防止 helper 再走系统解析.
    pub fn validate(&self) -> Result<()> {
        const MAX_TIMEOUT_MS: u32 = 60_000;
        const MAX_CONNECTIONS: usize = 256;
        if self.listen_port == 0 {
            return Err(ConfigError::Invalid(
                "store_accel.listen_port must not be 0".into(),
            ));
        }
        let resolver = self
            .resolver
            .parse::<std::net::SocketAddrV4>()
            .map_err(|_| {
                ConfigError::Invalid(
                    "store_accel.resolver must be an IPv4 address with port".into(),
                )
            })?;
        if resolver.port() == 0 {
            return Err(ConfigError::Invalid(
                "store_accel.resolver port must not be 0".into(),
            ));
        }
        if [self.dns_timeout_ms, self.connect_timeout_ms]
            .into_iter()
            .any(|timeout| timeout == 0 || timeout > MAX_TIMEOUT_MS)
        {
            return Err(ConfigError::Invalid(
                "store_accel timeout must be within 1..=60000 ms".into(),
            ));
        }
        if self.max_connections == 0 || self.max_connections > MAX_CONNECTIONS {
            return Err(ConfigError::Invalid(format!(
                "store_accel.max_connections must be within 1..={MAX_CONNECTIONS}"
            )));
        }
        match self.egress {
            StoreAccelEgress::Disabled
            | StoreAccelEgress::DirectDns
            | StoreAccelEgress::LocalCdn
                if !self.upstream.is_empty() =>
            {
                return Err(ConfigError::Invalid(
                    "store_accel.upstream requires egress = http_connect".into(),
                ));
            }
            StoreAccelEgress::Disabled | StoreAccelEgress::DirectDns => {}
            StoreAccelEgress::LocalCdn => {}
            StoreAccelEgress::HttpConnect => {
                let upstream = self
                    .upstream
                    .parse::<std::net::SocketAddrV4>()
                    .map_err(|_| {
                        ConfigError::Invalid(
                            "store_accel.upstream must be an IPv4 address with port".into(),
                        )
                    })?;
                if upstream.port() == 0 {
                    return Err(ConfigError::Invalid(
                        "store_accel.upstream port must not be 0".into(),
                    ));
                }
            }
        }
        if !self.clash_fallback.is_empty() {
            let fallback = self
                .clash_fallback
                .parse::<std::net::SocketAddrV4>()
                .map_err(|_| {
                    ConfigError::Invalid(
                        "store_accel.clash_fallback must be a loopback IPv4 address and port"
                            .into(),
                    )
                })?;
            if !fallback.ip().is_loopback() || fallback.port() == 0 {
                return Err(ConfigError::Invalid(
                    "store_accel.clash_fallback must be a loopback IPv4 address and port".into(),
                ));
            }
            if self.egress != StoreAccelEgress::LocalCdn {
                return Err(ConfigError::Invalid(
                    "store_accel.clash_fallback requires egress = local_cdn".into(),
                ));
            }
        }
        Ok(())
    }
}

/// 自更新配置.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateSection {
    /// 是否在启动时检查并应用新版本. 默认开; 关掉后完全不做网络请求.
    #[serde(default = "default_update_enabled")]
    pub enabled: bool,
    /// 更新通道, 目前只有 stable.
    #[serde(default = "default_update_channel")]
    pub channel: String,
}

fn default_update_enabled() -> bool {
    true
}

fn default_update_channel() -> String {
    "stable".into()
}

impl Default for UpdateSection {
    fn default() -> Self {
        Self {
            enabled: default_update_enabled(),
            channel: default_update_channel(),
        }
    }
}

impl UpdateSection {
    pub fn validate(&self) -> Result<()> {
        if self.channel != "stable" {
            return Err(ConfigError::Invalid("update.channel must be stable".into()));
        }
        Ok(())
    }
}

/// 完整 Catalog 的来源模式.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogMode {
    /// 内置社区多源聚合, 开箱即用的默认入库源.
    #[default]
    Community,
    /// 不配置完整目录源, 入库操作明确失败.
    Disabled,
    /// 从 URL 模板拉取 SteamTools wire v1.
    CustomHttp,
    /// 从固定的 `config/lua/catalog.lua` 调用 SteamTools wire v1 扩展.
    Lua,
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
            mode: CatalogMode::Community,
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

/// 额外 lua 目录的条数上限 (与 intent.rs 的 MAX_LUA_PATHS 一致).
const MAX_LUA_PATHS: usize = 8;

/// 单条路径的长度上限 (与 intent.rs 的 MAX_PATH_LEN 一致).
const MAX_PATH_LEN: usize = 260;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LuaSection {
    /// 额外 lua 目录; 加载器总会再挂上默认的 `<Steam>/config/lua`.
    #[serde(default)]
    pub paths: Vec<String>,
}

impl LuaSection {
    /// 解析期体检: 每条路径非空、≤260、无控制字符, 条数 ≤ 8; 目录存在性留到 intent 阶段查.
    pub fn validate(&self) -> Result<()> {
        if self.paths.len() > MAX_LUA_PATHS {
            return Err(ConfigError::Invalid(format!(
                "lua.paths: at most {MAX_LUA_PATHS} entries, got {}",
                self.paths.len()
            )));
        }
        for path in &self.paths {
            // 镜像 intent.rs 的 sane_path: 先 trim 再查长度与非法字符.
            let path = path.trim();
            if path.is_empty() || path.len() > MAX_PATH_LEN {
                return Err(ConfigError::Invalid(
                    "lua.paths entry must be non-empty and at most 260 bytes".into(),
                ));
            }
            if path.contains(['\0', '\n', '\r']) {
                return Err(ConfigError::Invalid(
                    "lua.paths entry must not contain NUL, CR, or LF".into(),
                ));
            }
        }
        Ok(())
    }
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
        config.lua.validate()?;
        config.store_accel.validate()?;
        config.update.validate()?;
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
        assert_eq!(c.catalog.mode, CatalogMode::Community);
        assert_eq!(c.manifest.url, "opensteamtool");
        assert_eq!(c.store_accel.egress, StoreAccelEgress::Disabled);
        assert!(c.update.enabled);
        assert_eq!(c.update.channel, "stable");
        assert!(c.is_tool_enabled(ToolId::CatalogAdd));
        assert!(c.is_tool_enabled(ToolId::LibraryUx));
        assert!(!c.is_tool_enabled(ToolId::StoreAccel));
    }

    #[test]
    fn parse_update_section() {
        let c = HostConfig::parse_str("[update]\nenabled = false\n").unwrap();
        assert!(!c.update.enabled);
    }

    #[test]
    fn update_rejects_unknown_channel() {
        let error = HostConfig::parse_str("[update]\nchannel = \"beta\"\n").unwrap_err();
        assert!(error.to_string().contains("update.channel"));
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

    #[test]
    fn store_accel_requires_bounded_ip_resolver() {
        assert!(HostConfig::parse_str("[store_accel]\nresolver = \"dns.example:53\"").is_err());
        assert!(HostConfig::parse_str("[store_accel]\nresolver = \"1.1.1.1:0\"").is_err());
        assert!(HostConfig::parse_str("[store_accel]\nmax_connections = 0").is_err());
        assert!(HostConfig::parse_str("[store_accel]\ndns_timeout_ms = 60001").is_err());

        let parsed = HostConfig::parse_str("[store_accel]\nresolver = \"9.9.9.9:53\"").unwrap();
        assert_eq!(parsed.store_accel.resolver, "9.9.9.9:53");
    }

    #[test]
    fn store_accel_http_connect_requires_fixed_upstream() {
        assert!(HostConfig::parse_str("[store_accel]\negress = \"http_connect\"").is_err());
        assert!(HostConfig::parse_str(
            "[store_accel]\negress = \"http_connect\"\nupstream = \"proxy.example:3128\""
        )
        .is_err());
        assert!(HostConfig::parse_str("[store_accel]\nupstream = \"1.1.1.1:3128\"").is_err());

        let parsed = HostConfig::parse_str(
            "[store_accel]\negress = \"http_connect\"\nupstream = \"203.0.113.9:3128\"",
        )
        .unwrap();
        assert_eq!(parsed.store_accel.egress, StoreAccelEgress::HttpConnect);
    }

    #[test]
    fn store_accel_direct_dns_requires_explicit_egress() {
        assert!(HostConfig::parse_str("[store_accel]\nupstream = \"203.0.113.9:3128\"").is_err());

        let parsed = HostConfig::parse_str("[store_accel]\negress = \"direct_dns\"").unwrap();
        assert_eq!(parsed.store_accel.egress, StoreAccelEgress::DirectDns);
    }

    #[test]
    fn store_accel_local_cdn_accepts_loopback_clash_fallback() {
        let parsed = HostConfig::parse_str(
            "[store_accel]\negress = \"local_cdn\"\nclash_fallback = \"127.0.0.1:7890\"",
        )
        .unwrap();
        assert_eq!(parsed.store_accel.egress, StoreAccelEgress::LocalCdn);
        assert_eq!(parsed.store_accel.clash_fallback, "127.0.0.1:7890");
    }

    #[test]
    fn store_accel_clash_fallback_is_loopback_only() {
        assert!(HostConfig::parse_str(
            "[store_accel]\negress = \"local_cdn\"\nclash_fallback = \"8.8.8.8:7890\"",
        )
        .is_err());
        assert!(HostConfig::parse_str(
            "[store_accel]\negress = \"direct_dns\"\nclash_fallback = \"127.0.0.1:7890\"",
        )
        .is_err());
    }

    #[test]
    fn lua_path_rejects_empty_entry() {
        assert!(HostConfig::parse_str("[lua]\npaths = [\"\"]").is_err());
        // 全空白也算空, 与 sane_path 的 trim 一致.
        assert!(HostConfig::parse_str("[lua]\npaths = [\"   \"]").is_err());
    }

    #[test]
    fn lua_path_rejects_overlong_entry() {
        let long = "D:/".to_string() + &"a".repeat(258);
        assert_eq!(long.len(), 261);
        let error = HostConfig::parse_str(&format!("[lua]\npaths = [\"{long}\"]")).unwrap_err();
        assert!(error.to_string().contains("lua.paths"));
    }

    #[test]
    fn lua_path_rejects_control_characters() {
        // TOML 的 \u0000 转义会解码成真正的 NUL 字节.
        let error = HostConfig::parse_str("[lua]\npaths = [\"D:/x\\u0000y\"]").unwrap_err();
        assert!(error.to_string().contains("NUL"));
    }

    #[test]
    fn lua_paths_reject_more_than_eight_entries() {
        let nine = (0..9)
            .map(|i| format!("\"D:/dir{i}\""))
            .collect::<Vec<_>>()
            .join(",");
        let error = HostConfig::parse_str(&format!("[lua]\npaths = [{nine}]")).unwrap_err();
        assert!(error.to_string().contains("at most 8"));
    }

    #[test]
    fn lua_paths_accept_valid_entries() {
        let parsed = HostConfig::parse_str("[lua]\npaths = [\"D:/extra/lua\"]").unwrap();
        assert_eq!(parsed.lua.paths, vec!["D:/extra/lua"]);

        // 上限 8 条也通过.
        let eight = (0..8)
            .map(|i| format!("\"D:/dir{i}\""))
            .collect::<Vec<_>>()
            .join(",");
        let parsed = HostConfig::parse_str(&format!("[lua]\npaths = [{eight}]")).unwrap();
        assert_eq!(parsed.lua.paths.len(), 8);
    }

    #[test]
    fn default_config_without_lua_section_still_parses() {
        let parsed = HostConfig::parse_str("").unwrap();
        assert!(parsed.lua.paths.is_empty());
    }
}
