//! 配置页要显示的一份只读快照 (宿主推给页面, 页面不自己算).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use crate::intent::{CATALOG_MODES, LOG_LEVELS, MANIFEST_SOURCES};
use crate::tools::ToolId;
use crate::ConfigState;

/// 一个工具在配置页上的样子.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolView {
    pub id: &'static str,
    pub name: &'static str,
    pub enabled: bool,
    /// 占位工具: 功能尚未实现, 页面应显示为禁用开关.
    pub placeholder: bool,
    /// 这个工具此刻在干什么 / 为什么没干成.
    ///
    /// 开着不等于跑起来了: 缺 pattern 会降级, 通道没起来会哑火. 这些原来只进
    /// host.log, 用户在界面上看不出区别 —— 工具中心就是为了把它摆出来.
    pub detail: String,
}

/// 宿主每轮给配置页的工具运行状态 (id → 一句话).
pub type ToolDetails = std::collections::HashMap<&'static str, String>;

/// 只有宿主知道 (或算起来贵, 不该每轮重算) 的那部分.
#[derive(Debug, Default, Clone)]
pub struct HostFacts {
    /// 各工具此刻在干什么 / 为什么没干成.
    pub tool_details: ToolDetails,
    /// 自更新的一行状态 (已是最新 / 新版本已就绪等).
    pub update_status: String,
    /// 我们自己入库的 app; 由宿主按 rules epoch 缓存, 见 [`managed_apps`].
    pub managed: Vec<u32>,
    /// 受管 app 的本地显示名称, 找不到时由页面回退到 AppId.
    pub managed_names: BTreeMap<u32, String>,
}

/// 配置页一次渲染需要的全部数据.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigSnapshot {
    pub version: &'static str,
    pub tools: Vec<ToolView>,
    pub log_level: String,
    pub log_levels: &'static [&'static str],
    pub catalog_mode: String,
    pub catalog_modes: &'static [&'static str],
    pub catalog_url_template: String,
    pub catalog_status: String,
    /// 主游戏入库后是否自动添加 DLC.
    pub catalog_auto_dlc: bool,
    pub manifest_url: String,
    pub manifest_sources: &'static [&'static str],
    /// 额外 lua 目录 (默认目录不在其中, 单独给).
    pub lua_paths: Vec<String>,
    pub lua_dir: String,
    pub owned_count: usize,
    pub epoch: u64,
    /// 调试通道: pipe 或 127.0.0.1:端口.
    pub channel: String,
    /// 最近一次保存的结果, 给页面显示.
    pub note: String,
    /// 自更新状态一行 (页面直接展示).
    pub update_status: String,
    /// 我们自己入库的 app (有 `stt_{id}.lua` 那些), 升序.
    ///
    /// 库里右键要用它判断"这一项是不是我们加的" —— 不是我们加的就别抢 Steam 的菜单.
    pub managed: Vec<u32>,
    /// `AppId -> Steam 显示名称`, 只来自本机 appinfo 缓存.
    pub managed_names: BTreeMap<u32, String>,
}

impl ConfigSnapshot {
    /// 不带宿主补给的快照 (测试用; 受管列表就地扫一次目录).
    pub fn from_state(state: &ConfigState, steam_root: &Path, channel: &str, note: &str) -> Self {
        let facts = HostFacts {
            managed: managed_apps(state, steam_root),
            ..HostFacts::default()
        };
        let mut facts = facts;
        facts.managed_names = crate::app_names(steam_root, &facts.managed);
        Self::new(state, steam_root, channel, note, &facts)
    }

    /// 带上宿主补给的那部分.
    pub fn new(
        state: &ConfigState,
        steam_root: &Path,
        channel: &str,
        note: &str,
        facts: &HostFacts,
    ) -> Self {
        let host = state.host();
        let tools = state.tools();
        Self {
            version: env!("CARGO_PKG_VERSION"),
            tools: ToolId::ALL
                .iter()
                .map(|id| ToolView {
                    id: id.as_str(),
                    name: id.display_name(),
                    enabled: tools.is_enabled(*id),
                    placeholder: id.is_placeholder(),
                    detail: facts
                        .tool_details
                        .get(id.as_str())
                        .cloned()
                        .unwrap_or_default(),
                })
                .collect(),
            // `host` 已经是 `state.host()` 给的副本, 直接搬走字段, 别再克隆一遍.
            log_level: host.log.level,
            log_levels: LOG_LEVELS,
            catalog_mode: host.catalog.mode.as_str().to_owned(),
            catalog_modes: CATALOG_MODES,
            catalog_url_template: host.catalog.url_template,
            catalog_status: match host.catalog.mode {
                crate::CatalogMode::Disabled => "已禁用".to_owned(),
                crate::CatalogMode::CustomHttp => "CustomHttp".to_owned(),
                crate::CatalogMode::Lua => "Lua (config/lua/catalog.lua)".to_owned(),
                crate::CatalogMode::Community => "Community (多源聚合)".to_owned(),
                crate::CatalogMode::Mock => "Mock (开发模式)".to_owned(),
            },
            catalog_auto_dlc: host.catalog.auto_dlc,
            manifest_url: host.manifest.url,
            manifest_sources: MANIFEST_SOURCES,
            lua_paths: host.lua.paths,
            lua_dir: ConfigState::default_lua_dir(steam_root)
                .display()
                .to_string(),
            owned_count: state.owned_count(),
            epoch: state.rules_epoch(),
            channel: channel.to_owned(),
            note: note.to_owned(),
            update_status: facts.update_status.clone(),
            managed: facts.managed.clone(),
            managed_names: facts.managed_names.clone(),
        }
    }
}

/// 我们自己入库的 app: 目录里有 `stt_{id}.lua` 的那些.
///
/// 按文件名认而不是按 `AppRules` 里的 owned —— owned 是所有 lua 合并出来的,
/// 里面混着用户手写的, 那些不该由我们的菜单去"移除".
///
/// 要扫一次目录, 所以调用方应按 `rules_epoch` 缓存, 别每轮都来.
pub fn managed_apps(state: &ConfigState, steam_root: &Path) -> Vec<u32> {
    let dir = ConfigState::default_lua_dir(steam_root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut ids: Vec<u32> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_str()?;
            let id: u32 = name
                .strip_prefix("stt_")?
                .strip_suffix(".lua")?
                .parse()
                .ok()?;
            // 文件在但规则里没有 = 那份 lua 有问题, 别当成管着.
            state.with_rules(|r| r.is_owned(id)).then_some(id)
        })
        .collect();
    ids.sort_unstable();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_toml::HostConfig;

    /// 只认我们自己写的 `stt_*.lua`; 用户手写的不归菜单管.
    #[cfg(feature = "lua")]
    #[test]
    fn managed_lists_only_our_own_files() {
        let root = tempfile::tempdir().unwrap();
        let dir = ConfigState::default_lua_dir(root.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stt_730.lua"), "addappid(730)\n").unwrap();
        std::fs::write(dir.join("mine.lua"), "addappid(777)\n").unwrap();

        let state = ConfigState::new();
        state.reload_lua_dirs(root.path());
        let snap = ConfigSnapshot::from_state(&state, root.path(), "pipe", "");
        assert_eq!(snap.managed, vec![730]);
    }

    /// 文件在但规则里没有 = 那份 lua 坏了, 不该报成"管着".
    #[cfg(feature = "lua")]
    #[test]
    fn managed_skips_files_that_did_not_load() {
        let root = tempfile::tempdir().unwrap();
        let dir = ConfigState::default_lua_dir(root.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stt_730.lua"), "this is not lua ((\n").unwrap();

        let state = ConfigState::new();
        state.reload_lua_dirs(root.path());
        let snap = ConfigSnapshot::from_state(&state, root.path(), "pipe", "");
        assert!(snap.managed.is_empty());
    }

    #[test]
    fn tool_detail_comes_from_the_host() {
        let state = ConfigState::new();
        let mut facts = HostFacts::default();
        facts
            .tool_details
            .insert("library_ux", "缺 pattern, 已降级".into());
        let snap = ConfigSnapshot::new(&state, Path::new("C:/steam"), "pipe", "", &facts);
        let ux = snap.tools.iter().find(|t| t.id == "library_ux").unwrap();
        assert_eq!(ux.detail, "缺 pattern, 已降级");
    }

    #[test]
    fn snapshot_lists_every_tool() {
        let state = ConfigState::new();
        let snap = ConfigSnapshot::from_state(&state, Path::new("C:/steam"), "pipe", "");
        assert_eq!(snap.tools.len(), ToolId::ALL.len());
        let ui = snap.tools.iter().find(|t| t.id == "config_ui").unwrap();
        assert!(ui.enabled);
        let accel = snap.tools.iter().find(|t| t.id == "store_accel").unwrap();
        assert!(!accel.enabled);
        assert!(!accel.placeholder);
        let download = snap.tools.iter().find(|t| t.id == "download_kit").unwrap();
        assert!(download.enabled);
        assert!(!download.placeholder);
    }

    #[test]
    fn snapshot_follows_host_config() {
        let state = ConfigState::new();
        state.apply_host(
            HostConfig::parse_str(
                "[tools.enabled]\nlibrary_ux = false\n\n[catalog]\nmode = \"mock\"\n\n[manifest]\nurl = \"wudrm\"\n",
            )
            .unwrap(),
        );
        let snap = ConfigSnapshot::from_state(&state, Path::new("C:/steam"), "pipe", "saved");
        assert!(
            !snap
                .tools
                .iter()
                .find(|t| t.id == "library_ux")
                .unwrap()
                .enabled
        );
        assert_eq!(snap.manifest_url, "wudrm");
        assert_eq!(snap.catalog_mode, "mock");
        assert!(snap.catalog_status.contains("开发模式"));
        assert!(snap.catalog_auto_dlc, "auto_dlc 默认开");
        assert_eq!(snap.note, "saved");
    }

    #[test]
    fn snapshot_reflects_auto_dlc_off() {
        let state = ConfigState::new();
        state.apply_host(
            HostConfig::parse_str("[catalog]\nmode = \"mock\"\nauto_dlc = false\n").unwrap(),
        );
        let snap = ConfigSnapshot::from_state(&state, Path::new("C:/steam"), "pipe", "");
        assert!(!snap.catalog_auto_dlc);
    }
}
