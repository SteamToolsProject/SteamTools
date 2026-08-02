//! 宿主配置: TOML, 工具注册表, Catalog 落盘, 可选 Lua DSL.

mod appinfo;
mod catalog_add;
mod error;
mod host_toml;
mod intent;
mod snapshot;
mod tools;
mod watch;

#[cfg(feature = "lua")]
mod lua_catalog;
#[cfg(feature = "lua")]
mod lua_dsl;
#[cfg(feature = "lua")]
mod lua_http;
#[cfg(feature = "lua")]
mod lua_load;
#[cfg(feature = "lua")]
mod lua_manifest_code;
#[cfg(feature = "lua")]
mod lua_vm;

pub use appinfo::app_names;
#[cfg(feature = "lua")]
pub use catalog_add::remove_from_library;
pub use catalog_add::{
    add_to_library, catalog_lua_path, format_catalog_lua, manifest_file_name, write_catalog_lua,
    write_manifest_blobs, AddToLibraryOutcome, MissingDownloadData,
};
pub use error::{ConfigError, Result};
pub use host_toml::{
    CatalogMode, CatalogSection, HostConfig, LogSection, LuaSection, ManifestSection,
    StoreAccelEgress, StoreAccelSection, ToolsSection, HOST_TOML_NAME, LEGACY_TOML_NAME,
};
pub use intent::{
    apply_intent, host_toml_write_path, save_host_change, ConfigIntent, CATALOG_MODES, LOG_LEVELS,
    MANIFEST_SOURCES,
};
pub use snapshot::{managed_apps, ConfigSnapshot, HostFacts, ToolDetails, ToolView};
pub use tools::{builtin_manifests, ToolId, ToolManifest, ToolRegistry};
pub use watch::DebouncedWatcher;

#[cfg(feature = "lua")]
pub use lua_catalog::LuaCatalogProvider;
#[cfg(feature = "lua")]
pub use lua_dsl::{apply_lua_chunk, eval_lua_to_bundle, eval_lua_to_bundle_with_http};
#[cfg(feature = "lua")]
pub use lua_http::{
    LuaHttpClient, LuaHttpErrorKind, LuaHttpMethod, LuaHttpRequest, LuaHttpResponse,
};
#[cfg(feature = "lua")]
pub use lua_load::{
    default_lua_dir, list_lua_files, list_lua_files_in_dirs, load_lua_directories, lua_search_dirs,
    LuaLoadReport,
};
#[cfg(feature = "lua")]
pub use lua_manifest_code::{
    LuaManifestCodeErrorKind, LuaManifestCodeExecutor, LuaManifestCodeResult,
};

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use stt_core::AppRules;

type EpochListener = Arc<dyn Fn(u64) + Send + Sync>;

/// 进程内配置与 rules 快照, 支持 epoch 订阅.
#[derive(Clone, Default)]
pub struct ConfigState {
    inner: Arc<Mutex<ConfigStateInner>>,
}

#[derive(Default)]
struct ConfigStateInner {
    host: HostConfig,
    tools: ToolRegistry,
    rules: AppRules,
    listeners: Vec<EpochListener>,
}

impl ConfigState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ConfigStateInner {
                host: HostConfig::default(),
                tools: ToolRegistry::with_defaults(),
                rules: AppRules::new(),
                listeners: Vec::new(),
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ConfigStateInner> {
        // 锁被毒化说明上次持有者 panic; 继续用现有数据.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn notify_epoch(listeners: &[EpochListener], epoch: u64) {
        for l in listeners {
            l(epoch);
        }
    }

    pub fn load_host_from_steam_root(&self, steam_root: &Path) -> Result<()> {
        let host = HostConfig::load_from_steam_root(steam_root)?;
        self.apply_host(host);
        Ok(())
    }

    pub fn apply_host(&self, host: HostConfig) {
        let mut g = self.lock();
        g.tools = ToolRegistry::from_host_tools(&host.tools.enabled);
        g.host = host;
    }

    pub fn host(&self) -> HostConfig {
        self.lock().host.clone()
    }

    pub fn tools(&self) -> ToolRegistry {
        self.lock().tools.clone()
    }

    pub fn rules_epoch(&self) -> u64 {
        self.lock().rules.epoch()
    }

    pub fn owned_count(&self) -> usize {
        self.lock().rules.owned_count()
    }

    pub fn with_rules<R>(&self, f: impl FnOnce(&AppRules) -> R) -> R {
        let g = self.lock();
        f(&g.rules)
    }

    pub fn with_rules_mut<R>(&self, f: impl FnOnce(&mut AppRules) -> R) -> R {
        let (out, epoch, listeners) = {
            let mut g = self.lock();
            let before = g.rules.epoch();
            let out = f(&mut g.rules);
            let after = g.rules.epoch();
            if after != before {
                (out, Some(after), g.listeners.clone())
            } else {
                (out, None, Vec::new())
            }
        };
        if let Some(e) = epoch {
            Self::notify_epoch(&listeners, e);
        }
        out
    }

    /// 整表替换 rules (lua 目录全量重载后用).
    pub fn replace_rules(&self, rules: AppRules) {
        let (epoch, listeners) = {
            let mut g = self.lock();
            let before = g.rules.epoch();
            let after = rules.epoch();
            g.rules = rules;
            if after != before {
                (Some(after), g.listeners.clone())
            } else {
                (None, Vec::new())
            }
        };
        if let Some(e) = epoch {
            Self::notify_epoch(&listeners, e);
        }
    }

    /// 订阅 AppRules 的 epoch 变化; 回调里尽量不要再进 ConfigState.
    pub fn subscribe_epoch(&self, listener: impl Fn(u64) + Send + Sync + 'static) {
        self.lock().listeners.push(Arc::new(listener));
    }

    #[cfg(feature = "lua")]
    pub fn apply_lua(&self, source: &str) -> Result<()> {
        self.with_rules_mut(|rules| apply_lua_chunk(rules, source))
    }

    /// 扫描 lua 目录并替换内存中的 rules.
    #[cfg(feature = "lua")]
    pub fn reload_lua_dirs(&self, steam_root: &Path) -> LuaLoadReport {
        let host = self.host();
        let (rules, report) = load_lua_directories(steam_root, &host);
        self.replace_rules(rules);
        report
    }

    pub fn default_lua_dir(steam_root: &Path) -> PathBuf {
        steam_root.join("config").join("lua")
    }

    pub fn lua_search_dirs(steam_root: &Path, host: &HostConfig) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = host.lua.paths.iter().map(PathBuf::from).collect();
        dirs.push(Self::default_lua_dir(steam_root));
        dirs
    }
}

/// 为已解析到的宿主 toml 建监视器 (没有文件则 None).
pub fn host_toml_watcher(steam_root: &Path, debounce: Duration) -> Option<DebouncedWatcher> {
    HostConfig::resolve_path(steam_root).map(|p| DebouncedWatcher::watch_file(p, debounce))
}

/// 枚举当前 lua 文件并建立防抖监视.
#[cfg(feature = "lua")]
pub fn lua_files_watcher(
    steam_root: &Path,
    host: &HostConfig,
    debounce: Duration,
) -> DebouncedWatcher {
    let dirs = ConfigState::lua_search_dirs(steam_root, host);
    let files = list_lua_files_in_dirs(&dirs);
    DebouncedWatcher::new(files, debounce)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn epoch_subscription_fires_on_rules_change() {
        let state = ConfigState::new();
        let seen = Arc::new(AtomicU64::new(0));
        let s2 = Arc::clone(&seen);
        state.subscribe_epoch(move |e| {
            s2.store(e, Ordering::SeqCst);
        });
        state.with_rules_mut(|r| r.add_app(1));
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn lua_apply_via_state() {
        let state = ConfigState::new();
        state
            .apply_lua("addappid(5)\naddtoken(5, \"42\")\n")
            .unwrap();
        state.with_rules(|r| {
            assert!(r.is_owned(5));
            assert_eq!(r.access_token(5), Some(42));
        });
    }

    #[cfg(feature = "lua")]
    #[test]
    fn reload_lua_dirs_replaces_rules() {
        let root = tempfile::tempdir().unwrap();
        let lua_dir = root.path().join("config").join("lua");
        std::fs::create_dir_all(&lua_dir).unwrap();
        std::fs::write(lua_dir.join("a.lua"), "addappid(7)\n").unwrap();

        let state = ConfigState::new();
        let report = state.reload_lua_dirs(root.path());
        assert_eq!(report.files_ok, 1);
        assert!(state.with_rules(|r| r.is_owned(7)));
        assert_eq!(state.owned_count(), 1);

        std::fs::write(lua_dir.join("a.lua"), "addappid(8)\n").unwrap();
        let report = state.reload_lua_dirs(root.path());
        assert_eq!(report.files_ok, 1);
        assert!(!state.with_rules(|r| r.is_owned(7)));
        assert!(state.with_rules(|r| r.is_owned(8)));
    }
}
