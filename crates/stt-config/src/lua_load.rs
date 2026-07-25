//! Scan lua directories and apply scripts into AppRules.

use std::path::{Path, PathBuf};

use stt_core::AppRules;

use crate::error::{ConfigError, Result};
use crate::host_toml::HostConfig;
use crate::lua_dsl::apply_lua_chunk;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LuaLoadReport {
    pub dirs_scanned: usize,
    pub files_ok: usize,
    pub files_err: usize,
    pub errors: Vec<String>,
}

/// Default `<Steam>/config/lua`.
pub fn default_lua_dir(steam_root: &Path) -> PathBuf {
    steam_root.join("config").join("lua")
}

/// Extra `[lua].paths` first, then the default dir last (user files win on merge).
pub fn lua_search_dirs(steam_root: &Path, host: &HostConfig) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = host.lua.paths.iter().map(PathBuf::from).collect();
    dirs.push(default_lua_dir(steam_root));
    dirs
}

/// Sorted `.lua` files directly under `dir` (non-recursive, matches common layout).
pub fn list_lua_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    for ent in rd.flatten() {
        let path = ent.path();
        if !path.is_file() {
            continue;
        }
        let is_lua = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("lua"));
        if is_lua {
            out.push(path);
        }
    }
    out.sort();
    out
}

pub fn list_lua_files_in_dirs(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut all = Vec::new();
    for d in dirs {
        all.extend(list_lua_files(d));
    }
    all
}

/// Apply every `.lua` file under the search dirs into a **fresh** `AppRules`.
pub fn load_lua_directories(steam_root: &Path, host: &HostConfig) -> (AppRules, LuaLoadReport) {
    let dirs = lua_search_dirs(steam_root, host);
    let mut rules = AppRules::new();
    let mut report = LuaLoadReport {
        dirs_scanned: dirs.len(),
        ..Default::default()
    };

    for dir in &dirs {
        for path in list_lua_files(dir) {
            match load_one_lua_file(&mut rules, &path) {
                Ok(()) => report.files_ok += 1,
                Err(e) => {
                    report.files_err += 1;
                    report.errors.push(format!("{}: {e}", path.display()));
                }
            }
        }
    }

    (rules, report)
}

fn load_one_lua_file(rules: &mut AppRules, path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    apply_lua_chunk(rules, &text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn loads_sorted_lua_from_default_dir() {
        let root = tempfile::tempdir().unwrap();
        let lua_dir = root.path().join("config").join("lua");
        fs::create_dir_all(&lua_dir).unwrap();
        fs::write(lua_dir.join("b.lua"), "addappid(2)\n").unwrap();
        fs::write(lua_dir.join("a.lua"), "addappid(1)\naddtoken(1, \"9\")\n").unwrap();
        fs::write(lua_dir.join("skip.txt"), "nope\n").unwrap();

        let host = HostConfig::default();
        let (rules, report) = load_lua_directories(root.path(), &host);
        assert_eq!(report.files_ok, 2);
        assert_eq!(report.files_err, 0);
        assert!(rules.is_owned(1));
        assert!(rules.is_owned(2));
        assert_eq!(rules.access_token(1), Some(9));
        assert!(rules.epoch() > 0);
    }

    #[test]
    fn extra_paths_then_default() {
        let root = tempfile::tempdir().unwrap();
        let extra = root.path().join("extra");
        let def = root.path().join("config").join("lua");
        fs::create_dir_all(&extra).unwrap();
        fs::create_dir_all(&def).unwrap();
        // default last wins for same depot manifest
        fs::write(extra.join("x.lua"), "setmanifestid(10, \"1\")\n").unwrap();
        fs::write(def.join("y.lua"), "setmanifestid(10, \"2\")\n").unwrap();

        let mut host = HostConfig::default();
        host.lua.paths.push(extra.to_string_lossy().into_owned());
        let (rules, report) = load_lua_directories(root.path(), &host);
        assert_eq!(report.files_ok, 2);
        assert_eq!(
            rules.manifest_override(10).map(|m| m.manifest_gid),
            Some(2)
        );
    }
}
