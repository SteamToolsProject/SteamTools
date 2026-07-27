//! `CatalogBundle` 的集中校验和规范化.

use std::collections::HashSet;

use stt_core::{AppId, CatalogBundle, DepotId};

use crate::{CatalogError, CatalogResult};

/// Catalog v1 的资源上限.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogLimits {
    /// 单次结果最多包含的 app 数.
    pub max_apps: usize,
    /// 单次结果最多包含的 app-depot 关联数.
    pub max_depots: usize,
    /// wire body 最大字节数.
    pub max_wire_bytes: usize,
}

impl Default for CatalogLimits {
    fn default() -> Self {
        Self {
            max_apps: 256,
            max_depots: 4096,
            max_wire_bytes: 1024 * 1024,
        }
    }
}

/// 校验并规范化 provider 返回的 bundle.
///
/// 规范化会排序 app/depot, 并把 hex key 统一为小写.
///
/// # Errors
///
/// 请求 app 缺失, ID/metadata 无效, 重复项或数量超限时返回 [`CatalogError`].
pub fn validate_bundle(
    requested_app: AppId,
    mut bundle: CatalogBundle,
) -> CatalogResult<CatalogBundle> {
    validate_bundle_with_limits(requested_app, &mut bundle, CatalogLimits::default())?;
    Ok(bundle)
}

pub(crate) fn validate_bundle_with_limits(
    requested_app: AppId,
    bundle: &mut CatalogBundle,
    limits: CatalogLimits,
) -> CatalogResult<()> {
    validate_apps(requested_app, bundle, limits)?;
    let declared_depots = validate_app_depots(bundle, limits)?;
    validate_metadata(bundle, &declared_depots)?;
    normalize(bundle);
    Ok(())
}

fn validate_apps(
    requested_app: AppId,
    bundle: &CatalogBundle,
    limits: CatalogLimits,
) -> CatalogResult<()> {
    if bundle.apps.is_empty() {
        return Err(CatalogError::EmptyBundle);
    }
    if bundle.apps.len() > limits.max_apps {
        return Err(CatalogError::TooManyEntries {
            field: "apps",
            actual: bundle.apps.len(),
            limit: limits.max_apps,
        });
    }

    let mut seen = HashSet::with_capacity(bundle.apps.len());
    for &app_id in &bundle.apps {
        if app_id == 0 {
            return Err(CatalogError::ZeroAppId);
        }
        if !seen.insert(app_id) {
            return Err(CatalogError::DuplicateApp(app_id));
        }
    }
    if !seen.contains(&requested_app) {
        return Err(CatalogError::RequestedAppMissing(requested_app));
    }

    for &app_id in bundle
        .access_tokens
        .keys()
        .chain(bundle.purchase_times.keys())
        .chain(bundle.app_depots.keys())
    {
        if !seen.contains(&app_id) {
            return Err(CatalogError::UndeclaredApp(app_id));
        }
    }
    Ok(())
}

fn validate_app_depots(
    bundle: &CatalogBundle,
    limits: CatalogLimits,
) -> CatalogResult<HashSet<DepotId>> {
    let depot_count = bundle.app_depots.values().map(Vec::len).sum::<usize>();
    if depot_count > limits.max_depots {
        return Err(CatalogError::TooManyEntries {
            field: "depots",
            actual: depot_count,
            limit: limits.max_depots,
        });
    }

    let mut declared = HashSet::with_capacity(depot_count);
    for (&app_id, depots) in &bundle.app_depots {
        let mut app_seen = HashSet::with_capacity(depots.len());
        for &depot_id in depots {
            if depot_id == 0 {
                return Err(CatalogError::ZeroDepotId);
            }
            if !app_seen.insert(depot_id) {
                return Err(CatalogError::DuplicateDepot { app_id, depot_id });
            }
            declared.insert(depot_id);
        }
    }
    Ok(declared)
}

fn validate_metadata(
    bundle: &CatalogBundle,
    declared_depots: &HashSet<DepotId>,
) -> CatalogResult<()> {
    for (&depot_id, key) in &bundle.depot_keys {
        if !declared_depots.contains(&depot_id) {
            return Err(CatalogError::UndeclaredDepot(depot_id));
        }
        if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(CatalogError::InvalidDepotKey { depot_id });
        }
    }
    for (&depot_id, manifest) in &bundle.manifests {
        if !declared_depots.contains(&depot_id) {
            return Err(CatalogError::UndeclaredDepot(depot_id));
        }
        if manifest.manifest_gid == 0 {
            return Err(CatalogError::InvalidDecimalU64 {
                field: format!("depots[{depot_id}].manifest.gid"),
            });
        }
    }
    if let Some((&app_id, _)) = bundle.access_tokens.iter().find(|(_, token)| **token == 0) {
        return Err(CatalogError::InvalidDecimalU64 {
            field: format!("apps[{app_id}].access_token"),
        });
    }
    Ok(())
}

fn normalize(bundle: &mut CatalogBundle) {
    bundle.apps.sort_unstable();
    for depots in bundle.app_depots.values_mut() {
        depots.sort_unstable();
    }
    for key in bundle.depot_keys.values_mut() {
        key.make_ascii_lowercase();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use stt_core::ManifestOverride;

    use super::*;

    fn valid_bundle() -> CatalogBundle {
        CatalogBundle {
            apps: vec![42],
            app_depots: HashMap::from([(42, vec![43])]),
            depot_keys: HashMap::from([(43, "AB".repeat(32))]),
            manifests: HashMap::from([(
                43,
                ManifestOverride {
                    manifest_gid: 99,
                    size: 0,
                },
            )]),
            ..CatalogBundle::default()
        }
    }

    #[test]
    fn validate_bundle_normalizes_order_and_key_case() {
        let bundle = validate_bundle(42, valid_bundle()).unwrap();

        assert_eq!(bundle.depot_keys[&43], "ab".repeat(32));
    }

    #[test]
    fn validate_bundle_rejects_empty_bundle() {
        let error = validate_bundle(42, CatalogBundle::default()).unwrap_err();

        assert!(matches!(error, CatalogError::EmptyBundle));
    }

    #[test]
    fn validate_bundle_rejects_missing_requested_app() {
        let error = validate_bundle(7, valid_bundle()).unwrap_err();

        assert!(matches!(error, CatalogError::RequestedAppMissing(7)));
    }

    #[test]
    fn validate_bundle_rejects_duplicate_app() {
        let mut bundle = valid_bundle();
        bundle.apps.push(42);

        let error = validate_bundle(42, bundle).unwrap_err();

        assert!(matches!(error, CatalogError::DuplicateApp(42)));
    }

    #[test]
    fn validate_bundle_rejects_duplicate_depot_for_app() {
        let mut bundle = valid_bundle();
        bundle.app_depots.get_mut(&42).unwrap().push(43);

        let error = validate_bundle(42, bundle).unwrap_err();

        assert!(matches!(
            error,
            CatalogError::DuplicateDepot {
                app_id: 42,
                depot_id: 43
            }
        ));
    }

    #[test]
    fn validate_bundle_rejects_bad_key() {
        let mut bundle = valid_bundle();
        bundle.depot_keys.insert(43, "not-a-key".into());

        let error = validate_bundle(42, bundle).unwrap_err();

        assert!(matches!(
            error,
            CatalogError::InvalidDepotKey { depot_id: 43 }
        ));
    }

    #[test]
    fn validate_bundle_rejects_undeclared_depot() {
        let mut bundle = valid_bundle();
        bundle.depot_keys.insert(99, "ab".repeat(32));

        let error = validate_bundle(42, bundle).unwrap_err();

        assert!(matches!(error, CatalogError::UndeclaredDepot(99)));
    }

    #[test]
    fn validate_bundle_rejects_app_count_over_limit() {
        let mut bundle = valid_bundle();
        bundle.apps.push(7);
        let limits = CatalogLimits {
            max_apps: 1,
            ..CatalogLimits::default()
        };

        let error = validate_bundle_with_limits(42, &mut bundle, limits).unwrap_err();

        assert!(matches!(
            error,
            CatalogError::TooManyEntries { field: "apps", .. }
        ));
    }
}
