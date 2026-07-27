//! Lua DSL -> CatalogBundle / AppRules (mlua).

use std::sync::{Arc, Mutex, MutexGuard};

use mlua::{Lua, Value};
use stt_core::{AppRules, CatalogBundle, ManifestOverride};

use crate::error::{ConfigError, Result};

fn lock_bundle(b: &Mutex<CatalogBundle>) -> MutexGuard<'_, CatalogBundle> {
    b.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 执行一段 Lua 配置并合并进 `rules`.
///
/// 兼容面 (小写注册):
/// - `addappid(id [, unused, key64hex])`
/// - `addtoken(appId, tokenDecimalString)`
/// - `setmanifestid(depotId, gidString [, size])`
pub fn apply_lua_chunk(rules: &mut AppRules, source: &str) -> Result<()> {
    let bundle = eval_lua_to_bundle(source)?;
    rules.apply_catalog_bundle(&bundle);
    Ok(())
}

pub fn eval_lua_to_bundle(source: &str) -> Result<CatalogBundle> {
    let lua = Lua::new();
    let bundle = Arc::new(Mutex::new(CatalogBundle::default()));

    {
        let b = Arc::clone(&bundle);
        let f = lua
            .create_function(
                move |_, (id, _unused, key): (u32, Option<Value>, Option<String>)| {
                    let mut g = lock_bundle(&b);
                    if !g.apps.contains(&id) {
                        g.apps.push(id);
                    }
                    let depots = g.app_depots.entry(id).or_default();
                    if !depots.contains(&id) {
                        depots.push(id);
                    }
                    if let Some(k) = key {
                        if k.len() == 64 && k.chars().all(|c| c.is_ascii_hexdigit()) {
                            g.depot_keys.insert(id, k);
                        }
                    }
                    Ok(())
                },
            )
            .map_err(lua_err)?;
        lua.globals().set("addappid", f).map_err(lua_err)?;
    }

    {
        let b = Arc::clone(&bundle);
        let f = lua
            .create_function(move |_, (app_id, token_s): (u32, String)| {
                let token: u64 = token_s.parse().map_err(|e| {
                    mlua::Error::external(format!("addtoken: invalid token '{token_s}': {e}"))
                })?;
                lock_bundle(&b).access_tokens.insert(app_id, token);
                Ok(())
            })
            .map_err(lua_err)?;
        lua.globals().set("addtoken", f).map_err(lua_err)?;
    }

    {
        let b = Arc::clone(&bundle);
        let f = lua
            .create_function(
                move |_, (depot_id, gid_s, size): (u32, String, Option<u64>)| {
                    if !gid_s.chars().all(|c| c.is_ascii_digit()) {
                        return Err(mlua::Error::external(format!(
                            "setmanifestid: gid must be digits, got '{gid_s}'"
                        )));
                    }
                    let gid: u64 = gid_s.parse().map_err(|e| {
                        mlua::Error::external(format!("setmanifestid: bad gid: {e}"))
                    })?;
                    lock_bundle(&b).manifests.insert(
                        depot_id,
                        ManifestOverride {
                            manifest_gid: gid,
                            // 上游忽略 size; 这里可选 size 方便测试.
                            size: size.unwrap_or(0),
                        },
                    );
                    Ok(())
                },
            )
            .map_err(lua_err)?;
        lua.globals().set("setmanifestid", f).map_err(lua_err)?;
    }

    lua.load(source)
        .exec()
        .map_err(|e| ConfigError::Lua(e.to_string()))?;

    let out = lock_bundle(&bundle).clone();
    Ok(out)
}

fn lua_err(e: mlua::Error) -> ConfigError {
    ConfigError::Lua(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addappid_and_token_and_manifest() {
        let src = r#"
addappid(1361510)
addappid(1361510, 0, "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
addtoken(1361510, "1234567890")
setmanifestid(228980, "9876543210")
"#;
        let mut rules = AppRules::new();
        apply_lua_chunk(&mut rules, src).unwrap();
        assert!(rules.is_owned(1361510));
        assert_eq!(rules.depot_key(1361510).map(|s| s.len()), Some(64));
        assert_eq!(rules.access_token(1361510), Some(1234567890));
        assert_eq!(
            rules.manifest_override(228980).map(|m| m.manifest_gid),
            Some(9876543210)
        );
    }

    #[test]
    fn short_key_ignored() {
        let mut rules = AppRules::new();
        apply_lua_chunk(&mut rules, r#"addappid(1, 0, "ab")"#).unwrap();
        assert!(rules.is_owned(1));
        assert!(rules.depot_key(1).is_none());
    }
}
