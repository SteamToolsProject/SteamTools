//! 宿主共享的 app/depot 状态.

use std::collections::{HashMap, HashSet};

pub type AppId = u32;
pub type DepotId = u32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestOverride {
    pub manifest_gid: u64,
    pub size: u64,
}

/// 目录源 (或 Lua/TOML 应用) 得到的结构化结果.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogBundle {
    pub apps: Vec<AppId>,
    pub app_depots: HashMap<AppId, Vec<DepotId>>,
    pub depot_keys: HashMap<DepotId, String>,
    pub access_tokens: HashMap<AppId, u64>,
    pub manifests: HashMap<DepotId, ManifestOverride>,
    pub purchase_times: HashMap<AppId, u32>,
}

#[derive(Debug, Clone, Default)]
pub struct AppRules {
    owned: HashSet<AppId>,
    app_depots: HashMap<AppId, Vec<DepotId>>,
    depot_keys: HashMap<DepotId, String>,
    access_tokens: HashMap<AppId, u64>,
    manifest_overrides: HashMap<DepotId, ManifestOverride>,
    purchase_time: HashMap<AppId, u32>,
    /// 有实质变更时递增.
    epoch: u64,
}

impl AppRules {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn is_owned(&self, app_id: AppId) -> bool {
        self.owned.contains(&app_id)
    }

    pub fn owned_iter(&self) -> impl Iterator<Item = AppId> + '_ {
        self.owned.iter().copied()
    }

    pub fn owned_count(&self) -> usize {
        self.owned.len()
    }

    pub fn add_app(&mut self, app_id: AppId) {
        if self.owned.insert(app_id) {
            self.bump();
        }
    }

    pub fn remove_app(&mut self, app_id: AppId) {
        let mut changed = self.owned.remove(&app_id);
        if let Some(depots) = self.app_depots.remove(&app_id) {
            for depot_id in depots {
                let still_used = self.app_depots.values().any(|ids| ids.contains(&depot_id));
                if !still_used {
                    self.depot_keys.remove(&depot_id);
                    self.manifest_overrides.remove(&depot_id);
                }
            }
            changed = true;
        }
        changed |= self.access_tokens.remove(&app_id).is_some();
        changed |= self.purchase_time.remove(&app_id).is_some();
        if changed {
            self.bump();
        }
    }

    pub fn set_depot_key(&mut self, depot_id: DepotId, key_hex: impl Into<String>) {
        let key = key_hex.into();
        let key_changed = self.depot_keys.get(&depot_id).map(String::as_str) != Some(key.as_str());
        if key_changed {
            self.depot_keys.insert(depot_id, key);
        }
        if key_changed {
            self.bump();
        }
    }

    pub fn depot_key(&self, depot_id: DepotId) -> Option<&str> {
        self.depot_keys.get(&depot_id).map(String::as_str)
    }

    pub fn depot_keys_iter(&self) -> impl Iterator<Item = (DepotId, &str)> + '_ {
        self.depot_keys
            .iter()
            .map(|(&depot_id, key)| (depot_id, key.as_str()))
    }

    pub fn has_depot_keys(&self) -> bool {
        !self.depot_keys.is_empty()
    }

    pub fn app_depots(&self, app_id: AppId) -> &[DepotId] {
        self.app_depots
            .get(&app_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn app_has_depot_key(&self, app_id: AppId) -> bool {
        self.app_depots(app_id)
            .iter()
            .any(|depot_id| self.depot_keys.contains_key(depot_id))
    }

    pub fn set_access_token(&mut self, app_id: AppId, token: u64) {
        if self.access_tokens.get(&app_id).copied() == Some(token) {
            return;
        }
        self.access_tokens.insert(app_id, token);
        self.bump();
    }

    pub fn access_token(&self, app_id: AppId) -> Option<u64> {
        self.access_tokens.get(&app_id).copied()
    }

    pub fn has_access_tokens(&self) -> bool {
        !self.access_tokens.is_empty()
    }

    pub fn set_manifest_override(&mut self, depot_id: DepotId, over: ManifestOverride) {
        if self.manifest_overrides.get(&depot_id) == Some(&over) {
            return;
        }
        self.manifest_overrides.insert(depot_id, over);
        self.bump();
    }

    pub fn manifest_override(&self, depot_id: DepotId) -> Option<&ManifestOverride> {
        self.manifest_overrides.get(&depot_id)
    }

    pub fn manifest_overrides_iter(
        &self,
    ) -> impl Iterator<Item = (DepotId, &ManifestOverride)> + '_ {
        self.manifest_overrides
            .iter()
            .map(|(&depot_id, over)| (depot_id, over))
    }

    pub fn has_manifest_overrides(&self) -> bool {
        !self.manifest_overrides.is_empty()
    }

    pub fn set_purchase_time(&mut self, app_id: AppId, unix_secs: u32) {
        if self.purchase_time.get(&app_id).copied() == Some(unix_secs) {
            return;
        }
        self.purchase_time.insert(app_id, unix_secs);
        self.bump();
    }

    pub fn purchase_time(&self, app_id: AppId) -> Option<u32> {
        self.purchase_time.get(&app_id).copied()
    }

    /// 合并目录结果; 相同数据尽量不重复 bump.
    pub fn apply_catalog_bundle(&mut self, bundle: &CatalogBundle) {
        let mut changed = false;

        for &app in &bundle.apps {
            changed |= self.owned.insert(app);
        }
        for (&app, depots) in &bundle.app_depots {
            if self.app_depots.get(&app) != Some(depots) {
                self.app_depots.insert(app, depots.clone());
                changed = true;
            }
        }
        for (&depot, key) in &bundle.depot_keys {
            if self.depot_keys.get(&depot).map(String::as_str) != Some(key.as_str()) {
                self.depot_keys.insert(depot, key.clone());
                changed = true;
            }
        }
        for (&app, &token) in &bundle.access_tokens {
            if self.access_tokens.get(&app).copied() != Some(token) {
                self.access_tokens.insert(app, token);
                changed = true;
            }
        }
        for (&depot, over) in &bundle.manifests {
            if self.manifest_overrides.get(&depot) != Some(over) {
                self.manifest_overrides.insert(depot, over.clone());
                changed = true;
            }
        }
        for (&app, &t) in &bundle.purchase_times {
            if self.purchase_time.get(&app).copied() != Some(t) {
                self.purchase_time.insert(app, t);
                changed = true;
            }
        }

        if changed {
            self.bump();
        }
    }

    fn bump(&mut self) {
        self.epoch = self.epoch.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_app_bumps_epoch_and_marks_owned() {
        let mut rules = AppRules::new();
        assert_eq!(rules.epoch(), 0);
        rules.add_app(1361510);
        assert!(rules.is_owned(1361510));
        assert_eq!(rules.epoch(), 1);
        rules.add_app(1361510);
        assert_eq!(rules.epoch(), 1, "duplicate add must not bump epoch");
    }

    #[test]
    fn remove_app_clears_related_fields() {
        let mut rules = AppRules::new();
        rules.add_app(1);
        rules.app_depots.insert(1, vec![2]);
        rules.set_depot_key(2, "ab");
        rules.set_purchase_time(1, 123);
        rules.remove_app(1);
        assert!(!rules.is_owned(1));
        assert!(rules.depot_key(2).is_none());
        assert!(rules.purchase_time(1).is_none());
    }

    #[test]
    fn apply_catalog_bundle_merges_fields() {
        let mut rules = AppRules::new();
        let mut bundle = CatalogBundle::default();
        bundle.apps.push(42);
        bundle.app_depots.insert(42, vec![7]);
        bundle.depot_keys.insert(7, "aa".into());
        bundle.access_tokens.insert(42, 99);
        bundle.manifests.insert(
            7,
            ManifestOverride {
                manifest_gid: 100,
                size: 0,
            },
        );
        rules.apply_catalog_bundle(&bundle);
        assert!(rules.is_owned(42));
        assert_eq!(rules.depot_key(7), Some("aa"));
        assert_eq!(rules.access_token(42), Some(99));
        assert_eq!(
            rules.manifest_override(7).map(|m| m.manifest_gid),
            Some(100)
        );
        assert!(rules.epoch() > 0);

        let epoch = rules.epoch();
        rules.apply_catalog_bundle(&bundle);
        assert_eq!(rules.epoch(), epoch, "identical re-apply is a no-op");
    }

    #[test]
    fn app_has_depot_key_uses_explicit_relationship() {
        let mut rules = AppRules::new();
        let mut bundle = CatalogBundle::default();
        bundle.apps.push(42);
        bundle.app_depots.insert(42, vec![43]);
        bundle.depot_keys.insert(43, "aa".into());
        rules.apply_catalog_bundle(&bundle);

        assert!(rules.app_has_depot_key(42));
    }
}
