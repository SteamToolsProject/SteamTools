//! download_kit 的纯逻辑能力门禁与报告.

use stt_config::{ToolId, ToolRegistry};
use stt_metadata::PatternStore;

const MANIFEST_SYMBOLS: &[&str] = &["BuildDepotDependency"];
const KEY_SYMBOLS: &[&str] = &["ConfigStoreGetBinary"];
const TOKEN_SYMBOLS: &[&str] = &["BBuildAndAsyncSendFrame"];
const REQUEST_CODE_SYMBOLS: &[&str] = &["BBuildAndAsyncSendFrame", "RecvPkt"];

/// download_kit 内彼此独立的能力.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadCapability {
    ManifestOverride,
    DepotKey,
    AccessToken,
    RequestCode,
}

impl DownloadCapability {
    pub const ALL: [Self; 4] = [
        Self::ManifestOverride,
        Self::DepotKey,
        Self::AccessToken,
        Self::RequestCode,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ManifestOverride => "manifest",
            Self::DepotKey => "key",
            Self::AccessToken => "token",
            Self::RequestCode => "request-code",
        }
    }

    const fn required_symbols(self) -> &'static [&'static str] {
        match self {
            Self::ManifestOverride => MANIFEST_SYMBOLS,
            Self::DepotKey => KEY_SYMBOLS,
            Self::AccessToken => TOKEN_SYMBOLS,
            Self::RequestCode => REQUEST_CODE_SYMBOLS,
        }
    }
}

/// 当前构建包含哪些能力.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadFeatureSet {
    pub manifest: bool,
    pub key: bool,
    pub token: bool,
    pub request_code: bool,
}

impl DownloadFeatureSet {
    pub const fn compiled() -> Self {
        Self {
            manifest: cfg!(feature = "download-manifest"),
            key: cfg!(feature = "download-key"),
            token: cfg!(feature = "download-token"),
            request_code: cfg!(feature = "download-request-code"),
        }
    }

    fn enabled(self, capability: DownloadCapability) -> bool {
        match capability {
            DownloadCapability::ManifestOverride => self.manifest,
            DownloadCapability::DepotKey => self.key,
            DownloadCapability::AccessToken => self.token,
            DownloadCapability::RequestCode => self.request_code,
        }
    }
}

/// 每项能力的运行时开关. `false` 表示环境变量显式关闭.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadRuntimeSwitches {
    pub manifest: bool,
    pub key: bool,
    pub token: bool,
    pub request_code: bool,
}

impl Default for DownloadRuntimeSwitches {
    fn default() -> Self {
        Self {
            manifest: true,
            key: true,
            token: true,
            request_code: true,
        }
    }
}

impl DownloadRuntimeSwitches {
    fn enabled(self, capability: DownloadCapability) -> bool {
        match capability {
            DownloadCapability::ManifestOverride => self.manifest,
            DownloadCapability::DepotKey => self.key,
            DownloadCapability::AccessToken => self.token,
            DownloadCapability::RequestCode => self.request_code,
        }
    }
}

/// core/config 当前是否有可消费的数据.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DownloadDataAvailability {
    pub manifest: bool,
    pub key: bool,
    pub token: bool,
    pub request_code: bool,
}

impl DownloadDataAvailability {
    fn available(self, capability: DownloadCapability) -> bool {
        match capability {
            DownloadCapability::ManifestOverride => self.manifest,
            DownloadCapability::DepotKey => self.key,
            DownloadCapability::AccessToken => self.token,
            DownloadCapability::RequestCode => self.request_code,
        }
    }
}

/// 单项能力当前停在哪一道门禁.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadCapabilityStatus {
    ToolDisabled,
    FeatureDisabled,
    EnvironmentDisabled,
    DataMissing,
    PatternMissing,
    SymbolsMissing,
    LogicOnly,
    HooksAttached,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadCapabilityReport {
    pub capability: DownloadCapability,
    pub status: DownloadCapabilityStatus,
    pub resolved: Vec<String>,
    pub missing: Vec<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadKitReport {
    pub capabilities: Vec<DownloadCapabilityReport>,
}

impl DownloadKitReport {
    pub fn summary_line(&self) -> String {
        self.capabilities
            .iter()
            .map(|report| format!("{}={:?}", report.capability.as_str(), report.status))
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn detail_for_ui(&self) -> String {
        self.capabilities
            .iter()
            .map(|report| {
                let base = match report.status {
                    DownloadCapabilityStatus::SymbolsMissing => format!(
                        "{}=SymbolsMissing({})",
                        report.capability.as_str(),
                        report.missing.join(",")
                    ),
                    status => format!("{}={status:?}", report.capability.as_str()),
                };
                match report.detail.as_deref() {
                    Some(detail) => format!("{base} ({detail})"),
                    None => base,
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// 只计算安装前提与降级原因, 不解析地址也不安装 hook.
pub fn plan_download_kit(
    tools: &ToolRegistry,
    patterns: &PatternStore,
    component: &str,
    features: DownloadFeatureSet,
    switches: DownloadRuntimeSwitches,
    data: DownloadDataAvailability,
) -> DownloadKitReport {
    let capabilities = DownloadCapability::ALL
        .into_iter()
        .map(|capability| {
            plan_capability(
                capability, tools, patterns, component, features, switches, data,
            )
        })
        .collect();
    DownloadKitReport { capabilities }
}

fn plan_capability(
    capability: DownloadCapability,
    tools: &ToolRegistry,
    patterns: &PatternStore,
    component: &str,
    features: DownloadFeatureSet,
    switches: DownloadRuntimeSwitches,
    data: DownloadDataAvailability,
) -> DownloadCapabilityReport {
    let status = if !tools.is_enabled(ToolId::DownloadKit) {
        DownloadCapabilityStatus::ToolDisabled
    } else if !features.enabled(capability) {
        DownloadCapabilityStatus::FeatureDisabled
    } else if !switches.enabled(capability) {
        DownloadCapabilityStatus::EnvironmentDisabled
    } else if !data.available(capability) {
        DownloadCapabilityStatus::DataMissing
    } else if patterns.map(component).is_none() || patterns.is_failed(component) {
        DownloadCapabilityStatus::PatternMissing
    } else {
        DownloadCapabilityStatus::LogicOnly
    };

    if status != DownloadCapabilityStatus::LogicOnly {
        return DownloadCapabilityReport {
            capability,
            status,
            resolved: Vec::new(),
            missing: Vec::new(),
            detail: None,
        };
    }

    let Some(map) = patterns.map(component) else {
        return DownloadCapabilityReport {
            capability,
            status: DownloadCapabilityStatus::PatternMissing,
            resolved: Vec::new(),
            missing: Vec::new(),
            detail: None,
        };
    };
    let mut resolved = Vec::new();
    let mut missing = Vec::new();
    for &name in capability.required_symbols() {
        if map.get_by_name(name).is_some() {
            resolved.push(name.to_owned());
        } else {
            missing.push(name.to_owned());
        }
    }
    let status = if missing.is_empty() {
        DownloadCapabilityStatus::LogicOnly
    } else {
        DownloadCapabilityStatus::SymbolsMissing
    };
    DownloadCapabilityReport {
        capability,
        status,
        resolved,
        missing,
        detail: None,
    }
}

#[cfg(test)]
mod tests {
    use stt_metadata::fnv1a32_str;

    use super::*;

    fn enabled_tools() -> ToolRegistry {
        let mut tools = ToolRegistry::with_defaults();
        tools.set_enabled(ToolId::DownloadKit, true);
        tools
    }

    fn all_features() -> DownloadFeatureSet {
        DownloadFeatureSet {
            manifest: true,
            key: true,
            token: true,
            request_code: true,
        }
    }

    fn all_data() -> DownloadDataAvailability {
        DownloadDataAvailability {
            manifest: true,
            key: true,
            token: true,
            request_code: true,
        }
    }

    fn complete_patterns() -> PatternStore {
        let names = [
            "BuildDepotDependency",
            "ConfigStoreGetBinary",
            "BBuildAndAsyncSendFrame",
            "RecvPkt",
        ];
        let mut text = String::new();
        for name in names {
            text.push_str(&format!(
                "[0x{:08X}]\nname = \"{name}\"\nrva = \"0x10\"\n\n",
                fnv1a32_str(name)
            ));
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("patterns.toml");
        std::fs::write(&path, text).unwrap();
        let mut patterns = PatternStore::new();
        patterns.load_file("steamclient", &path).unwrap();
        patterns
    }

    fn statuses(report: &DownloadKitReport) -> Vec<DownloadCapabilityStatus> {
        report.capabilities.iter().map(|item| item.status).collect()
    }

    #[test]
    fn tool_off_precedes_every_capability_gate() {
        let mut tools = ToolRegistry::with_defaults();
        tools.set_enabled(ToolId::DownloadKit, false);
        let report = plan_download_kit(
            &tools,
            &complete_patterns(),
            "steamclient",
            all_features(),
            DownloadRuntimeSwitches::default(),
            all_data(),
        );

        assert_eq!(
            statuses(&report),
            vec![DownloadCapabilityStatus::ToolDisabled; 4]
        );
    }

    #[test]
    fn reports_feature_env_data_and_pattern_gates_separately() {
        let features = DownloadFeatureSet {
            manifest: false,
            ..all_features()
        };
        let switches = DownloadRuntimeSwitches {
            key: false,
            ..DownloadRuntimeSwitches::default()
        };
        let data = DownloadDataAvailability {
            token: false,
            ..all_data()
        };

        let report = plan_download_kit(
            &enabled_tools(),
            &PatternStore::new(),
            "steamclient",
            features,
            switches,
            data,
        );

        assert_eq!(
            statuses(&report),
            vec![
                DownloadCapabilityStatus::FeatureDisabled,
                DownloadCapabilityStatus::EnvironmentDisabled,
                DownloadCapabilityStatus::DataMissing,
                DownloadCapabilityStatus::PatternMissing,
            ]
        );
    }

    #[test]
    fn each_feature_gate_disables_only_its_own_capability() {
        let cases = [
            DownloadFeatureSet {
                manifest: false,
                ..all_features()
            },
            DownloadFeatureSet {
                key: false,
                ..all_features()
            },
            DownloadFeatureSet {
                token: false,
                ..all_features()
            },
            DownloadFeatureSet {
                request_code: false,
                ..all_features()
            },
        ];

        for (disabled, features) in cases.into_iter().enumerate() {
            let report = plan_download_kit(
                &enabled_tools(),
                &complete_patterns(),
                "steamclient",
                features,
                DownloadRuntimeSwitches::default(),
                all_data(),
            );
            for (index, status) in statuses(&report).into_iter().enumerate() {
                assert_eq!(
                    status,
                    if index == disabled {
                        DownloadCapabilityStatus::FeatureDisabled
                    } else {
                        DownloadCapabilityStatus::LogicOnly
                    }
                );
            }
        }
    }

    #[test]
    fn each_runtime_gate_disables_only_its_own_capability() {
        let cases = [
            DownloadRuntimeSwitches {
                manifest: false,
                ..DownloadRuntimeSwitches::default()
            },
            DownloadRuntimeSwitches {
                key: false,
                ..DownloadRuntimeSwitches::default()
            },
            DownloadRuntimeSwitches {
                token: false,
                ..DownloadRuntimeSwitches::default()
            },
            DownloadRuntimeSwitches {
                request_code: false,
                ..DownloadRuntimeSwitches::default()
            },
        ];

        for (disabled, switches) in cases.into_iter().enumerate() {
            let report = plan_download_kit(
                &enabled_tools(),
                &complete_patterns(),
                "steamclient",
                all_features(),
                switches,
                all_data(),
            );
            for (index, status) in statuses(&report).into_iter().enumerate() {
                assert_eq!(
                    status,
                    if index == disabled {
                        DownloadCapabilityStatus::EnvironmentDisabled
                    } else {
                        DownloadCapabilityStatus::LogicOnly
                    }
                );
            }
        }
    }

    #[test]
    fn each_data_gate_disables_only_its_own_capability() {
        let cases = [
            DownloadDataAvailability {
                manifest: false,
                ..all_data()
            },
            DownloadDataAvailability {
                key: false,
                ..all_data()
            },
            DownloadDataAvailability {
                token: false,
                ..all_data()
            },
            DownloadDataAvailability {
                request_code: false,
                ..all_data()
            },
        ];

        for (disabled, data) in cases.into_iter().enumerate() {
            let report = plan_download_kit(
                &enabled_tools(),
                &complete_patterns(),
                "steamclient",
                all_features(),
                DownloadRuntimeSwitches::default(),
                data,
            );
            for (index, status) in statuses(&report).into_iter().enumerate() {
                assert_eq!(
                    status,
                    if index == disabled {
                        DownloadCapabilityStatus::DataMissing
                    } else {
                        DownloadCapabilityStatus::LogicOnly
                    }
                );
            }
        }
    }

    #[test]
    fn complete_prerequisites_stop_at_logic_only() {
        let report = plan_download_kit(
            &enabled_tools(),
            &complete_patterns(),
            "steamclient",
            all_features(),
            DownloadRuntimeSwitches::default(),
            all_data(),
        );

        assert_eq!(
            statuses(&report),
            vec![DownloadCapabilityStatus::LogicOnly; 4]
        );
    }

    #[test]
    fn missing_symbols_are_reported_per_capability() {
        let mut patterns = complete_patterns();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial.toml");
        let name = "BBuildAndAsyncSendFrame";
        std::fs::write(
            &path,
            format!(
                "[0x{:08X}]\nname = \"{name}\"\nrva = \"0x10\"\n",
                fnv1a32_str(name)
            ),
        )
        .unwrap();
        patterns.load_file("steamclient", &path).unwrap();

        let report = plan_download_kit(
            &enabled_tools(),
            &patterns,
            "steamclient",
            all_features(),
            DownloadRuntimeSwitches::default(),
            all_data(),
        );

        assert_eq!(
            statuses(&report),
            vec![
                DownloadCapabilityStatus::SymbolsMissing,
                DownloadCapabilityStatus::SymbolsMissing,
                DownloadCapabilityStatus::LogicOnly,
                DownloadCapabilityStatus::SymbolsMissing,
            ]
        );
        assert_eq!(report.capabilities[3].missing, vec!["RecvPkt"]);
    }
}
