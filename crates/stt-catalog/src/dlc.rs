//! 从 steamcmd 风格 JSON / store appdetails 提取 DLC 候选 AppId.

use serde_json::Value;
use stt_core::AppId;
use stt_platform::{winhttp_get, WinHttpGetOptions};

use std::collections::BTreeSet;

/// 从 steamcmd / caigames 风格的 appinfo JSON 提取 DLC 列表.
///
/// 来源 (并集):
/// - `common.listofdlc` / `extended.listofdlc` (逗号或空白分隔)
/// - `depots.dlc` 对象的 keys
/// - 顶层 `dlc` 对象的 keys
///
/// 去重、去 0、去主 app 自身, 升序返回. 解析失败返回空.
pub fn extract_dlc_ids(app_id: AppId, root: &Value) -> Vec<AppId> {
    let app_key = app_id.to_string();
    let app_data = root
        .get("data")
        .and_then(|data| data.get(&app_key))
        .or_else(|| root.get(&app_key))
        .unwrap_or(root);

    let mut ids = BTreeSet::new();
    collect_listofdlc(app_data.get("common"), &mut ids);
    collect_listofdlc(app_data.get("extended"), &mut ids);
    collect_object_keys(
        app_data.get("depots").and_then(|depots| depots.get("dlc")),
        &mut ids,
    );
    collect_object_keys(app_data.get("dlc"), &mut ids);

    ids.into_iter()
        .filter(|&id| id != 0 && id != app_id)
        .collect()
}

/// 从原始 JSON 字节提取; 非法 JSON 返回空.
pub fn extract_dlc_ids_from_bytes(app_id: AppId, body: &[u8]) -> Vec<AppId> {
    match serde_json::from_slice::<Value>(body) {
        Ok(root) => extract_dlc_ids(app_id, &root),
        Err(_) => Vec::new(),
    }
}

/// store.steampowered.com/api/appdetails 兜底; 失败返回空, 不抛错.
pub fn fetch_dlc_ids_store(app_id: AppId, options: WinHttpGetOptions) -> Vec<AppId> {
    let url = format!("https://store.steampowered.com/api/appdetails?appids={app_id}&l=english");
    let Ok(response) = winhttp_get(&url, options) else {
        return Vec::new();
    };
    if !(200..300).contains(&response.status) {
        return Vec::new();
    }
    let Ok(root) = serde_json::from_slice::<Value>(&response.body) else {
        return Vec::new();
    };
    let Some(list) = root
        .get(app_id.to_string())
        .and_then(|entry| entry.get("data"))
        .and_then(|data| data.get("dlc"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut ids = BTreeSet::new();
    for item in list {
        if let Some(id) = json_app_id(item) {
            if id != 0 && id != app_id {
                ids.insert(id);
            }
        }
    }
    ids.into_iter().collect()
}

fn collect_listofdlc(section: Option<&Value>, out: &mut BTreeSet<AppId>) {
    let Some(section) = section else {
        return;
    };
    let Some(list) = section.get("listofdlc") else {
        return;
    };
    match list {
        Value::String(s) => {
            for part in s.split(|c: char| !c.is_ascii_digit()) {
                if part.is_empty() {
                    continue;
                }
                if let Ok(id) = part.parse::<AppId>() {
                    out.insert(id);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                if let Some(id) = json_app_id(item) {
                    out.insert(id);
                }
            }
        }
        Value::Number(n) => {
            if let Some(id) = n.as_u64().and_then(|v| u32::try_from(v).ok()) {
                out.insert(id);
            }
        }
        _ => {}
    }
}

fn collect_object_keys(section: Option<&Value>, out: &mut BTreeSet<AppId>) {
    let Some(Value::Object(map)) = section else {
        return;
    };
    for key in map.keys() {
        if key.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(id) = key.parse::<AppId>() {
                out.insert(id);
            }
        }
    }
}

fn json_app_id(value: &Value) -> Option<AppId> {
    match value {
        Value::Number(n) => n.as_u64().and_then(|v| u32::try_from(v).ok()),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_from_listofdlc_and_objects() {
        let root = json!({
            "data": {
                "100": {
                    "common": { "listofdlc": "101, 102" },
                    "extended": { "listofdlc": "102 103" },
                    "depots": { "dlc": { "104": {}, "0": {} } },
                    "dlc": { "105": "name", "100": "self" }
                }
            }
        });
        assert_eq!(extract_dlc_ids(100, &root), vec![101, 102, 103, 104, 105]);
    }

    #[test]
    fn empty_when_no_dlc_fields() {
        let root = json!({ "data": { "42": { "depots": { "43": {} } } } });
        assert!(extract_dlc_ids(42, &root).is_empty());
    }

    #[test]
    fn ignores_invalid_json_bytes() {
        assert!(extract_dlc_ids_from_bytes(1, b"not-json").is_empty());
    }

    #[test]
    fn array_listofdlc() {
        let root = json!({
            "data": { "7": { "common": { "listofdlc": [8, "9", 0] } } }
        });
        assert_eq!(extract_dlc_ids(7, &root), vec![8, 9]);
    }
}
