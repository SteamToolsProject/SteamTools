//! VDF / Lua 中的 depot decryption key / ticket 提取.
//!
//! 从 community 抽出, 打断 caigamer → community 的反向依赖: 两边都走本模块.
//! ticket 行不执行脚本, 只做大小写不敏感的行解析 (对齐 OST LuaConfig).

use std::collections::HashMap;

use stt_core::{AppId, DepotId};

use crate::{CatalogError, CatalogResult, ProviderErrorKind};

/// 社区 lua 旁路凭证 (hex / steamid 原文, 不解码).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LuaTicketSet {
    pub app_tickets: HashMap<AppId, String>,
    pub etickets: HashMap<AppId, String>,
    pub steam_ids: HashMap<AppId, String>,
}

impl LuaTicketSet {
    pub fn is_empty(&self) -> bool {
        self.app_tickets.is_empty() && self.etickets.is_empty() && self.steam_ids.is_empty()
    }

    /// 并入 CatalogBundle 对应字段 (不覆盖已有).
    pub fn merge_into_bundle(&self, bundle: &mut stt_core::CatalogBundle) {
        merge_hex_map(&self.app_tickets, &mut bundle.app_tickets);
        merge_hex_map(&self.etickets, &mut bundle.etickets);
        merge_hex_map(&self.steam_ids, &mut bundle.steam_ids);
    }
}

fn merge_hex_map(src: &HashMap<AppId, String>, dst: &mut HashMap<AppId, String>) {
    for (&app_id, value) in src {
        match dst.get(&app_id) {
            Some(existing) if existing.eq_ignore_ascii_case(value) => {}
            Some(_) => {}
            None => {
                dst.insert(app_id, value.clone());
            }
        }
    }
}

/// ticket hex 上限: 与 credential_store 64 KiB 对齐 (hex 字符数 = 2 * 字节).
const MAX_TICKET_HEX_CHARS: usize = 64 * 1024 * 2;

const MAX_VDF_DEPTH: usize = 16;
const MAX_VDF_NODES: usize = 8192;

/// 从 key.vdf / config.vdf 文本收集 `depots.<id>.DecryptionKey`.
pub(crate) fn collect_vdf_keys(
    provider: &'static str,
    text: &str,
    keys: &mut HashMap<DepotId, String>,
) -> CatalogResult<()> {
    let parsed = keyvalues_parser::parse(text).map_err(|_| {
        provider_error(
            provider,
            ProviderErrorKind::Rejected,
            "invalid VDF key metadata",
        )
    })?;
    let mut nodes = 0;
    visit_vdf_pair(
        provider,
        parsed.key.as_ref(),
        &parsed.value,
        0,
        &mut nodes,
        keys,
    )
}

fn visit_vdf_pair(
    provider: &'static str,
    key: &str,
    value: &keyvalues_parser::Value<'_>,
    depth: usize,
    nodes: &mut usize,
    keys: &mut HashMap<DepotId, String>,
) -> CatalogResult<()> {
    *nodes = nodes.saturating_add(1);
    if *nodes > MAX_VDF_NODES || depth > MAX_VDF_DEPTH {
        return Err(provider_error(
            provider,
            ProviderErrorKind::Rejected,
            "VDF structure exceeds limits",
        ));
    }
    let Some(object) = value.get_obj() else {
        return Ok(());
    };
    if key.eq_ignore_ascii_case("depots") {
        collect_depots_object(object, keys)?;
    }
    for (child_key, child_values) in object.iter() {
        for child_value in child_values {
            visit_vdf_pair(
                provider,
                child_key.as_ref(),
                child_value,
                depth + 1,
                nodes,
                keys,
            )?;
        }
    }
    Ok(())
}

fn collect_depots_object(
    object: &keyvalues_parser::Obj<'_>,
    keys: &mut HashMap<DepotId, String>,
) -> CatalogResult<()> {
    for (depot_text, values) in object.iter() {
        let Ok(depot_id) = depot_text.parse::<DepotId>() else {
            continue;
        };
        if depot_id == 0 {
            return Err(CatalogError::ZeroDepotId);
        }
        for value in values {
            let Some(depot) = value.get_obj() else {
                continue;
            };
            for (name, candidates) in depot.iter() {
                if !name.eq_ignore_ascii_case("DecryptionKey") {
                    continue;
                }
                for candidate in candidates {
                    if let Some(key) = candidate.get_str() {
                        insert_archive_key(keys, depot_id, key)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// 从 archive 内 `.lua` 的 `addappid(depot, _, "key")` 行收集 key (不执行脚本).
pub(crate) fn collect_lua_keys(
    text: &str,
    keys: &mut HashMap<DepotId, String>,
) -> CatalogResult<()> {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("--") {
            continue;
        }
        let Some((name, rest)) = split_lua_call(line) else {
            continue;
        };
        if name != "addappid" {
            continue;
        }
        let Some(arguments) = rest.split_once(')').map(|(arguments, _)| arguments) else {
            continue;
        };
        let mut arguments = arguments.split(',').map(str::trim);
        let Some(depot_text) = arguments.next() else {
            continue;
        };
        let Some(_) = arguments.next() else {
            continue;
        };
        let Some(key_text) = arguments.next() else {
            continue;
        };
        if arguments.next().is_some() {
            continue;
        }
        let Ok(depot_id) = depot_text.parse::<DepotId>() else {
            continue;
        };
        if let Some(key) = strip_lua_string(key_text) {
            insert_archive_key(keys, depot_id, key)?;
        }
    }
    Ok(())
}

/// 从社区 lua 收集 `setAppticket` / `setETicket` / `setStat` (大小写不敏感, 不执行脚本).
///
/// 对齐 OST `LuaConfig.cpp` 注册名; 非法/空/超长 hex 静默跳过, 不拖垮整包.
pub(crate) fn collect_lua_tickets(text: &str, out: &mut LuaTicketSet) -> CatalogResult<()> {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("--") {
            continue;
        }
        let Some((name, rest)) = split_lua_call(line) else {
            continue;
        };
        let Some(arguments) = rest.split_once(')').map(|(arguments, _)| arguments) else {
            continue;
        };
        let parts: Vec<&str> = arguments.split(',').map(str::trim).collect();
        match name.as_str() {
            "setappticket" => {
                if let Some((app_id, hex)) = parse_app_string_args(&parts) {
                    insert_ticket_hex(&mut out.app_tickets, app_id, hex);
                }
            }
            "seteticket" | "setappeticket" => {
                if let Some((app_id, hex)) = parse_app_string_args(&parts) {
                    insert_ticket_hex(&mut out.etickets, app_id, hex);
                }
            }
            "setstat" => {
                if let Some((app_id, steam_id)) = parse_app_string_args(&parts) {
                    insert_steam_id(&mut out.steam_ids, app_id, steam_id);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// 函数名小写 + 去掉空白, 便于 `setAppTicket(` 等变体.
fn split_lua_call(line: &str) -> Option<(String, &str)> {
    let open = line.find('(')?;
    let name = line[..open].trim();
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return None;
    }
    Some((name.to_ascii_lowercase(), &line[open + 1..]))
}

fn strip_lua_string(text: &str) -> Option<&str> {
    text.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| text.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
}

fn parse_app_string_args<'a>(parts: &[&'a str]) -> Option<(AppId, &'a str)> {
    if parts.len() != 2 {
        return None;
    }
    let app_id = parts[0].parse::<AppId>().ok()?;
    if app_id == 0 {
        return None;
    }
    let value = strip_lua_string(parts[1])?;
    Some((app_id, value))
}

fn insert_ticket_hex(map: &mut HashMap<AppId, String>, app_id: AppId, hex: &str) {
    let hex = hex.trim();
    if hex.is_empty() || hex.len() > MAX_TICKET_HEX_CHARS {
        return;
    }
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return;
    }
    match map.get(&app_id) {
        Some(existing) if existing.eq_ignore_ascii_case(hex) => {}
        Some(_) => {}
        None => {
            map.insert(app_id, hex.to_ascii_lowercase());
        }
    }
}

fn insert_steam_id(map: &mut HashMap<AppId, String>, app_id: AppId, steam_id: &str) {
    let steam_id = steam_id.trim();
    if steam_id.is_empty() || !steam_id.bytes().all(|b| b.is_ascii_digit()) {
        return;
    }
    map.entry(app_id).or_insert_with(|| steam_id.to_owned());
}

fn insert_archive_key(
    keys: &mut HashMap<DepotId, String>,
    depot_id: DepotId,
    key: &str,
) -> CatalogResult<()> {
    if depot_id == 0 {
        return Err(CatalogError::ZeroDepotId);
    }
    // 非 64 hex 的 key 是上游数据格式问题 (如 CaiGames 主 depot 的超长 key),
    // 跳过该 depot 而不是让整份 archive 解析失败.
    if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(());
    }
    match keys.get(&depot_id) {
        Some(existing) if !existing.eq_ignore_ascii_case(key) => {
            Err(CatalogError::ConflictingDepot(depot_id))
        }
        Some(_) => Ok(()),
        None => {
            keys.insert(depot_id, key.to_owned());
            Ok(())
        }
    }
}

fn provider_error(
    provider: &'static str,
    kind: ProviderErrorKind,
    detail: impl Into<String>,
) -> CatalogError {
    CatalogError::Provider {
        provider: provider.to_owned(),
        kind,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_lua_tickets_case_insensitive() {
        let lua = r#"
setAppticket(42, "AaBb")
setETicket(42, "CcDd")
setStat(42, "76561198000000000")
setAppTicket(7, "1122")
"#;
        let mut tickets = LuaTicketSet::default();
        collect_lua_tickets(lua, &mut tickets).unwrap();
        assert_eq!(
            tickets.app_tickets.get(&42).map(String::as_str),
            Some("aabb")
        );
        assert_eq!(tickets.etickets.get(&42).map(String::as_str), Some("ccdd"));
        assert_eq!(
            tickets.steam_ids.get(&42).map(String::as_str),
            Some("76561198000000000")
        );
        assert_eq!(
            tickets.app_tickets.get(&7).map(String::as_str),
            Some("1122")
        );
    }

    #[test]
    fn collect_lua_tickets_skips_invalid() {
        let lua = r#"
setAppticket(0, "aa")
setAppticket(1, "zz")
setAppticket(2, "")
setStat(3, "not-digits")
"#;
        let mut tickets = LuaTicketSet::default();
        collect_lua_tickets(lua, &mut tickets).unwrap();
        assert!(tickets.is_empty());
    }

    #[test]
    fn collect_lua_keys_still_finds_addappid() {
        let key = "ab".repeat(32);
        let lua = format!(r#"addappid(43, 0, "{key}")"#);
        let mut keys = HashMap::new();
        collect_lua_keys(&lua, &mut keys).unwrap();
        assert_eq!(keys.get(&43).map(String::as_str), Some(key.as_str()));
    }
}
