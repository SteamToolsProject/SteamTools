//! Catalog provider trait + mock (no network).

use stt_core::{AppId, CatalogBundle, ManifestOverride};

use crate::error::{ConfigError, Result};

pub trait CatalogProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn fetch(&self, app_id: AppId) -> Result<CatalogBundle>;
}

/// In-memory provider for tests and offline UI wiring.
#[derive(Debug, Clone, Default)]
pub struct MockCatalogProvider {
    /// Pre-seeded bundles keyed by app id.
    pub fixtures: std::collections::HashMap<AppId, CatalogBundle>,
    /// If set, every fetch returns this error.
    pub fail_with: Option<String>,
}

impl MockCatalogProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_fixture(mut self, app_id: AppId, bundle: CatalogBundle) -> Self {
        self.fixtures.insert(app_id, bundle);
        self
    }

    /// Convenience: mark app owned with a fake depot key + manifest.
    pub fn with_simple_app(mut self, app_id: AppId, depot_id: u64, key_hex: &str, gid: u64) -> Self {
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
}

impl CatalogProvider for MockCatalogProvider {
    fn id(&self) -> &'static str {
        "mock"
    }

    fn fetch(&self, app_id: AppId) -> Result<CatalogBundle> {
        if let Some(msg) = &self.fail_with {
            return Err(ConfigError::Invalid(msg.clone()));
        }
        self.fixtures
            .get(&app_id)
            .cloned()
            .ok_or_else(|| ConfigError::Invalid(format!("mock has no fixture for app {app_id}")))
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
