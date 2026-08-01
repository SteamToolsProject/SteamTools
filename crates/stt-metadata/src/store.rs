//! 按组件加载 pattern, 并记录缺失符号.

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

    /// 从指定文件加载; 失败则标记该组件禁用.
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

    /// 优先主路径, 否则 legacy; 文件不存在 -> 模块禁用 (非硬错误).
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

    /// 仅用 pattern 里的 RVA (不扫映像). 大 DLL 上避免整模块拷贝.
    ///
    /// `image_size` 为模块映像大小: rva >= image_size 视为越界, 记入 missing 并返回 None.
    pub fn find_by_rva_only(
        &self,
        component: &str,
        name: &str,
        module_base: usize,
        image_size: usize,
    ) -> Option<usize> {
        if self.failed.contains(component) {
            return None;
        }
        let map = self.maps.get(component)?;
        let entry = map.get_by_name(name)?;
        let rva = entry.rva? as usize;
        if rva >= image_size {
            let mut g = self
                .missing
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.push(format!("{component}::{name}"));
            return None;
        }
        Some(module_base.wrapping_add(rva))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fnv::fnv1a32_str;

    fn store_with_rva(rva: u64) -> PatternStore {
        let hash = fnv1a32_str("DemoFunc");
        let text = format!(
            r#"
[0x{hash:08X}]
name = "DemoFunc"
rva = "0x{rva:X}"
"#
        );
        let map = PatternMap::parse_str("steamui", &text).unwrap();
        let mut store = PatternStore::new();
        store.maps.insert("steamui".to_string(), map);
        store
    }

    #[test]
    fn find_by_rva_only_in_bounds() {
        let store = store_with_rva(0x1000);
        assert_eq!(
            store.find_by_rva_only("steamui", "DemoFunc", 0x7000_0000, 0x100_0000),
            Some(0x7000_1000)
        );
        assert!(store.take_missing().is_empty());
    }

    #[test]
    fn find_by_rva_only_equal_to_image_size_is_missing() {
        let store = store_with_rva(0x100_0000);
        assert_eq!(
            store.find_by_rva_only("steamui", "DemoFunc", 0x7000_0000, 0x100_0000),
            None
        );
        assert_eq!(store.take_missing(), vec!["steamui::DemoFunc"]);
    }

    #[test]
    fn find_by_rva_only_out_of_bounds_is_missing() {
        let store = store_with_rva(0x100_0001);
        assert_eq!(
            store.find_by_rva_only("steamui", "DemoFunc", 0x7000_0000, 0x100_0000),
            None
        );
        assert_eq!(store.take_missing(), vec!["steamui::DemoFunc"]);
    }

    #[test]
    fn find_by_rva_only_failed_component_is_none() {
        let mut store = store_with_rva(0x1000);
        store.failed.insert("steamui".to_string());
        assert_eq!(
            store.find_by_rva_only("steamui", "DemoFunc", 0x7000_0000, 0x100_0000),
            None
        );
    }
}
