//! 工具注册表骨架 (id, 默认开关).

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolId {
    CatalogAdd,
    LibraryUx,
    ConfigUi,
    DownloadKit,
    StoreAccel,
    /// 在配置面板拖入本地 lua 包.
    LuaDrop,
}

impl ToolId {
    pub const ALL: &'static [ToolId] = &[
        ToolId::CatalogAdd,
        ToolId::LibraryUx,
        ToolId::ConfigUi,
        ToolId::DownloadKit,
        ToolId::StoreAccel,
        ToolId::LuaDrop,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ToolId::CatalogAdd => "catalog_add",
            ToolId::LibraryUx => "library_ux",
            ToolId::ConfigUi => "config_ui",
            ToolId::DownloadKit => "download_kit",
            ToolId::StoreAccel => "store_accel",
            ToolId::LuaDrop => "lua_drop",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "catalog_add" => Some(Self::CatalogAdd),
            "library_ux" => Some(Self::LibraryUx),
            "config_ui" => Some(Self::ConfigUi),
            "download_kit" => Some(Self::DownloadKit),
            "store_accel" => Some(Self::StoreAccel),
            "lua_drop" => Some(Self::LuaDrop),
            _ => None,
        }
    }

    /// 默认开: 入库 + 库 UX + 配置页 + 下载清单 + 拖放导入.
    ///
    /// 产品表常写「默认仅 catalog_add + library_ux」; 狗粮期把 `config_ui` 也默认开,
    /// 否则通道 gating (`catalog_add || config_ui`) 下关掉入库会把自己关没,
    /// 也没法在面板里拨开关. 2026-07-31 起 `download_kit` 默认开.
    /// 用户仍可在 toml 里关掉.
    pub fn default_enabled(self) -> bool {
        matches!(
            self,
            ToolId::CatalogAdd
                | ToolId::LibraryUx
                | ToolId::ConfigUi
                | ToolId::DownloadKit
                | ToolId::LuaDrop
        )
    }

    pub fn display_name(self) -> &'static str {
        match self {
            ToolId::CatalogAdd => "入库 / 清单",
            ToolId::LibraryUx => "库 UX",
            ToolId::ConfigUi => "配置页",
            ToolId::DownloadKit => "下载清单",
            ToolId::StoreAccel => "商店加速",
            ToolId::LuaDrop => "拖放导入",
        }
    }

    /// 仍未实现的工具在 UI 上标出来, 别让人以为开了就有效果.
    pub fn is_placeholder(self) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolManifest {
    pub id: ToolId,
    pub name: &'static str,
    pub default_enabled: bool,
    /// UI 贡献点落地前的占位.
    pub needs_client: bool,
}

pub fn builtin_manifests() -> Vec<ToolManifest> {
    ToolId::ALL
        .iter()
        .copied()
        .map(|id| ToolManifest {
            id,
            name: id.display_name(),
            default_enabled: id.default_enabled(),
            needs_client: matches!(id, ToolId::DownloadKit),
        })
        .collect()
}

pub fn default_tool_enabled_map() -> HashMap<String, bool> {
    ToolId::ALL
        .iter()
        .map(|id| (id.as_str().to_string(), id.default_enabled()))
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct ToolRegistry {
    enabled: HashMap<ToolId, bool>,
}

impl ToolRegistry {
    pub fn with_defaults() -> Self {
        let mut enabled = HashMap::new();
        for id in ToolId::ALL {
            enabled.insert(*id, id.default_enabled());
        }
        Self { enabled }
    }

    pub fn apply_overrides(&mut self, overrides: &HashMap<String, bool>) {
        for (k, v) in overrides {
            if let Some(id) = ToolId::parse(k) {
                self.enabled.insert(id, *v);
            }
        }
    }

    pub fn from_host_tools(enabled: &HashMap<String, bool>) -> Self {
        let mut reg = Self::with_defaults();
        reg.apply_overrides(enabled);
        reg
    }

    pub fn is_enabled(&self, id: ToolId) -> bool {
        self.enabled
            .get(&id)
            .copied()
            .unwrap_or_else(|| id.default_enabled())
    }

    pub fn set_enabled(&mut self, id: ToolId, on: bool) {
        self.enabled.insert(id, on);
    }

    pub fn enabled_ids(&self) -> impl Iterator<Item = ToolId> + '_ {
        ToolId::ALL
            .iter()
            .copied()
            .filter(|id| self.is_enabled(*id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_product() {
        let reg = ToolRegistry::with_defaults();
        assert!(reg.is_enabled(ToolId::CatalogAdd));
        assert!(reg.is_enabled(ToolId::LibraryUx));
        assert!(reg.is_enabled(ToolId::ConfigUi));
        assert!(reg.is_enabled(ToolId::DownloadKit));
        assert!(reg.is_enabled(ToolId::LuaDrop));
        assert!(!reg.is_enabled(ToolId::StoreAccel));
    }

    /// id 字符串是配置文件里的键, 改了会静默丢用户的开关.
    #[test]
    fn ids_round_trip() {
        for id in ToolId::ALL {
            assert_eq!(ToolId::parse(id.as_str()), Some(*id));
        }
    }

    #[test]
    fn override_disables_library_ux() {
        let mut map = HashMap::new();
        map.insert("library_ux".into(), false);
        let reg = ToolRegistry::from_host_tools(&map);
        assert!(!reg.is_enabled(ToolId::LibraryUx));
        assert!(reg.is_enabled(ToolId::CatalogAdd));
    }
}
