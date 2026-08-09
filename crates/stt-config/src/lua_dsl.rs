//! Lua DSL -> CatalogBundle / AppRules (mlua).

use std::sync::{Arc, Mutex, MutexGuard};

use mlua::Table;
use stt_core::{AppRules, CatalogBundle, ManifestOverride};

use crate::error::{ConfigError, Result};
use crate::lua_http::{register_lua_http, LuaHttpClient};
use crate::lua_vm::new_sandboxed;

fn lock_bundle(b: &Mutex<CatalogBundle>) -> MutexGuard<'_, CatalogBundle> {
    b.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 执行一段 Lua 配置并合并进 `rules`.
///
/// 兼容面 (小写主名 + 社区常见大小写别名):
/// - `addappid` / `AddAppId`
/// - `addtoken` / `AddToken`
/// - `setmanifestid` / `setManifestid` / `SetManifestid` / `SetManifestId`
/// - `setappdepots` / `setAppDepots` / `SetAppDepots`
/// - `setappticket` / `setAppticket` / `setAppTicket` (写本机 AppTicket)
/// - `seteticket` / `setETicket` / `setAppEticket` (写本机 ETicket)
/// - `setstat` / `setStat` (写 per-app SteamID, 成就路径用)
pub fn apply_lua_chunk(rules: &mut AppRules, source: &str) -> Result<()> {
    let bundle = eval_lua_to_bundle(source)?;
    rules.apply_catalog_bundle(&bundle);
    Ok(())
}

pub fn eval_lua_to_bundle(source: &str) -> Result<CatalogBundle> {
    eval_lua_to_bundle_inner(source, None)
}

/// 使用注入的受限 HTTP client 执行 Lua DSL.
pub fn eval_lua_to_bundle_with_http(
    source: &str,
    client: Arc<dyn LuaHttpClient>,
) -> Result<CatalogBundle> {
    eval_lua_to_bundle_inner(source, Some(client))
}

fn eval_lua_to_bundle_inner(
    source: &str,
    http_client: Option<Arc<dyn LuaHttpClient>>,
) -> Result<CatalogBundle> {
    let lua = new_sandboxed().map_err(lua_err)?;
    let bundle = Arc::new(Mutex::new(CatalogBundle::default()));

    if let Some(client) = http_client {
        register_lua_http(&lua, client).map_err(lua_err)?;
    }

    {
        let b = Arc::clone(&bundle);
        let f = lua
            .create_function(
                move |_, (id, purchase_time, key): (u32, Option<u32>, Option<String>)| {
                    let mut g = lock_bundle(&b);
                    if let Some(k) = key {
                        let key_ok = k.len() == 64 && k.chars().all(|c| c.is_ascii_hexdigit());
                        if key_ok {
                            g.depot_keys.insert(id, k);
                        }
                        // 社区常见: addappid(app, purchaseTime, "64hex") 主 app 自带 key.
                        // purchase_time != 0 视为 app; ==0 多为纯 depot (只收 key).
                        let treat_as_app = purchase_time.is_some_and(|t| t != 0);
                        if treat_as_app {
                            if !g.apps.contains(&id) {
                                g.apps.push(id);
                            }
                            if let Some(pt) = purchase_time.filter(|v| *v != 0) {
                                g.purchase_times.insert(id, pt);
                            }
                        }
                        if g.apps.contains(&id) {
                            let depots = g.app_depots.entry(id).or_default();
                            if !depots.contains(&id) {
                                depots.push(id);
                            }
                        }
                    } else {
                        if !g.apps.contains(&id) {
                            g.apps.push(id);
                        }
                        if let Some(purchase_time) = purchase_time.filter(|value| *value != 0) {
                            g.purchase_times.insert(id, purchase_time);
                        }
                    }
                    Ok(())
                },
            )
            .map_err(lua_err)?;
        // 社区 lua 大小写不统一, 主名 + 常见别名都挂同一函数.
        set_global_aliases(&lua, &["addappid", "AddAppId", "AddAppID"], f)?;
    }

    {
        let b = Arc::clone(&bundle);
        let f = lua
            .create_function(move |_, (app_id, values): (u32, Table)| {
                const MAX_DEPOTS_PER_APP: usize = 4096;
                let mut depots = Vec::new();
                for value in values.sequence_values::<u32>() {
                    let depot_id = value?;
                    if depot_id == 0 || depots.len() >= MAX_DEPOTS_PER_APP {
                        return Err(mlua::Error::external("setappdepots: invalid depot list"));
                    }
                    if !depots.contains(&depot_id) {
                        depots.push(depot_id);
                    }
                }
                let mut g = lock_bundle(&b);
                if !g.apps.contains(&app_id) {
                    g.apps.push(app_id);
                }
                g.app_depots.insert(app_id, depots);
                Ok(())
            })
            .map_err(lua_err)?;
        set_global_aliases(&lua, &["setappdepots", "setAppDepots", "SetAppDepots"], f)?;
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
        set_global_aliases(&lua, &["addtoken", "AddToken"], f)?;
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
                            // size 写入 hook 快照; 0 = 保留 Steam 原值 (假 license 时常为 0 → UI 0B).
                            size: size.unwrap_or(0),
                        },
                    );
                    Ok(())
                },
            )
            .map_err(lua_err)?;
        // ManifestAutoUpdate / OpenSteamTool 系常用 setManifestid.
        set_global_aliases(
            &lua,
            &[
                "setmanifestid",
                "setManifestid",
                "SetManifestid",
                "SetManifestId",
            ],
            f,
        )?;
    }

    // setAppTicket(appId, hex): 写 HKCU AppTicket, 不进 CatalogBundle.
    {
        let f = lua
            .create_function(move |_, (app_id, hex): (u32, String)| {
                stt_platform::write_app_ticket_hex(app_id, &hex)
                    .map_err(|e| mlua::Error::external(format!("setappticket: {e}")))
            })
            .map_err(lua_err)?;
        set_global_aliases(
            &lua,
            &[
                "setappticket",
                "setAppticket",
                "setAppTicket",
                "SetAppTicket",
            ],
            f,
        )?;
    }

    // setETicket / setAppEticket: 写 HKCU ETicket.
    {
        let f = lua
            .create_function(move |_, (app_id, hex): (u32, String)| {
                stt_platform::write_eticket_hex(app_id, &hex)
                    .map_err(|e| mlua::Error::external(format!("seteticket: {e}")))
            })
            .map_err(lua_err)?;
        set_global_aliases(
            &lua,
            &[
                "seteticket",
                "setETicket",
                "setEticket",
                "SetETicket",
                "setappeticket",
                "setAppEticket",
                "SetAppEticket",
            ],
            f,
        )?;
    }

    // setStat(appId, "steamId"): 写 per-app SteamID (成就 spoof 用).
    {
        let f = lua
            .create_function(move |_, (app_id, sid_s): (u32, String)| {
                let steam_id: u64 = sid_s.parse().map_err(|e| {
                    mlua::Error::external(format!("setstat: steamId must be digits: {e}"))
                })?;
                stt_platform::write_steam_id(app_id, steam_id)
                    .map_err(|e| mlua::Error::external(format!("setstat: {e}")))
            })
            .map_err(lua_err)?;
        set_global_aliases(&lua, &["setstat", "setStat", "SetStat"], f)?;
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

/// 同一函数挂多个全局名 (社区脚本大小写混用).
fn set_global_aliases(lua: &mlua::Lua, names: &[&str], f: mlua::Function) -> Result<()> {
    for name in names {
        lua.globals().set(*name, f.clone()).map_err(lua_err)?;
    }
    Ok(())
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
        assert_eq!(rules.app_depots(1361510), &[1361510]);
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
        assert!(!rules.is_owned(1));
        assert!(rules.depot_key(1).is_none());
    }

    #[test]
    fn explicit_app_depots_and_purchase_time_survive_lua() {
        let mut rules = AppRules::new();
        apply_lua_chunk(
            &mut rules,
            "addappid(42, 123)\naddappid(43, 0, \"\")\nsetappdepots(42, {43})",
        )
        .unwrap();

        assert_eq!(rules.app_depots(42), &[43]);
        assert_eq!(rules.purchase_time(42), Some(123));
        assert!(!rules.is_owned(43));
    }

    /// 社区 ManifestAutoUpdate 系常用 setManifestid (大写 M).
    #[test]
    fn community_case_aliases_work() {
        let src = r#"
AddAppId(3167020)
addappid(3167021, 0, "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
setManifestid(3167021, "1234567890")
AddToken(3167020, "42")
"#;
        let mut rules = AppRules::new();
        apply_lua_chunk(&mut rules, src).unwrap();
        assert!(rules.is_owned(3167020));
        assert_eq!(
            rules.manifest_override(3167021).map(|m| m.manifest_gid),
            Some(1234567890)
        );
        assert_eq!(rules.access_token(3167020), Some(42));
    }

    /// 社区包: 主 app 一行带 purchaseTime + key, depot 用 purchaseTime=0.
    #[test]
    fn app_with_purchase_time_and_key_is_owned() {
        let key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let src = format!(
            "addappid(4570720, 1, \"{key}\")\naddappid(4570721, 0, \"{key}\")\naddappid(4742640)\n"
        );
        let mut rules = AppRules::new();
        apply_lua_chunk(&mut rules, &src).unwrap();
        assert!(rules.is_owned(4570720));
        assert_eq!(rules.purchase_time(4570720), Some(1));
        assert_eq!(rules.depot_key(4570720).map(|s| s.len()), Some(64));
        // depot 行不进 owned, 只收 key.
        assert!(!rules.is_owned(4570721));
        assert_eq!(rules.depot_key(4570721).map(|s| s.len()), Some(64));
        assert!(rules.is_owned(4742640));
    }

    /// setAppticket 写注册表, 脚本不因未知函数失败.
    #[test]
    fn set_appticket_writes_credential_store() {
        const APP: u32 = 4_000_000_010;
        let src = r#"
addappid(4000000010)
setAppticket(4000000010, "aabbccdd")
"#;
        let mut rules = AppRules::new();
        apply_lua_chunk(&mut rules, src).unwrap();
        assert!(rules.is_owned(APP));
        let got = stt_platform::read_app_ticket(APP).unwrap();
        assert_eq!(got, vec![0xaa, 0xbb, 0xcc, 0xdd]);
    }

    #[test]
    fn set_eticket_alias_writes_credential_store() {
        const APP: u32 = 4_000_000_011;
        let src = r#"
addappid(4000000011)
setAppEticket(4000000011, "11223344")
"#;
        apply_lua_chunk(&mut AppRules::new(), src).unwrap();
        let got = stt_platform::read_eticket(APP).unwrap();
        assert_eq!(got, vec![0x11, 0x22, 0x33, 0x44]);
    }
}
