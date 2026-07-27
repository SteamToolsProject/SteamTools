//! steamui 符号清单与可降级安装报告 (不猜偏移, 默认不 attach 业务 detour).

use stt_config::{ToolId, ToolRegistry};
use stt_metadata::PatternStore;

/// 库 UX 需要的 steamui 符号 (对标 Hooks_SteamUI::Install).
pub const LIBRARY_UX_SYMBOLS: &[&str] = &[
    "FillInAppOverview",
    "BuildCompleteAppOverviewChange",
    "CSteamUIAppControllerRunFrame",
    "GetAppByID",
    "MarkAppChange",
    "RepeatedFieldUint32_Add",
];

/// detour 安装所需的最小集合 (capture/resolve 可后置).
pub const LIBRARY_UX_HOOK_SYMBOLS: &[&str] = &[
    "FillInAppOverview",
    "BuildCompleteAppOverviewChange",
    "CSteamUIAppControllerRunFrame",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryUxInstallStatus {
    /// 工具关闭.
    Disabled,
    /// pattern 未加载.
    PatternMissing,
    /// 缺关键符号.
    SymbolsMissing,
    /// 逻辑就绪, 但尚未 attach 业务 detour (缺布局/策略).
    LogicOnly,
    /// 将来: 业务 hook 已挂上.
    HooksAttached,
}

#[derive(Debug, Clone)]
pub struct LibraryUxInstallReport {
    pub status: LibraryUxInstallStatus,
    pub resolved: Vec<String>,
    pub missing: Vec<String>,
}

impl LibraryUxInstallReport {
    pub fn summary_line(&self) -> String {
        format!(
            "library_ux=status={:?} resolved={} missing={}",
            self.status,
            self.resolved.len(),
            if self.missing.is_empty() {
                "none".into()
            } else {
                self.missing.join(",")
            }
        )
    }
}

/// 根据工具开关 + pattern 解析结果决定库 UX 安装状态.
///
/// `symbol_present`: 名称是否在 pattern map 中有条目 (不要求已扫到地址).
pub fn plan_library_ux_install(
    tools: &ToolRegistry,
    patterns: &PatternStore,
    component: &str,
) -> LibraryUxInstallReport {
    if !tools.is_enabled(ToolId::LibraryUx) {
        return LibraryUxInstallReport {
            status: LibraryUxInstallStatus::Disabled,
            resolved: Vec::new(),
            missing: Vec::new(),
        };
    }

    let map = match patterns.map(component) {
        Some(map) if !patterns.is_failed(component) => map,
        _ => {
            return LibraryUxInstallReport {
                status: LibraryUxInstallStatus::PatternMissing,
                resolved: Vec::new(),
                missing: LIBRARY_UX_SYMBOLS
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
            }
        }
    };
    let mut resolved = Vec::new();
    let mut missing = Vec::new();
    for &name in LIBRARY_UX_SYMBOLS {
        if map.get_by_name(name).is_some() {
            resolved.push(name.to_string());
        } else {
            missing.push(name.to_string());
        }
    }

    let hooks_ok = LIBRARY_UX_HOOK_SYMBOLS
        .iter()
        .all(|n| map.get_by_name(n).is_some());

    let status = if !hooks_ok {
        LibraryUxInstallStatus::SymbolsMissing
    } else {
        // 有 pattern 条目仍不够: CSteamApp 字段偏移 / RunFrame 入口未在本机
        // SteamUI 上钉死前不 attach 业务 detour (写 PurchasedTime 会踩内存).
        LibraryUxInstallStatus::LogicOnly
    };

    LibraryUxInstallReport {
        status,
        resolved,
        missing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(text: &str) -> PatternStore {
        let mut s = PatternStore::new();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("p.toml");
        std::fs::write(&p, text).unwrap();
        s.load_file("steamui", &p).unwrap();
        s
    }

    #[test]
    fn disabled_when_tool_off() {
        let mut tools = ToolRegistry::with_defaults();
        tools.set_enabled(ToolId::LibraryUx, false);
        let store = PatternStore::new();
        let r = plan_library_ux_install(&tools, &store, "steamui");
        assert_eq!(r.status, LibraryUxInstallStatus::Disabled);
    }

    #[test]
    fn pattern_missing_status() {
        let tools = ToolRegistry::with_defaults();
        let store = PatternStore::new();
        let r = plan_library_ux_install(&tools, &store, "steamui");
        assert_eq!(r.status, LibraryUxInstallStatus::PatternMissing);
    }

    #[test]
    fn logic_only_when_hook_symbols_present() {
        use stt_metadata::fnv1a32_str;
        let mut body = String::new();
        for name in LIBRARY_UX_HOOK_SYMBOLS {
            let h = fnv1a32_str(name);
            body.push_str(&format!(
                "[0x{h:08X}]\nname = \"{name}\"\nrva = \"0x10\"\n\n"
            ));
        }
        let tools = ToolRegistry::with_defaults();
        let store = store_with(&body);
        let r = plan_library_ux_install(&tools, &store, "steamui");
        assert_eq!(r.status, LibraryUxInstallStatus::LogicOnly);
        assert!(!r.missing.is_empty()); // capture symbols still missing
    }
}
