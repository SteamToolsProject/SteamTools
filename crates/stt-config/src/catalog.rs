//! 目录源 trait 与 Mock (不联网).

use stt_core::{AppId, CatalogBundle, ManifestOverride};

use crate::error::{ConfigError, Result};

pub trait CatalogProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn fetch(&self, app_id: AppId) -> Result<CatalogBundle>;
}

/// 内存 Mock, 给测试和离线 UI 接线用.
#[derive(Debug, Clone, Default)]
pub struct MockCatalogProvider {
    /// 按 app id 预置的 bundle.
    pub fixtures: std::collections::HashMap<AppId, CatalogBundle>,
    /// 若设置, 每次 fetch 都返回该错误.
    pub fail_with: Option<String>,
    /// 无 fixture 时自动生成假 key/manifest (仅开发/狗粮).
    pub auto_generate: bool,
}

impl MockCatalogProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_auto_generate(mut self, on: bool) -> Self {
        self.auto_generate = on;
        self
    }

    pub fn with_fixture(mut self, app_id: AppId, bundle: CatalogBundle) -> Self {
        self.fixtures.insert(app_id, bundle);
        self
    }

    /// 便捷: 标记拥有, 并塞假 depot key + manifest.
    pub fn with_simple_app(
        mut self,
        app_id: AppId,
        depot_id: u64,
        key_hex: &str,
        gid: u64,
    ) -> Self {
        let mut bundle = CatalogBundle::default();
        bundle.apps.push(app_id);
        bundle.depot_keys.insert(app_id, key_hex.to_string());
        bundle.manifests.insert(
            depot_id,
            ManifestOverride {
                manifest_gid: gid,
                size: 0,
            },
        );
        self.fixtures.insert(app_id, bundle);
        self
    }

    fn synthetic(app_id: AppId) -> CatalogBundle {
        let mut bundle = CatalogBundle::default();
        bundle.apps.push(app_id);
        // 64 hex, 确定性假 key (非真实 depot key).
        let key = format!("{app_id:08x}").repeat(8);
        bundle.depot_keys.insert(app_id, key);
        bundle.manifests.insert(
            u64::from(app_id),
            ManifestOverride {
                manifest_gid: u64::from(app_id).saturating_mul(1000).saturating_add(1),
                size: 0,
            },
        );
        bundle
    }
}

impl CatalogProvider for MockCatalogProvider {
    fn id(&self) -> &'static str {
        "mock"
    }

    fn fetch(&self, app_id: AppId) -> Result<CatalogBundle> {
        if let Some(msg) = &self.fail_with {
            return Err(ConfigError::Invalid(msg.clone()));
        }
        if let Some(b) = self.fixtures.get(&app_id) {
            return Ok(b.clone());
        }
        if self.auto_generate {
            return Ok(Self::synthetic(app_id));
        }
        Err(ConfigError::Invalid(format!(
            "mock has no fixture for app {app_id}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stt_core::AppRules;

    #[test]
    fn mock_fetch_applies_to_rules() {
        let provider = MockCatalogProvider::new().with_simple_app(10, 20, "deadbeef", 99);
        let bundle = provider.fetch(10).unwrap();
        let mut rules = AppRules::new();
        rules.apply_catalog_bundle(&bundle);
        assert!(rules.is_owned(10));
        assert_eq!(rules.depot_key(10), Some("deadbeef"));
        assert_eq!(rules.manifest_override(20).unwrap().manifest_gid, 99);
    }

    #[test]
    fn mock_missing_is_error() {
        let provider = MockCatalogProvider::new();
        assert!(provider.fetch(1).is_err());
    }
}
