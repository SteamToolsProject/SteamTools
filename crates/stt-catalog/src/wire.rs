//! Catalog JSON wire schema v1.

use std::collections::hash_map::Entry;

use serde::{Deserialize, Serialize};
use stt_core::{AppId, CatalogBundle, DepotId, ManifestOverride};

use crate::validate::validate_bundle_with_limits;
use crate::{CatalogError, CatalogLimits, CatalogResult};

/// 当前支持的 Catalog wire schema 版本.
pub const CATALOG_SCHEMA_V1: u32 = 1;

/// 完整 Catalog 响应的 v1 wire 文档.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogWireV1 {
    /// 固定为 `1`.
    pub schema_version: u32,
    /// 响应包含的 app 列表.
    pub apps: Vec<CatalogAppV1>,
}

/// v1 中的 app 条目.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogAppV1 {
    /// Steam AppId.
    pub app_id: AppId,
    /// PICS access token 的十进制字符串.
    #[serde(default)]
    pub access_token: Option<String>,
    /// 可选购买时间, Unix 秒.
    #[serde(default)]
    pub purchase_time: Option<u32>,
    /// 该 app 需要的 depot.
    #[serde(default)]
    pub depots: Vec<CatalogDepotV1>,
}

/// v1 中的 depot 条目.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogDepotV1 {
    /// Steam DepotId.
    pub depot_id: DepotId,
    /// 32-byte depot key 的 64 位 hex.
    #[serde(default)]
    pub key: Option<String>,
    /// 可选 manifest override.
    #[serde(default)]
    pub manifest: Option<CatalogManifestV1>,
}

/// v1 中的 manifest override.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogManifestV1 {
    /// Manifest GID 的十进制字符串.
    pub gid: String,
    /// 可选 manifest size 的十进制字符串; `0` 表示保留 Steam 原值.
    #[serde(default)]
    pub size: Option<String>,
}

impl CatalogWireV1 {
    /// 将未验证的 wire 文档转换成规范化 bundle.
    ///
    /// # Errors
    ///
    /// 版本不支持, ID/十进制字符串无效, metadata 冲突或契约校验失败时返回错误.
    pub fn into_bundle(self, requested_app: AppId) -> CatalogResult<CatalogBundle> {
        self.into_bundle_with_limits(requested_app, CatalogLimits::default())
    }

    fn into_bundle_with_limits(
        self,
        requested_app: AppId,
        limits: CatalogLimits,
    ) -> CatalogResult<CatalogBundle> {
        if self.schema_version != CATALOG_SCHEMA_V1 {
            return Err(CatalogError::UnsupportedSchema {
                found: self.schema_version,
                expected: CATALOG_SCHEMA_V1,
            });
        }

        let mut bundle = CatalogBundle::default();
        for app in self.apps {
            bundle.apps.push(app.app_id);
            if let Some(token) = app.access_token {
                let field = format!("apps[{}].access_token", app.app_id);
                bundle
                    .access_tokens
                    .insert(app.app_id, parse_nonzero_u64(&token, field)?);
            }
            if let Some(purchase_time) = app.purchase_time {
                bundle.purchase_times.insert(app.app_id, purchase_time);
            }

            let mut depot_ids = Vec::with_capacity(app.depots.len());
            for depot in app.depots {
                depot_ids.push(depot.depot_id);
                insert_depot_metadata(&mut bundle, depot)?;
            }
            bundle.app_depots.insert(app.app_id, depot_ids);
        }

        validate_bundle_with_limits(requested_app, &mut bundle, limits)?;
        Ok(bundle)
    }
}

/// 解析并校验 v1 JSON body.
///
/// # Errors
///
/// body 超限, JSON/schema 无效或 bundle 未通过契约校验时返回 [`CatalogError`].
pub fn parse_catalog_wire_v1(requested_app: AppId, body: &[u8]) -> CatalogResult<CatalogBundle> {
    parse_catalog_wire_v1_with_limits(requested_app, body, CatalogLimits::default())
}

fn parse_catalog_wire_v1_with_limits(
    requested_app: AppId,
    body: &[u8],
    limits: CatalogLimits,
) -> CatalogResult<CatalogBundle> {
    if body.len() > limits.max_wire_bytes {
        return Err(CatalogError::PayloadTooLarge {
            actual: body.len(),
            limit: limits.max_wire_bytes,
        });
    }
    let wire: CatalogWireV1 = serde_json::from_slice(body)?;
    wire.into_bundle_with_limits(requested_app, limits)
}

fn insert_depot_metadata(bundle: &mut CatalogBundle, depot: CatalogDepotV1) -> CatalogResult<()> {
    if let Some(key) = depot.key {
        match bundle.depot_keys.entry(depot.depot_id) {
            Entry::Vacant(slot) => {
                slot.insert(key);
            }
            Entry::Occupied(slot) if slot.get().eq_ignore_ascii_case(&key) => {}
            Entry::Occupied(_) => return Err(CatalogError::ConflictingDepot(depot.depot_id)),
        }
    }
    if let Some(manifest) = depot.manifest {
        let manifest = ManifestOverride {
            manifest_gid: parse_nonzero_u64(
                &manifest.gid,
                format!("depots[{}].manifest.gid", depot.depot_id),
            )?,
            size: parse_optional_u64(
                manifest.size.as_deref(),
                format!("depots[{}].manifest.size", depot.depot_id),
            )?,
        };
        match bundle.manifests.entry(depot.depot_id) {
            Entry::Vacant(slot) => {
                slot.insert(manifest);
            }
            Entry::Occupied(slot) if slot.get() == &manifest => {}
            Entry::Occupied(_) => return Err(CatalogError::ConflictingDepot(depot.depot_id)),
        }
    }
    Ok(())
}

fn parse_nonzero_u64(value: &str, field: String) -> CatalogResult<u64> {
    let parsed = parse_u64(value, &field)?;
    if parsed == 0 {
        return Err(CatalogError::InvalidDecimalU64 { field });
    }
    Ok(parsed)
}

fn parse_optional_u64(value: Option<&str>, field: String) -> CatalogResult<u64> {
    value.map_or(Ok(0), |value| parse_u64(value, &field))
}

fn parse_u64(value: &str, field: &str) -> CatalogResult<u64> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(CatalogError::InvalidDecimalU64 {
            field: field.to_owned(),
        });
    }
    value.parse().map_err(|_| CatalogError::InvalidDecimalU64 {
        field: field.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_json() -> Vec<u8> {
        br#"{
            "schema_version": 1,
            "apps": [{
                "app_id": 42,
                "access_token": "18446744073709551615",
                "depots": [{
                    "depot_id": 43,
                    "key": "abababababababababababababababababababababababababababababababab",
                    "manifest": {
                        "gid": "18446744073709551615",
                        "size": "0"
                    }
                }]
            }]
        }"#
        .to_vec()
    }

    #[test]
    fn parse_catalog_wire_v1_preserves_full_u64_strings() {
        let bundle = parse_catalog_wire_v1(42, &valid_json()).unwrap();

        assert_eq!(bundle.access_tokens[&42], u64::MAX);
    }

    #[test]
    fn parse_catalog_wire_v1_preserves_depot_identity() {
        let bundle = parse_catalog_wire_v1(42, &valid_json()).unwrap();

        assert_eq!(bundle.app_depots[&42], vec![43]);
    }

    #[test]
    fn parse_catalog_wire_v1_rejects_numeric_u64_fields() {
        let body = br#"{"schema_version":1,"apps":[{"app_id":42,"access_token":99}]}"#;

        let error = parse_catalog_wire_v1(42, body).unwrap_err();

        assert!(matches!(error, CatalogError::Json(_)));
    }

    #[test]
    fn parse_catalog_wire_v1_rejects_unknown_version() {
        let body = br#"{"schema_version":2,"apps":[]}"#;

        let error = parse_catalog_wire_v1(42, body).unwrap_err();

        assert!(matches!(
            error,
            CatalogError::UnsupportedSchema {
                found: 2,
                expected: 1
            }
        ));
    }

    #[test]
    fn parse_catalog_wire_v1_rejects_payload_over_limit() {
        let limits = CatalogLimits {
            max_wire_bytes: 8,
            ..CatalogLimits::default()
        };

        let error = parse_catalog_wire_v1_with_limits(42, &valid_json(), limits).unwrap_err();

        assert!(matches!(
            error,
            CatalogError::PayloadTooLarge { limit: 8, .. }
        ));
    }

    #[test]
    fn into_bundle_rejects_conflicting_shared_depot() {
        let wire = CatalogWireV1 {
            schema_version: 1,
            apps: vec![
                CatalogAppV1 {
                    app_id: 42,
                    access_token: None,
                    purchase_time: None,
                    depots: vec![CatalogDepotV1 {
                        depot_id: 43,
                        key: Some("ab".repeat(32)),
                        manifest: None,
                    }],
                },
                CatalogAppV1 {
                    app_id: 44,
                    access_token: None,
                    purchase_time: None,
                    depots: vec![CatalogDepotV1 {
                        depot_id: 43,
                        key: Some("cd".repeat(32)),
                        manifest: None,
                    }],
                },
            ],
        };

        let error = wire.into_bundle(42).unwrap_err();

        assert!(matches!(error, CatalogError::ConflictingDepot(43)));
    }
}
