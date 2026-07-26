//! 工具注册表骨架 (id, 默认开关; 尚无真实 hook).

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolId {
    CatalogAdd,
    LibraryUx,
    StoreAccel,
}

impl ToolId {
    pub const ALL: &'static [ToolId] = &[ToolId::CatalogAdd, ToolId::LibraryUx, ToolId::StoreAccel];

    pub fn as_str(self) -> &'static str {
        match self {
            ToolId::CatalogAdd => "catalog_add",
            ToolId::LibraryUx => "library_ux",
            ToolId::StoreAccel => "store_accel",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "catalog_add" => Some(Self::CatalogAdd),
            "library_ux" => Some(Self::LibraryUx),
            "store_accel" => Some(Self::StoreAccel),
            _ => None,
        }
    }

    pub fn default_enabled(self) -> bool {
        matches!(self, ToolId::CatalogAdd | ToolId::LibraryUx)
    }

    pub fn display_name(self) -> &'static str {
        match self {
            ToolId::CatalogAdd => "Catalog Add",
            ToolId::LibraryUx => "Library UX",
            ToolId::StoreAccel => "Store Accel",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolManifest {
    pub id: ToolId,
    pub name: &'static str,
    pub default_enabled: bool,
    /// UI 贡献点落地前的占位.
    pub needs_client: bool,
}

pub fn builtin_manifests() -> Vec<ToolManifest> {
    ToolId::ALL
        .iter()
        .copied()
        .map(|id| ToolManifest {
            id,
            name: id.display_name(),
            default_enabled: id.default_enabled(),
            needs_client: matches!(id, ToolId::StoreAccel),
        })
        .collect()
}

pub fn default_tool_enabled_map() -> HashMap<String, bool> {
    ToolId::ALL
        .iter()
        .map(|id| (id.as_str().to_string(), id.default_enabled()))
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct ToolRegistry {
    enabled: HashMap<ToolId, bool>,
}

impl ToolRegistry {
    pub fn with_defaults() -> Self {
        let mut enabled = HashMap::new();
        for id in ToolId::ALL {
            enabled.insert(*id, id.default_enabled());
        }
        Self { enabled }
    }

    pub fn apply_overrides(&mut self, overrides: &HashMap<String, bool>) {
        for (k, v) in overrides {
            if let Some(id) = ToolId::parse(k) {
                self.enabled.insert(id, *v);
            }
        }
    }

    pub fn from_host_tools(enabled: &HashMap<String, bool>) -> Self {
        let mut reg = Self::with_defaults();
        reg.apply_overrides(enabled);
        reg
    }

    pub fn is_enabled(&self, id: ToolId) -> bool {
        self.enabled
            .get(&id)
            .copied()
            .unwrap_or_else(|| id.default_enabled())
    }

    pub fn set_enabled(&mut self, id: ToolId, on: bool) {
        self.enabled.insert(id, on);
    }

    pub fn enabled_ids(&self) -> impl Iterator<Item = ToolId> + '_ {
        ToolId::ALL
            .iter()
            .copied()
            .filter(|id| self.is_enabled(*id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_product() {
        let reg = ToolRegistry::with_defaults();
        assert!(reg.is_enabled(ToolId::CatalogAdd));
        assert!(reg.is_enabled(ToolId::LibraryUx));
        assert!(!reg.is_enabled(ToolId::StoreAccel));
    }

    #[test]
    fn override_disables_library_ux() {
        let mut map = HashMap::new();
        map.insert("library_ux".into(), false);
        let reg = ToolRegistry::from_host_tools(&map);
        assert!(!reg.is_enabled(ToolId::LibraryUx));
        assert!(reg.is_enabled(ToolId::CatalogAdd));
    }
}
