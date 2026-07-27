//! 离线 Catalog provider.

use std::collections::HashMap;

use stt_core::{AppId, CatalogBundle, ManifestOverride};

use crate::{validate_bundle, CatalogError, CatalogProvider, CatalogResult, ProviderErrorKind};

/// 内存 Mock, 仅用于测试和显式开发模式.
#[derive(Debug, Clone, Default)]
pub struct MockCatalogProvider {
    fixtures: HashMap<AppId, CatalogBundle>,
    failure: Option<String>,
    auto_generate: bool,
}

impl MockCatalogProvider {
    /// 创建一个没有 fixture 的 Mock.
    pub fn new() -> Self {
        Self::default()
    }

    /// 控制无 fixture 时是否生成确定性的假数据.
    #[must_use]
    pub fn with_auto_generate(mut self, enabled: bool) -> Self {
        self.auto_generate = enabled;
        self
    }

    /// 添加一个 fixture.
    #[must_use]
    pub fn with_fixture(mut self, app_id: AppId, bundle: CatalogBundle) -> Self {
        self.fixtures.insert(app_id, bundle);
        self
    }

    /// 让每次 fetch 都返回 unavailable 错误.
    #[must_use]
    pub fn with_failure(mut self, detail: impl Into<String>) -> Self {
        self.failure = Some(detail.into());
        self
    }

    /// 添加一个带 depot key 和 manifest 的 fixture.
    #[must_use]
    pub fn with_simple_app(
        mut self,
        app_id: AppId,
        depot_id: u32,
        key_hex: &str,
        manifest_gid: u64,
    ) -> Self {
        let mut bundle = CatalogBundle::default();
        bundle.apps.push(app_id);
        bundle.app_depots.insert(app_id, vec![depot_id]);
        bundle.depot_keys.insert(depot_id, key_hex.to_owned());
        bundle.manifests.insert(
            depot_id,
            ManifestOverride {
                manifest_gid,
                size: 0,
            },
        );
        self.fixtures.insert(app_id, bundle);
        self
    }

    fn synthetic(app_id: AppId) -> CatalogBundle {
        let mut bundle = CatalogBundle::default();
        bundle.apps.push(app_id);
        bundle.app_depots.insert(app_id, vec![app_id]);
        bundle
            .depot_keys
            .insert(app_id, format!("{app_id:08x}").repeat(8));
        bundle.manifests.insert(
            app_id,
            ManifestOverride {
                manifest_gid: u64::from(app_id).saturating_mul(1000).saturating_add(1),
                size: 0,
            },
        );
        bundle
    }
}

impl CatalogProvider for MockCatalogProvider {
    fn id(&self) -> &str {
        "mock"
    }

    fn fetch(&self, app_id: AppId) -> CatalogResult<CatalogBundle> {
        if let Some(detail) = &self.failure {
            return Err(CatalogError::Provider {
                provider: self.id().to_owned(),
                kind: ProviderErrorKind::Unavailable,
                detail: detail.clone(),
            });
        }
        let bundle = self
            .fixtures
            .get(&app_id)
            .cloned()
            .or_else(|| self.auto_generate.then(|| Self::synthetic(app_id)))
            .ok_or_else(|| CatalogError::Provider {
                provider: self.id().to_owned(),
                kind: ProviderErrorKind::NotFound,
                detail: format!("no fixture for app {app_id}"),
            })?;

        validate_bundle(app_id, bundle)
    }
}

#[cfg(test)]
mod tests {
    use stt_core::AppRules;

    use super::*;

    #[test]
    fn fetch_applies_depot_key_by_depot_id() {
        let provider = MockCatalogProvider::new().with_simple_app(10, 20, &"ab".repeat(32), 99);
        let bundle = provider.fetch(10).unwrap();
        let mut rules = AppRules::new();
        rules.apply_catalog_bundle(&bundle);

        assert_eq!(rules.depot_key(20).map(str::len), Some(64));
    }

    #[test]
    fn fetch_rejects_missing_fixture() {
        let error = MockCatalogProvider::new().fetch(1).unwrap_err();

        assert!(matches!(
            error,
            CatalogError::Provider {
                kind: ProviderErrorKind::NotFound,
                ..
            }
        ));
    }

    #[test]
    fn fetch_validates_fixture_before_returning() {
        let bundle = CatalogBundle {
            apps: vec![10],
            depot_keys: HashMap::from([(20, "bad".into())]),
            ..CatalogBundle::default()
        };
        let provider = MockCatalogProvider::new().with_fixture(10, bundle);

        let error = provider.fetch(10).unwrap_err();

        assert!(matches!(error, CatalogError::UndeclaredDepot(20)));
    }
}
