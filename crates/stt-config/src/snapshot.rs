//! 配置页要显示的一份只读快照 (宿主推给页面, 页面不自己算).

use std::path::Path;

use serde::Serialize;

use crate::intent::{LOG_LEVELS, MANIFEST_SOURCES};
use crate::tools::ToolId;
use crate::ConfigState;

/// 一个工具在配置页上的样子.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolView {
    pub id: &'static str,
    pub name: &'static str,
    pub enabled: bool,
    /// 占位工具: 开关能拨, 但还没有实现.
    pub placeholder: bool,
}

/// 配置页一次渲染需要的全部数据.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigSnapshot {
    pub version: &'static str,
    pub tools: Vec<ToolView>,
    pub log_level: String,
    pub log_levels: &'static [&'static str],
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
}

impl ConfigSnapshot {
    pub fn from_state(state: &ConfigState, steam_root: &Path, channel: &str, note: &str) -> Self {
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
                })
                .collect(),
            // `host` 已经是 `state.host()` 给的副本, 直接搬走字段, 别再克隆一遍.
            log_level: host.log.level,
            log_levels: LOG_LEVELS,
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_toml::HostConfig;

    #[test]
    fn snapshot_lists_every_tool() {
        let state = ConfigState::new();
        let snap = ConfigSnapshot::from_state(&state, Path::new("C:/steam"), "pipe", "");
        assert_eq!(snap.tools.len(), ToolId::ALL.len());
        let ui = snap.tools.iter().find(|t| t.id == "config_ui").unwrap();
        assert!(ui.enabled);
        let accel = snap.tools.iter().find(|t| t.id == "store_accel").unwrap();
        assert!(!accel.enabled);
        assert!(accel.placeholder);
    }

    #[test]
    fn snapshot_follows_host_config() {
        let state = ConfigState::new();
        state.apply_host(
            HostConfig::parse_str(
                "[tools.enabled]\nlibrary_ux = false\n\n[manifest]\nurl = \"wudrm\"\n",
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
        assert_eq!(snap.note, "saved");
    }
}
