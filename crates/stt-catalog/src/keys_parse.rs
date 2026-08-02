//! VDF / Lua 中的 depot decryption key 提取.
//!
//! 从 community 抽出, 打断 caigamer → community 的反向依赖: 两边都走本模块.

use std::collections::HashMap;

use stt_core::DepotId;

use crate::{CatalogError, CatalogResult, ProviderErrorKind};

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
        let Some(arguments) = line.strip_prefix("addappid(") else {
            continue;
        };
        let Some(arguments) = arguments.split_once(')').map(|(arguments, _)| arguments) else {
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
        let key = key_text
            .strip_prefix('"')
            .and_then(|key| key.strip_suffix('"'))
            .or_else(|| {
                key_text
                    .strip_prefix('\'')
                    .and_then(|key| key.strip_suffix('\''))
            });
        if let Some(key) = key {
            insert_archive_key(keys, depot_id, key)?;
        }
    }
    Ok(())
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
