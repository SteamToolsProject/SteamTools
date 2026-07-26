//! 配置页发回来的改动意图.
//!
//! 页面只发这些定好的意图, 值全部在这里校验; 页面永远不能直接决定写什么进 toml.
//! 落盘用定点改而不是整份重写 — 用户是手写这个文件的, 注释和键序不该被吃掉.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use toml_edit::{Array, DocumentMut, Item, Table, Value};

use crate::error::{ConfigError, Result};
use crate::host_toml::{HostConfig, HOST_TOML_NAME};
use crate::tools::ToolId;
use crate::ConfigState;

/// 允许的日志级别.
pub const LOG_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

/// 允许的上游源 id (真实实现还没接, 先只认这几个名字).
pub const MANIFEST_SOURCES: &[&str] = &["opensteamtool", "steamrun", "wudrm"];

/// 额外 lua 目录的条数上限.
const MAX_LUA_PATHS: usize = 8;

/// 单条路径的长度上限.
const MAX_PATH_LEN: usize = 260;

/// 页面能提出的改动; 构造函数就是白名单.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigIntent {
    SetTool {
        id: ToolId,
        on: bool,
    },
    SetLogLevel(String),
    SetManifestUrl(String),
    AddLuaPath(String),
    RemoveLuaPath(String),
    /// 重新拉一次清单并覆盖那个 app 的 lua.
    RefreshApp(u32),
    /// 撤掉入库: 删我们写的那个 lua.
    RemoveApp(u32),
}

impl ConfigIntent {
    pub fn set_tool(id: &str, on: bool) -> Option<Self> {
        ToolId::parse(id).map(|id| Self::SetTool { id, on })
    }

    pub fn set_log_level(level: &str) -> Option<Self> {
        LOG_LEVELS
            .contains(&level)
            .then(|| Self::SetLogLevel(level.to_owned()))
    }

    pub fn set_manifest_url(source: &str) -> Option<Self> {
        MANIFEST_SOURCES
            .contains(&source)
            .then(|| Self::SetManifestUrl(source.to_owned()))
    }

    pub fn add_lua_path(path: &str) -> Option<Self> {
        sane_path(path).map(Self::AddLuaPath)
    }

    pub fn remove_lua_path(path: &str) -> Option<Self> {
        sane_path(path).map(Self::RemoveLuaPath)
    }

    pub fn refresh_app(app_id: u32) -> Option<Self> {
        (app_id > 0).then_some(Self::RefreshApp(app_id))
    }

    pub fn remove_app(app_id: u32) -> Option<Self> {
        (app_id > 0).then_some(Self::RemoveApp(app_id))
    }

    /// 针对某个 app 的意图不改 toml, 由宿主拿 provider 去执行.
    pub fn app_target(&self) -> Option<u32> {
        match self {
            Self::RefreshApp(id) | Self::RemoveApp(id) => Some(*id),
            _ => None,
        }
    }

    /// 改一份配置副本; 只有这里通过了才会落盘.
    fn apply_to(&self, host: &mut HostConfig) -> Result<String> {
        match self {
            Self::SetTool { id, on } => {
                host.tools.enabled.insert(id.as_str().to_owned(), *on);
                Ok(format!("{}={on}", id.as_str()))
            }
            Self::SetLogLevel(level) => {
                host.log.level = level.clone();
                Ok(format!("log.level={level}"))
            }
            Self::SetManifestUrl(source) => {
                host.manifest.url = source.clone();
                Ok(format!("manifest.url={source}"))
            }
            Self::AddLuaPath(path) => {
                if !Path::new(path).is_dir() {
                    return Err(ConfigError::Invalid(format!("目录不存在: {path}")));
                }
                if host.lua.paths.iter().any(|p| p == path) {
                    return Err(ConfigError::Invalid(format!("目录已在列表里: {path}")));
                }
                if host.lua.paths.len() >= MAX_LUA_PATHS {
                    return Err(ConfigError::Invalid(format!(
                        "最多 {MAX_LUA_PATHS} 个额外目录"
                    )));
                }
                host.lua.paths.push(path.clone());
                Ok(format!("lua.paths += {path}"))
            }
            Self::RemoveLuaPath(path) => {
                let before = host.lua.paths.len();
                host.lua.paths.retain(|p| p != path);
                if host.lua.paths.len() == before {
                    return Err(ConfigError::Invalid(format!("列表里没有: {path}")));
                }
                Ok(format!("lua.paths -= {path}"))
            }
            // 这两个不动 toml, 由宿主接手 (它才有 provider).
            Self::RefreshApp(_) | Self::RemoveApp(_) => Err(ConfigError::Invalid(
                "app 意图不写 toml, 应由宿主处理".into(),
            )),
        }
    }
}

/// 路径的基本体检; 目录存不存在留到落盘前查.
fn sane_path(path: &str) -> Option<String> {
    let path = path.trim();
    if path.is_empty() || path.len() > MAX_PATH_LEN {
        return None;
    }
    if path.contains(['\0', '\n', '\r']) {
        return None;
    }
    Some(path.to_owned())
}

/// 校验 → 写盘 → 进内存. 写盘失败就不改内存, 免得两边不一致.
///
/// 写完文件监视会再重载一次同样的内容, 那是幂等的, 顺带当确认.
pub fn apply_intent(
    state: &ConfigState,
    steam_root: &Path,
    intent: &ConfigIntent,
) -> Result<String> {
    debug_assert!(
        intent.app_target().is_none(),
        "app 意图该走宿主, 不该进 apply_intent"
    );
    let mut host = state.host();
    let note = intent.apply_to(&mut host)?;
    save_host_change(steam_root, &host, intent)?;
    state.apply_host(host);
    Ok(note)
}

/// 宿主 toml 的落盘路径: 有旧文件就改旧的, 否则用新名字.
pub fn host_toml_write_path(steam_root: &Path) -> PathBuf {
    HostConfig::resolve_path(steam_root).unwrap_or_else(|| steam_root.join(HOST_TOML_NAME))
}

/// 定点改一个键并原子替换文件.
pub fn save_host_change(
    steam_root: &Path,
    host: &HostConfig,
    intent: &ConfigIntent,
) -> Result<PathBuf> {
    let path = host_toml_write_path(steam_root);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e| ConfigError::Invalid(format!("{} 解析失败: {e}", path.display())))?;
    match intent {
        ConfigIntent::SetTool { id, on } => {
            let table = table_at(&mut doc, &["tools", "enabled"])?;
            table[id.as_str()] = toml_edit::value(*on);
        }
        ConfigIntent::SetLogLevel(level) => {
            let table = table_at(&mut doc, &["log"])?;
            table["level"] = toml_edit::value(level.as_str());
        }
        ConfigIntent::SetManifestUrl(source) => {
            let table = table_at(&mut doc, &["manifest"])?;
            table["url"] = toml_edit::value(source.as_str());
        }
        // app 意图不动 toml, 走不到这儿 (apply_intent 已经挡住).
        ConfigIntent::RefreshApp(_) | ConfigIntent::RemoveApp(_) => {
            return Err(ConfigError::Invalid("app 意图不写 toml".into()))
        }
        ConfigIntent::AddLuaPath(_) | ConfigIntent::RemoveLuaPath(_) => {
            let mut array = Array::new();
            for p in &host.lua.paths {
                array.push(p.as_str());
            }
            let table = table_at(&mut doc, &["lua"])?;
            table["paths"] = Item::Value(Value::Array(array));
        }
    }
    write_atomic(&path, &doc.to_string())?;
    Ok(path)
}

/// 沿路径取表, 缺的段补上. 中间段建成隐式表, 免得多出一行空 `[tools]`.
fn table_at<'a>(doc: &'a mut DocumentMut, path: &[&str]) -> Result<&'a mut Table> {
    let mut table = doc.as_table_mut();
    for key in path {
        let entry = table.entry(key).or_insert({
            let mut fresh = Table::new();
            fresh.set_implicit(true);
            Item::Table(fresh)
        });
        table = entry
            .as_table_mut()
            .ok_or_else(|| ConfigError::Invalid(format!("{key} 不是一个表")))?;
    }
    // 叶子表要写进键值, 不能是隐式的.
    table.set_implicit(false);
    Ok(table)
}

/// 先写同目录临时文件再改名: 中途断电也不会留下半份配置.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let mut name: OsString = path.file_name().unwrap_or_default().to_owned();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    std::fs::write(&tmp, text).map_err(|source| ConfigError::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| {
        let _ = std::fs::remove_file(&tmp);
        ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_tool_takes_known_ids_only() {
        assert!(ConfigIntent::set_tool("library_ux", false).is_some());
        assert!(ConfigIntent::set_tool("rm -rf", false).is_none());
    }

    #[test]
    fn set_log_level_takes_known_levels_only() {
        assert!(ConfigIntent::set_log_level("info").is_some());
        assert!(ConfigIntent::set_log_level("verbose").is_none());
    }

    /// 页面不能自己指定一个上游地址 —— 只认这几个源的名字.
    #[test]
    fn set_manifest_url_refuses_arbitrary_urls() {
        assert!(ConfigIntent::set_manifest_url("wudrm").is_some());
        assert!(ConfigIntent::set_manifest_url("http://evil.test/x").is_none());
    }

    #[test]
    fn paths_are_trimmed_and_bounded() {
        assert_eq!(
            ConfigIntent::add_lua_path("  D:/lua  "),
            Some(ConfigIntent::AddLuaPath("D:/lua".into()))
        );
        assert!(ConfigIntent::add_lua_path("").is_none());
        assert!(ConfigIntent::add_lua_path("D:/lua\nmore").is_none());
        assert!(ConfigIntent::add_lua_path(&"x".repeat(MAX_PATH_LEN + 1)).is_none());
    }

    #[test]
    fn toggling_a_tool_keeps_comments_and_other_keys() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(HOST_TOML_NAME);
        std::fs::write(
            &path,
            "# 我的配置\n[log]\nlevel = \"debug\" # 别动\n\n[tools.enabled]\ncatalog_add = true\n",
        )
        .unwrap();

        let state = ConfigState::new();
        state.load_host_from_steam_root(root.path()).unwrap();
        let intent = ConfigIntent::set_tool("library_ux", false).unwrap();
        apply_intent(&state, root.path(), &intent).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# 我的配置"), "{text}");
        assert!(text.contains("level = \"debug\" # 别动"), "{text}");
        assert!(text.contains("catalog_add = true"), "{text}");
        assert!(text.contains("library_ux = false"), "{text}");
        assert!(!state.tools().is_enabled(ToolId::LibraryUx));
    }

    #[test]
    fn writing_creates_the_file_when_there_is_none() {
        let root = tempfile::tempdir().unwrap();
        let state = ConfigState::new();
        let intent = ConfigIntent::set_log_level("warn").unwrap();
        apply_intent(&state, root.path(), &intent).unwrap();

        let text = std::fs::read_to_string(root.path().join(HOST_TOML_NAME)).unwrap();
        assert!(text.contains("[log]"), "{text}");
        assert!(text.contains("level = \"warn\""), "{text}");
        // 没写过的段不该凭空多出来.
        assert!(!text.contains("[tools"), "{text}");
        assert_eq!(state.host().log.level, "warn");
    }

    #[test]
    fn tool_toggle_reaches_disk_and_reloads_the_same() {
        let root = tempfile::tempdir().unwrap();
        let state = ConfigState::new();
        apply_intent(
            &state,
            root.path(),
            &ConfigIntent::set_tool("store_accel", true).unwrap(),
        )
        .unwrap();

        let fresh = ConfigState::new();
        fresh.load_host_from_steam_root(root.path()).unwrap();
        assert!(fresh.tools().is_enabled(ToolId::StoreAccel));
    }

    #[test]
    fn lua_paths_round_trip_through_the_file() {
        let root = tempfile::tempdir().unwrap();
        let extra = root.path().join("extra");
        std::fs::create_dir_all(&extra).unwrap();
        let extra = extra.display().to_string();

        let state = ConfigState::new();
        apply_intent(
            &state,
            root.path(),
            &ConfigIntent::add_lua_path(&extra).unwrap(),
        )
        .unwrap();
        assert_eq!(state.host().lua.paths, vec![extra.clone()]);

        apply_intent(
            &state,
            root.path(),
            &ConfigIntent::remove_lua_path(&extra).unwrap(),
        )
        .unwrap();
        assert!(state.host().lua.paths.is_empty());

        let fresh = ConfigState::new();
        fresh.load_host_from_steam_root(root.path()).unwrap();
        assert!(fresh.host().lua.paths.is_empty());
    }

    #[test]
    fn a_missing_directory_is_refused_and_nothing_is_written() {
        let root = tempfile::tempdir().unwrap();
        let state = ConfigState::new();
        let intent = ConfigIntent::add_lua_path("D:/definitely/not/here").unwrap();
        assert!(apply_intent(&state, root.path(), &intent).is_err());
        assert!(!root.path().join(HOST_TOML_NAME).exists());
        assert!(state.host().lua.paths.is_empty());
    }

    #[test]
    fn a_broken_config_file_is_not_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(HOST_TOML_NAME);
        std::fs::write(&path, "this is not = = toml\n").unwrap();
        let state = ConfigState::new();
        let intent = ConfigIntent::set_log_level("info").unwrap();
        assert!(apply_intent(&state, root.path(), &intent).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "this is not = = toml\n"
        );
    }
}
