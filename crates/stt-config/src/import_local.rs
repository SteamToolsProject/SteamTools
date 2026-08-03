//! 本地 lua 包导入 (拖放 / 其它入口共用).
//!
//! 不走 CatalogProvider: 信任用户 lua 原文, 校验能解析后拷进 `config/lua`,
//! 旁路 `.manifest` 写入 depotcache. 生效靠既有 `reload_lua_dirs`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use stt_catalog::ManifestBlob;
use stt_core::AppId;

use crate::catalog_add::write_manifest_blobs;
use crate::error::{ConfigError, Result};
use crate::intent::write_atomic;
use crate::lua_dsl::eval_lua_to_bundle;
use crate::lua_load::default_lua_dir;
use crate::tools::ToolId;
use crate::ConfigState;

/// 单次导入最多收多少个 .lua.
const MAX_LUA_FILES: usize = 64;
/// 单个 .lua 大小上限.
const MAX_LUA_BYTES: u64 = 2 * 1024 * 1024;
/// 单个 .manifest 大小上限.
const MAX_MANIFEST_BYTES: u64 = 32 * 1024 * 1024;
/// 目录扫描深度 (0 = 只看这一层).
const MAX_DIR_DEPTH: usize = 2;

/// 一次本地导入的汇总.
#[derive(Debug, Clone, Default)]
pub struct ImportLocalReport {
    pub lua_written: Vec<PathBuf>,
    pub manifests_written: usize,
    /// 解析出的主 app (去重升序), 供 package notify.
    pub apps: Vec<AppId>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

impl ImportLocalReport {
    pub fn summary_line(&self) -> String {
        let apps = if self.apps.is_empty() {
            "-".to_owned()
        } else {
            self.apps
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        };
        format!(
            "import_local lua={} manifest={} apps={} skip={} err={}",
            self.lua_written.len(),
            self.manifests_written,
            apps,
            self.skipped.len(),
            self.errors.len()
        )
    }

    pub fn note_line(&self) -> String {
        if !self.errors.is_empty() && self.lua_written.is_empty() {
            return format!(
                "失败: {}",
                humanize_import_error(self.errors.first().map(String::as_str).unwrap_or(""))
            );
        }
        if self.lua_written.is_empty() && self.manifests_written == 0 {
            let reason = self
                .skipped
                .first()
                .cloned()
                .unwrap_or_else(|| "没有可识别的 lua".to_owned());
            return format!("失败: {}", humanize_import_error(&reason));
        }
        let mut parts = Vec::new();
        if !self.lua_written.is_empty() {
            parts.push(format!("{} 个 lua", self.lua_written.len()));
        }
        if self.manifests_written > 0 {
            parts.push(format!("{} 个 manifest", self.manifests_written));
        }
        if !self.apps.is_empty() {
            parts.push(format!(
                "app {}",
                self.apps
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        let mut note = format!("已导入 {}", parts.join(" · "));
        if !self.errors.is_empty() {
            note.push_str(&format!(" (部分失败 {})", self.errors.len()));
        }
        note
    }
}

/// 把用户拖入/选中的路径导入到 Steam 配置目录.
///
/// `paths` 可以是 `.lua` 文件、含 lua 的文件夹、旁路 `.manifest`.
#[cfg(feature = "lua")]
pub fn import_local_paths(
    state: &ConfigState,
    steam_root: &Path,
    paths: &[PathBuf],
) -> Result<ImportLocalReport> {
    if !state.tools().is_enabled(ToolId::LuaDrop) {
        return Err(ConfigError::Invalid("lua_drop tool is disabled".into()));
    }
    if paths.is_empty() {
        return Err(ConfigError::Invalid("没有可导入的路径".into()));
    }

    let mut report = ImportLocalReport::default();
    let mut lua_sources: Vec<PathBuf> = Vec::new();
    let mut manifest_sources: Vec<PathBuf> = Vec::new();
    let mut seen = BTreeSet::new();

    for raw in paths {
        let path = match raw.canonicalize() {
            Ok(p) => p,
            Err(_) => raw.clone(),
        };
        collect_from_path(
            &path,
            0,
            &mut lua_sources,
            &mut manifest_sources,
            &mut seen,
            &mut report,
        );
    }

    if lua_sources.len() > MAX_LUA_FILES {
        report.errors.push(format!(
            "一次最多导入 {MAX_LUA_FILES} 个 lua, 当前 {}",
            lua_sources.len()
        ));
        lua_sources.truncate(MAX_LUA_FILES);
    }

    let lua_dir = default_lua_dir(steam_root);
    std::fs::create_dir_all(&lua_dir).map_err(|source| ConfigError::Io {
        path: lua_dir.clone(),
        source,
    })?;

    let mut app_set = BTreeSet::new();

    for src in &lua_sources {
        match import_one_lua(&lua_dir, src) {
            Ok((dest, apps)) => {
                report.lua_written.push(dest);
                for id in apps {
                    if id != 0 {
                        app_set.insert(id);
                    }
                }
            }
            Err(e) => report.errors.push(format!("{}: {e}", src.display())),
        }
    }

    let blobs = load_manifest_blobs(&manifest_sources, &mut report);
    if !blobs.is_empty() {
        match write_manifest_blobs(steam_root, &blobs) {
            Ok(n) => report.manifests_written = n,
            Err(e) => report.errors.push(format!("manifest: {e}")),
        }
    }

    if report.lua_written.is_empty() && report.manifests_written == 0 {
        // 全失败也返回 Ok(report), 让宿主用 note 展示; 只有工具关闭等才 Err.
        report.apps = app_set.into_iter().collect();
        return Ok(report);
    }

    // 以磁盘为准重载, 与 catalog_add 一致.
    let _ = state.reload_lua_dirs(steam_root);
    report.apps = app_set.into_iter().collect();
    Ok(report)
}

fn collect_from_path(
    path: &Path,
    depth: usize,
    luas: &mut Vec<PathBuf>,
    manifests: &mut Vec<PathBuf>,
    seen: &mut BTreeSet<PathBuf>,
    report: &mut ImportLocalReport,
) {
    if !seen.insert(path.to_path_buf()) {
        return;
    }
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => {
            report.skipped.push(format!("无法访问 {}", path.display()));
            return;
        }
    };
    if meta.file_type().is_symlink() {
        report
            .skipped
            .push(format!("跳过符号链接 {}", path.display()));
        return;
    }
    if meta.is_file() {
        if is_lua_path(path) {
            if meta.len() > MAX_LUA_BYTES {
                report
                    .skipped
                    .push(format!("lua 过大 ({} B) {}", meta.len(), path.display()));
                return;
            }
            luas.push(path.to_path_buf());
        } else if is_manifest_path(path) {
            if meta.len() > MAX_MANIFEST_BYTES {
                report.skipped.push(format!(
                    "manifest 过大 ({} B) {}",
                    meta.len(),
                    path.display()
                ));
                return;
            }
            manifests.push(path.to_path_buf());
        } else {
            report
                .skipped
                .push(format!("不是 lua/manifest: {}", path.display()));
        }
        return;
    }
    if meta.is_dir() {
        if depth > MAX_DIR_DEPTH {
            report.skipped.push(format!("目录过深: {}", path.display()));
            return;
        }
        let rd = match std::fs::read_dir(path) {
            Ok(rd) => rd,
            Err(_) => {
                report
                    .skipped
                    .push(format!("无法读目录 {}", path.display()));
                return;
            }
        };
        for ent in rd.flatten() {
            collect_from_path(&ent.path(), depth + 1, luas, manifests, seen, report);
        }
        return;
    }
    report.skipped.push(format!("无法识别 {}", path.display()));
}

fn import_one_lua(lua_dir: &Path, src: &Path) -> Result<(PathBuf, Vec<AppId>)> {
    let text = std::fs::read_to_string(src).map_err(|source| ConfigError::Io {
        path: src.to_path_buf(),
        source,
    })?;
    import_one_lua_text(lua_dir, src, &text)
}

/// 面板拖入走内容: CEF 常给不出绝对路径, 只给 File 文本.
#[cfg(feature = "lua")]
pub fn import_local_texts(
    state: &ConfigState,
    steam_root: &Path,
    files: &[(String, String)],
) -> Result<ImportLocalReport> {
    if !state.tools().is_enabled(ToolId::LuaDrop) {
        return Err(ConfigError::Invalid("lua_drop tool is disabled".into()));
    }
    if files.is_empty() {
        return Err(ConfigError::Invalid("没有可导入的文件".into()));
    }

    let mut report = ImportLocalReport::default();
    let lua_dir = default_lua_dir(steam_root);
    std::fs::create_dir_all(&lua_dir).map_err(|source| ConfigError::Io {
        path: lua_dir.clone(),
        source,
    })?;

    let mut app_set = BTreeSet::new();
    let mut count = 0usize;
    for (name, text) in files {
        if count >= MAX_LUA_FILES {
            report
                .errors
                .push(format!("一次最多导入 {MAX_LUA_FILES} 个 lua"));
            break;
        }
        let Some(safe_name) = sanitize_upload_name(name) else {
            report.skipped.push(format!("文件名无效: {name}"));
            continue;
        };
        if text.len() as u64 > MAX_LUA_BYTES {
            report
                .skipped
                .push(format!("lua 过大 ({} B) {safe_name}", text.len()));
            continue;
        }
        let pseudo = PathBuf::from(&safe_name);
        match import_one_lua_text(&lua_dir, &pseudo, text) {
            Ok((dest, apps)) => {
                report.lua_written.push(dest);
                for id in apps {
                    if id != 0 {
                        app_set.insert(id);
                    }
                }
                count += 1;
            }
            Err(e) => report.errors.push(format!("{safe_name}: {e}")),
        }
    }

    if report.lua_written.is_empty() {
        report.apps = app_set.into_iter().collect();
        return Ok(report);
    }
    let _ = state.reload_lua_dirs(steam_root);
    report.apps = app_set.into_iter().collect();
    Ok(report)
}

fn import_one_lua_text(lua_dir: &Path, src: &Path, text: &str) -> Result<(PathBuf, Vec<AppId>)> {
    if text.trim().is_empty() {
        return Err(ConfigError::Invalid("空文件".into()));
    }
    let bundle = eval_lua_to_bundle(text)?;
    let has_signal = !bundle.apps.is_empty()
        || !bundle.depot_keys.is_empty()
        || !bundle.manifests.is_empty()
        || !bundle.access_tokens.is_empty();
    if !has_signal {
        return Err(ConfigError::Invalid(
            "未识别到 addappid/addtoken/setmanifestid".into(),
        ));
    }

    let primary = primary_app_id(src, &bundle.apps);
    let dest = allocate_dest_path(lua_dir, src, primary)?;
    // 保留用户原文, 不重写成 catalog 格式.
    write_atomic(&dest, text)?;
    let mut apps = bundle.apps;
    apps.sort_unstable();
    apps.dedup();
    Ok((dest, apps))
}

/// 面板上传文件名: 只要叶子名, 必须是 .lua, 去掉路径分隔.
fn sanitize_upload_name(name: &str) -> Option<String> {
    let leaf = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    if leaf.is_empty() || leaf.len() > 180 {
        return None;
    }
    if leaf.contains("..") || leaf.contains(['\0', '\n', '\r']) {
        return None;
    }
    let path = Path::new(leaf);
    let is_lua = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("lua"));
    if !is_lua {
        return None;
    }
    // 文件名只留安全字符.
    let safe: String = leaf
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if !safe.to_ascii_lowercase().ends_with(".lua") {
        return None;
    }
    Some(safe)
}

fn primary_app_id(src: &Path, apps: &[AppId]) -> Option<AppId> {
    if let Some(id) = app_id_from_filename(src) {
        return Some(id);
    }
    let mut ids: Vec<AppId> = apps.iter().copied().filter(|&id| id != 0).collect();
    ids.sort_unstable();
    ids.dedup();
    ids.into_iter().next()
}

fn app_id_from_filename(src: &Path) -> Option<AppId> {
    let stem = src.file_stem()?.to_str()?;
    let stem = stem
        .strip_prefix("stt_")
        .or_else(|| stem.strip_prefix("import_"))
        .unwrap_or(stem);
    if stem.is_empty() || !stem.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok().filter(|id: &AppId| *id != 0)
}

/// 落盘名: 原名空闲就用原名; 否则 stt_{id} / import_{id} / import_{id}_{n}.
fn allocate_dest_path(lua_dir: &Path, src: &Path, primary: Option<AppId>) -> Result<PathBuf> {
    let original = src
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("import.lua"));
    let original_dest = lua_dir.join(&original);
    // 同源覆盖 (同一路径) 允许; 不同源占坑则换名.
    if !original_dest.exists() {
        return Ok(original_dest);
    }
    if same_file(&original_dest, src) {
        return Ok(original_dest);
    }

    if let Some(id) = primary {
        let stt = lua_dir.join(format!("stt_{id}.lua"));
        if !stt.exists() {
            return Ok(stt);
        }
        let import = lua_dir.join(format!("import_{id}.lua"));
        if !import.exists() || same_file(&import, src) {
            return Ok(import);
        }
        for n in 2..100 {
            let alt = lua_dir.join(format!("import_{id}_{n}.lua"));
            if !alt.exists() {
                return Ok(alt);
            }
        }
    }

    let stem = original
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("import");
    for n in 1..100 {
        let alt = lua_dir.join(format!("import_{stem}_{n}.lua"));
        if !alt.exists() {
            return Ok(alt);
        }
    }
    Err(ConfigError::Invalid(format!(
        "无法为 {} 分配落盘名",
        src.display()
    )))
}

fn same_file(a: &Path, b: &Path) -> bool {
    let (Ok(ca), Ok(cb)) = (a.canonicalize(), b.canonicalize()) else {
        return false;
    };
    ca == cb
}

fn load_manifest_blobs(paths: &[PathBuf], report: &mut ImportLocalReport) -> Vec<ManifestBlob> {
    let mut out = Vec::new();
    for path in paths {
        match parse_manifest_file(path) {
            Ok(blob) => out.push(blob),
            Err(e) => report.skipped.push(format!("{}: {e}", path.display())),
        }
    }
    out
}

fn parse_manifest_file(path: &Path) -> Result<ManifestBlob> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| ConfigError::Invalid("manifest 文件名无效".into()))?;
    let stem = name
        .strip_suffix(".manifest")
        .or_else(|| name.strip_suffix(".Manifest"))
        .ok_or_else(|| ConfigError::Invalid("不是 .manifest".into()))?;
    let (depot_s, gid_s) = stem
        .split_once('_')
        .ok_or_else(|| ConfigError::Invalid("文件名应为 {depot}_{gid}.manifest".into()))?;
    let depot_id: u32 = depot_s
        .parse()
        .map_err(|_| ConfigError::Invalid(format!("depot id 无效: {depot_s}")))?;
    let manifest_gid: u64 = gid_s
        .parse()
        .map_err(|_| ConfigError::Invalid(format!("manifest gid 无效: {gid_s}")))?;
    if depot_id == 0 || manifest_gid == 0 {
        return Err(ConfigError::Invalid("depot/gid 不能为 0".into()));
    }
    let bytes = std::fs::read(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if bytes.is_empty() {
        return Err(ConfigError::Invalid("空 manifest".into()));
    }
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(ConfigError::Invalid("manifest 过大".into()));
    }
    Ok(ManifestBlob {
        depot_id,
        manifest_gid,
        bytes,
    })
}

/// 面板 note 不宜塞整段 mlua 堆栈; 抽出可读原因.
fn humanize_import_error(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return "本地导入失败".to_owned();
    }
    // "3167020.lua: lua error: ..." → 文件名 + 精简原因
    let (file, rest) = match raw.split_once(':') {
        Some((name, rest)) if name.trim().ends_with(".lua") => (Some(name.trim()), rest.trim()),
        _ => (None, raw),
    };
    let reason = if rest.contains("attempt to call a nil value") {
        if let Some(func) = rest
            .split("global '")
            .nth(1)
            .and_then(|s| s.split('\'').next())
        {
            format!("未知函数 {func}")
        } else {
            "脚本调用了未支持的函数".to_owned()
        }
    } else if rest.contains("未识别到") {
        "未识别到 addappid/addtoken/setmanifestid".to_owned()
    } else if rest.contains("空文件") {
        "空文件".to_owned()
    } else if let Some(idx) = rest.find("runtime error:") {
        // 截断堆栈
        let body = rest[idx + "runtime error:".len()..].trim();
        let first = body.lines().next().unwrap_or(body).trim();
        // 去掉内部路径噪声
        if let Some(msg) = first.rsplit("]: ").next() {
            msg.to_owned()
        } else {
            first.chars().take(120).collect()
        }
    } else {
        rest.chars().take(140).collect()
    };
    match file {
        Some(name) => format!("{name}: {reason}"),
        None => reason,
    }
}

fn is_lua_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("lua"))
}

fn is_manifest_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("manifest"))
}

/// 默认目录里属于某个 app 的受管文件 (stt_ / import_ / 纯数字名).
pub fn managed_lua_paths_for_app(steam_root: &Path, app_id: AppId) -> Vec<PathBuf> {
    let dir = default_lua_dir(steam_root);
    let mut out = Vec::new();
    for name in [
        format!("stt_{app_id}.lua"),
        format!("import_{app_id}.lua"),
        format!("{app_id}.lua"),
    ] {
        let p = dir.join(name);
        if p.is_file() {
            out.push(p);
        }
    }
    // import_{id}_{n}.lua
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let prefix = format!("import_{app_id}_");
        for ent in rd.flatten() {
            let path = ent.path();
            let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if fname.starts_with(&prefix)
                && fname.ends_with(".lua")
                && path.is_file()
                && !out.contains(&path)
            {
                out.push(path);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_toml::HostConfig;

    fn enable_lua_drop(state: &ConfigState) {
        let mut host = HostConfig::default();
        host.tools
            .enabled
            .insert(ToolId::LuaDrop.as_str().to_owned(), true);
        // catalog_add 等保持默认.
        state.apply_host(host);
    }

    #[test]
    fn imports_single_lua_preserving_body() {
        let root = tempfile::tempdir().unwrap();
        let state = ConfigState::new();
        enable_lua_drop(&state);

        let src_dir = root.path().join("pack");
        std::fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join("570.lua");
        let body = "addappid(570)\naddappid(571, 0, \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\")\n";
        std::fs::write(&src, body).unwrap();

        let report = import_local_paths(&state, root.path(), std::slice::from_ref(&src)).unwrap();
        assert_eq!(report.lua_written.len(), 1, "{report:?}");
        assert!(report.apps.contains(&570), "{report:?}");
        let written = std::fs::read_to_string(&report.lua_written[0]).unwrap();
        assert_eq!(written, body);
        assert!(state.with_rules(|r| r.is_owned(570)));
    }

    #[test]
    fn folder_with_manifest_writes_depotcache() {
        let root = tempfile::tempdir().unwrap();
        let state = ConfigState::new();
        enable_lua_drop(&state);

        let pack = root.path().join("game");
        std::fs::create_dir_all(&pack).unwrap();
        std::fs::write(
            pack.join("892970.lua"),
            "addappid(892970)\nsetmanifestid(892971, \"123\")\n",
        )
        .unwrap();
        std::fs::write(pack.join("892971_123.manifest"), b"manifest-bytes").unwrap();

        let report = import_local_paths(&state, root.path(), &[pack]).unwrap();
        assert_eq!(report.lua_written.len(), 1);
        assert_eq!(report.manifests_written, 1);
        let primary = root.path().join("depotcache").join("892971_123.manifest");
        assert_eq!(std::fs::read(primary).unwrap(), b"manifest-bytes");
    }

    #[test]
    fn rejects_non_lua_noise() {
        let root = tempfile::tempdir().unwrap();
        let state = ConfigState::new();
        enable_lua_drop(&state);
        let txt = root.path().join("note.txt");
        std::fs::write(&txt, "hello").unwrap();
        let report = import_local_paths(&state, root.path(), &[txt]).unwrap();
        assert!(report.lua_written.is_empty());
        assert!(!report.skipped.is_empty() || !report.errors.is_empty());
    }

    #[test]
    fn disabled_tool_errors() {
        let root = tempfile::tempdir().unwrap();
        let state = ConfigState::new();
        let mut host = HostConfig::default();
        host.tools
            .enabled
            .insert(ToolId::LuaDrop.as_str().to_owned(), false);
        state.apply_host(host);
        let src = root.path().join("1.lua");
        std::fs::write(&src, "addappid(1)\n").unwrap();
        let err = import_local_paths(&state, root.path(), &[src]).unwrap_err();
        assert!(err.to_string().contains("disabled"), "{err}");
    }

    #[test]
    fn imports_panel_text_payload() {
        let root = tempfile::tempdir().unwrap();
        let state = ConfigState::new();
        enable_lua_drop(&state);
        let files = vec![("570.lua".to_owned(), "addappid(570)\n".to_owned())];
        let report = import_local_texts(&state, root.path(), &files).unwrap();
        assert_eq!(report.lua_written.len(), 1, "{report:?}");
        assert!(report.apps.contains(&570), "{report:?}");
        assert!(state.with_rules(|r| r.is_owned(570)));
    }

    #[test]
    fn collision_uses_import_name() {
        let root = tempfile::tempdir().unwrap();
        let state = ConfigState::new();
        enable_lua_drop(&state);
        let lua_dir = default_lua_dir(root.path());
        std::fs::create_dir_all(&lua_dir).unwrap();
        std::fs::write(lua_dir.join("730.lua"), "addappid(730)\n").unwrap();

        let src = root.path().join("other").join("730.lua");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, "addappid(730)\naddtoken(730, \"1\")\n").unwrap();

        let report = import_local_paths(&state, root.path(), &[src]).unwrap();
        assert_eq!(report.lua_written.len(), 1);
        let name = report.lua_written[0].file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("stt_730") || name.starts_with("import_730"),
            "got {name}"
        );
    }
}
