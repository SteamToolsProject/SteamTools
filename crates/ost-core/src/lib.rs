//! Shared app/depot state used by the rest of the host.

use std::collections::{HashMap, HashSet};

pub type AppId = u32;
pub type DepotId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestOverride {
    pub manifest_gid: u64,
    pub size: u64,
}

#[derive(Debug, Clone, Default)]
pub struct AppRules {
    owned: HashSet<AppId>,
    depot_keys: HashMap<AppId, String>,
    access_tokens: HashMap<AppId, u64>,
    manifest_overrides: HashMap<DepotId, ManifestOverride>,
    purchase_time: HashMap<AppId, u32>,
    /// Bumps when anything meaningful changes.
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

    pub fn add_app(&mut self, app_id: AppId) {
        if self.owned.insert(app_id) {
            self.epoch = self.epoch.saturating_add(1);
        }
    }

    pub fn remove_app(&mut self, app_id: AppId) {
        let mut changed = self.owned.remove(&app_id);
        changed |= self.depot_keys.remove(&app_id).is_some();
        changed |= self.access_tokens.remove(&app_id).is_some();
        changed |= self.purchase_time.remove(&app_id).is_some();
        if changed {
            self.epoch = self.epoch.saturating_add(1);
        }
    }

    pub fn set_depot_key(&mut self, app_id: AppId, key_hex: impl Into<String>) {
        self.owned.insert(app_id);
        self.depot_keys.insert(app_id, key_hex.into());
        self.epoch = self.epoch.saturating_add(1);
    }

    pub fn depot_key(&self, app_id: AppId) -> Option<&str> {
        self.depot_keys.get(&app_id).map(String::as_str)
    }

    pub fn set_access_token(&mut self, app_id: AppId, token: u64) {
        self.access_tokens.insert(app_id, token);
        self.epoch = self.epoch.saturating_add(1);
    }

    pub fn set_manifest_override(&mut self, depot_id: DepotId, over: ManifestOverride) {
        self.manifest_overrides.insert(depot_id, over);
        self.epoch = self.epoch.saturating_add(1);
    }

    pub fn set_purchase_time(&mut self, app_id: AppId, unix_secs: u32) {
        self.purchase_time.insert(app_id, unix_secs);
        self.epoch = self.epoch.saturating_add(1);
    }

    pub fn purchase_time(&self, app_id: AppId) -> Option<u32> {
        self.purchase_time.get(&app_id).copied()
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
        rules.set_depot_key(1, "ab");
        rules.set_purchase_time(1, 123);
        rules.remove_app(1);
        assert!(!rules.is_owned(1));
        assert!(rules.depot_key(1).is_none());
        assert!(rules.purchase_time(1).is_none());
    }
}
