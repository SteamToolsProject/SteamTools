//! package 层可降级安装报告 (默认不 attach).

use stt_config::{ToolId, ToolRegistry};
use stt_metadata::PatternStore;

/// pattern 中需要存在的 P0 符号 (resolve + 唯一 detour).
pub const PACKAGE_P0_SYMBOLS: &[&str] = &[
    "CheckAppOwnership",
    "GetPackageInfo",
    "CUtlMemoryGrow",
    "MarkLicenseAsChanged",
    "ProcessPendingLicenseUpdates",
];

/// 唯一计划 detour 的符号 (其余为 resolve/capture).
pub const PACKAGE_HOOK_SYMBOLS: &[&str] = &["CheckAppOwnership"];

/// 可选, 不挡 LogicOnly.
pub const PACKAGE_OPTIONAL_SYMBOLS: &[&str] = &["LoadPackage"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageInstallStatus {
    /// catalog_add 关闭 → 不装 package 层.
    Disabled,
    /// steamclient pattern 未加载.
    PatternMissing,
    /// 缺 P0 符号.
    SymbolsMissing,
    /// 符号齐, 布局常量已有, 但未 attach (package0 Status 等运行时确认).
    LogicOnly,
    /// 将来: detour 已挂.
    HooksAttached,
}

#[derive(Debug, Clone)]
pub struct PackageInstallReport {
    pub status: PackageInstallStatus,
    pub resolved: Vec<String>,
    pub missing: Vec<String>,
    pub optional_resolved: Vec<String>,
    /// attach / 降级原因 (给人看的短句, 不塞进 optional 列表).
    pub detail: Option<String>,
}

impl PackageInstallReport {
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn summary_line(&self) -> String {
        let opt = if self.optional_resolved.is_empty() {
            "none".to_owned()
        } else {
            self.optional_resolved.join(",")
        };
        let base = format!(
            "package=status={:?} resolved={} missing={} optional={}",
            self.status,
            self.resolved.len(),
            if self.missing.is_empty() {
                "none".into()
            } else {
                self.missing.join(",")
            },
            opt
        );
        match &self.detail {
            Some(d) => format!("{base} detail={d}"),
            None => base,
        }
    }

    pub fn detail_for_ui(&self) -> String {
        match self.status {
            PackageInstallStatus::Disabled => "入库关闭, package 层未规划".to_owned(),
            PackageInstallStatus::PatternMissing => {
                "缺 steamclient pattern, package 已降级".to_owned()
            }
            PackageInstallStatus::SymbolsMissing => {
                format!("缺 package 符号: {}", self.missing.join(","))
            }
            PackageInstallStatus::LogicOnly => {
                let detail = self.detail.as_deref().unwrap_or("detour 未挂");
                format!("P0 符号就绪 ({}), {detail}", self.resolved.len())
            }
            PackageInstallStatus::HooksAttached => {
                let detail = self.detail.as_deref().unwrap_or("ok");
                format!("package detour 已挂上 ({detail})")
            }
        }
    }

    pub fn attach_detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

/// `component` 一般为 `"steamclient"`.
///
/// 开关跟 `catalog_add`: 入库工具关则不规划 client 拥有权路径.
pub fn plan_package_install(
    tools: &ToolRegistry,
    patterns: &PatternStore,
    component: &str,
) -> PackageInstallReport {
    if !tools.is_enabled(ToolId::CatalogAdd) {
        return PackageInstallReport {
            status: PackageInstallStatus::Disabled,
            resolved: Vec::new(),
            missing: Vec::new(),
            optional_resolved: Vec::new(),
            detail: None,
        };
    }

    let map = match patterns.map(component) {
        Some(map) if !patterns.is_failed(component) => map,
        _ => {
            return PackageInstallReport {
                status: PackageInstallStatus::PatternMissing,
                resolved: Vec::new(),
                missing: PACKAGE_P0_SYMBOLS
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
                optional_resolved: Vec::new(),
                detail: None,
            }
        }
    };

    let mut resolved = Vec::new();
    let mut missing = Vec::new();
    for &name in PACKAGE_P0_SYMBOLS {
        if map.get_by_name(name).is_some() {
            resolved.push(name.to_string());
        } else {
            missing.push(name.to_string());
        }
    }

    let mut optional_resolved = Vec::new();
    for &name in PACKAGE_OPTIONAL_SYMBOLS {
        if map.get_by_name(name).is_some() {
            optional_resolved.push(name.to_string());
        }
    }

    let hooks_ok = PACKAGE_HOOK_SYMBOLS
        .iter()
        .all(|n| map.get_by_name(n).is_some());
    let p0_ok = missing.is_empty();

    let status = if !hooks_ok || !p0_ok {
        PackageInstallStatus::SymbolsMissing
    } else {
        // 符号齐: host 可再调 try_install_package_hooks 尝试 attach.
        PackageInstallStatus::LogicOnly
    };

    PackageInstallReport {
        status,
        resolved,
        missing,
        optional_resolved,
        detail: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stt_metadata::fnv1a32_str;

    fn store_with(text: &str) -> PatternStore {
        let mut s = PatternStore::new();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("p.toml");
        std::fs::write(&p, text).unwrap();
        s.load_file("steamclient", &p).unwrap();
        // PatternMap 已在内存, 临时目录可随 dir drop.
        s
    }

    fn body_all_p0() -> String {
        let mut body = String::new();
        for name in PACKAGE_P0_SYMBOLS
            .iter()
            .chain(PACKAGE_OPTIONAL_SYMBOLS.iter())
        {
            let h = fnv1a32_str(name);
            body.push_str(&format!(
                "[0x{h:08X}]\nname = \"{name}\"\nrva = \"0x10\"\nsig = \"90\"\n\n"
            ));
        }
        body
    }

    #[test]
    fn disabled_when_catalog_off() {
        let mut tools = ToolRegistry::with_defaults();
        tools.set_enabled(ToolId::CatalogAdd, false);
        let r = plan_package_install(&tools, &PatternStore::new(), "steamclient");
        assert_eq!(r.status, PackageInstallStatus::Disabled);
    }

    #[test]
    fn pattern_missing() {
        let tools = ToolRegistry::with_defaults();
        let r = plan_package_install(&tools, &PatternStore::new(), "steamclient");
        assert_eq!(r.status, PackageInstallStatus::PatternMissing);
    }

    #[test]
    fn logic_only_when_p0_present() {
        let tools = ToolRegistry::with_defaults();
        let store = store_with(&body_all_p0());
        let r = plan_package_install(&tools, &store, "steamclient");
        assert_eq!(r.status, PackageInstallStatus::LogicOnly);
        assert_eq!(r.resolved.len(), PACKAGE_P0_SYMBOLS.len());
        assert!(r.optional_resolved.contains(&"LoadPackage".to_string()));
        assert!(r.summary_line().contains("package=status=LogicOnly"));
    }

    #[test]
    fn symbols_missing_when_hook_absent() {
        let tools = ToolRegistry::with_defaults();
        let mut body = String::new();
        for name in PACKAGE_P0_SYMBOLS
            .iter()
            .filter(|n| **n != "CheckAppOwnership")
        {
            let h = fnv1a32_str(name);
            body.push_str(&format!(
                "[0x{h:08X}]\nname = \"{name}\"\nrva = \"0x1\"\n\n"
            ));
        }
        let store = store_with(&body);
        let r = plan_package_install(&tools, &store, "steamclient");
        assert_eq!(r.status, PackageInstallStatus::SymbolsMissing);
        assert!(r.missing.iter().any(|m| m == "CheckAppOwnership"));
    }
}
