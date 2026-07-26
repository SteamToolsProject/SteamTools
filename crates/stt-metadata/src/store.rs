//! Per-component pattern load + missing-symbol bookkeeping.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{MetadataError, Result};
use crate::pattern::{resolve_in_image, PatternMap};

#[derive(Debug, Default)]
pub struct PatternStore {
    maps: HashMap<String, PatternMap>,
    failed: HashSet<String>,
    missing: Mutex<Vec<String>>,
}

impl PatternStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_failed(&self, component: &str) -> bool {
        self.failed.contains(component)
    }

    pub fn map(&self, component: &str) -> Option<&PatternMap> {
        self.maps.get(component)
    }

    /// Load from an explicit file. On failure the component is marked disabled.
    pub fn load_file(&mut self, component: &str, path: &Path) -> Result<()> {
        match PatternMap::load_file(component, path) {
            Ok(map) => {
                self.failed.remove(component);
                self.maps.insert(component.to_string(), map);
                Ok(())
            }
            Err(e) => {
                self.maps.remove(component);
                self.failed.insert(component.to_string());
                Err(e)
            }
        }
    }

    /// Prefer primary path, else legacy path; missing file → module disabled (not hard error).
    pub fn load_with_fallback(
        &mut self,
        component: &str,
        primary: &Path,
        legacy: Option<&Path>,
    ) -> Result<PathBufLoad> {
        if primary.is_file() {
            self.load_file(component, primary)?;
            return Ok(PathBufLoad {
                path: primary.to_path_buf(),
                legacy: false,
            });
        }
        if let Some(leg) = legacy {
            if leg.is_file() {
                self.load_file(component, leg)?;
                return Ok(PathBufLoad {
                    path: leg.to_path_buf(),
                    legacy: true,
                });
            }
        }
        self.maps.remove(component);
        self.failed.insert(component.to_string());
        Err(MetadataError::ModuleDisabled)
    }

    pub fn find_in_image(
        &self,
        component: &str,
        name: &str,
        image: &[u8],
        module_base: usize,
    ) -> Option<usize> {
        if self.failed.contains(component) {
            return None;
        }
        let map = self.maps.get(component)?;
        match resolve_in_image(map, name, image, module_base) {
            Some(addr) => Some(addr),
            None => {
                let mut g = self
                    .missing
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                g.push(format!("{component}::{name}"));
                None
            }
        }
    }

    pub fn take_missing(&self) -> Vec<String> {
        let mut g = self
            .missing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *g)
    }
}

#[derive(Debug, Clone)]
pub struct PathBufLoad {
    pub path: PathBuf,
    pub legacy: bool,
}
