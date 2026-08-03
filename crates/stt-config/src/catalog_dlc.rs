//! 主游戏入库后的 DLC 扩展 (仅解锁 + 有限可下载探测).

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use stt_catalog::{fetch_dlc_ids_store, CatalogLimits, CatalogProvider};
use stt_core::{AppId, CatalogBundle};
use stt_platform::WinHttpGetOptions;

use crate::appinfo::app_names;
use crate::catalog_add::missing_download_data;

/// 单次入库的 DLC 策略 (商店可覆盖全局 auto_dlc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogDlcMode {
    /// 主游戏 + 全部候选 DLC.
    Full,
    /// 只写主游戏.
    GameOnly,
    /// 只扩展用户勾选的子集; 空 = 仅游戏.
    Selected(Vec<AppId>),
}

impl CatalogDlcMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::GameOnly => "game_only",
            Self::Selected(_) => "selected",
        }
    }

    /// 映射到 expand 开关; Selected(空) 视作关闭.
    pub fn expand_options(&self, max_dlc: usize, timeout: Duration) -> DlcExpandOptions {
        match self {
            Self::GameOnly => DlcExpandOptions::disabled(),
            Self::Selected(ids) if ids.is_empty() => DlcExpandOptions::disabled(),
            Self::Full | Self::Selected(_) => DlcExpandOptions {
                enabled: true,
                max_dlc,
                timeout,
            },
        }
    }

    /// Selected 时作为 expand 的候选过滤; Full/GameOnly 为 None.
    pub fn selected_filter(&self) -> Option<&[AppId]> {
        match self {
            Self::Selected(ids) if !ids.is_empty() => Some(ids.as_slice()),
            _ => None,
        }
    }
}

/// DLC 扩展开关与预算.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DlcExpandOptions {
    pub enabled: bool,
    pub max_dlc: usize,
    pub timeout: Duration,
}

impl Default for DlcExpandOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            max_dlc: 64,
            timeout: Duration::from_millis(15_000),
        }
    }
}

impl DlcExpandOptions {
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            max_dlc: 0,
            timeout: Duration::from_millis(0),
        }
    }
}

/// 商店「选择 DLC」浮层用的一行.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DlcListItem {
    pub app_id: AppId,
    pub name: String,
}

/// 只读列出候选 DLC (不写盘).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DlcListOutcome {
    pub items: Vec<DlcListItem>,
    pub list_source: String,
    pub truncated: bool,
}

/// 一次 DLC 扩展的诊断.
///
/// - `unlock_only` / `downloadable`: 已写入 bundle 的 DLC
/// - `skipped`: **未**写入的 (cap / max_apps); 探测失败仍仅解锁的不算 skip
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DlcExpandReport {
    pub unlock_only: Vec<AppId>,
    pub downloadable: Vec<AppId>,
    pub skipped: Vec<(AppId, &'static str)>,
    pub list_source: String,
}

impl DlcExpandReport {
    pub fn total_added(&self) -> usize {
        self.unlock_only.len() + self.downloadable.len()
    }

    pub fn summary_suffix(&self) -> String {
        let n = self.total_added();
        if n == 0 {
            return String::new();
        }
        format!(
            " + {n} DLC (可下载 {} / 仅解锁 {})",
            self.downloadable.len(),
            self.unlock_only.len()
        )
    }
}

/// 有 public manifest 且 key/token 齐 = 可下载.
pub fn is_downloadable_dlc(app_id: AppId, bundle: &CatalogBundle) -> bool {
    let Some(depots) = bundle.app_depots.get(&app_id) else {
        return false;
    };
    depots
        .iter()
        .any(|depot_id| bundle.manifests.contains_key(depot_id))
        && missing_download_data(app_id, bundle).is_empty()
}

/// 只读列出主游戏的候选 DLC (供商店 picker).
///
/// 优先 provider 元数据里的 `related_dlc_ids`; 空则 store appdetails 兜底.
/// 名称来自本地 appinfo, 缺失时用 `DLC {id}`.
pub fn list_related_dlcs(
    steam_root: &Path,
    main_app: AppId,
    provider: &dyn CatalogProvider,
    http: WinHttpGetOptions,
    max_dlc: usize,
) -> DlcListOutcome {
    let mut list_source = "none".to_owned();
    let mut ids: Vec<AppId> = match provider.fetch_with_trace(main_app) {
        Ok(fetched) if !fetched.related_dlc_ids.is_empty() => {
            list_source = "metadata".to_owned();
            fetched.related_dlc_ids
        }
        _ => Vec::new(),
    };
    if ids.is_empty() {
        let store_ids = fetch_dlc_ids_store(main_app, http);
        if !store_ids.is_empty() {
            list_source = "store".to_owned();
            ids = store_ids;
        }
    }
    ids.retain(|&id| id != 0 && id != main_app);
    ids.sort_unstable();
    ids.dedup();

    let mut truncated = false;
    if max_dlc > 0 && ids.len() > max_dlc {
        ids.truncate(max_dlc);
        truncated = true;
    }

    let names = app_names(steam_root, &ids);
    let items = ids
        .into_iter()
        .map(|app_id| DlcListItem {
            name: names
                .get(&app_id)
                .cloned()
                .unwrap_or_else(|| format!("DLC {app_id}")),
            app_id,
        })
        .collect();
    DlcListOutcome {
        items,
        list_source,
        truncated,
    }
}

/// 把 DLC 候选并入主 bundle; DLC 失败不返回 Err.
///
/// `selected_filter`:
/// - `None` — 用 `related_dlc_ids` (空则 store 兜底), 即全量
/// - `Some(ids)` — 只处理用户勾选的 id (不再 store 扩表)
pub fn expand_dlcs(
    main_app: AppId,
    bundle: &mut CatalogBundle,
    related_dlc_ids: &[AppId],
    provider: &dyn CatalogProvider,
    opts: DlcExpandOptions,
    http: WinHttpGetOptions,
    selected_filter: Option<&[AppId]>,
) -> DlcExpandReport {
    let mut report = DlcExpandReport {
        list_source: if selected_filter.is_some() {
            "selected".to_owned()
        } else if related_dlc_ids.is_empty() {
            "none".to_owned()
        } else {
            // community metadata (steamcmd/ddxnb/caigames) 已带列表.
            "metadata".to_owned()
        },
        ..DlcExpandReport::default()
    };
    if !opts.enabled {
        return report;
    }

    let started = Instant::now();
    let mut candidates: Vec<AppId> = if let Some(selected) = selected_filter {
        selected
            .iter()
            .copied()
            .filter(|&id| id != 0 && id != main_app)
            .collect()
    } else {
        related_dlc_ids.to_vec()
    };
    if selected_filter.is_none() && candidates.is_empty() {
        // store 兜底: 用剩余预算收紧 HTTP 超时, 避免主路径已经很慢时再卡满 15s.
        let remain_ms = opts
            .timeout
            .saturating_sub(started.elapsed())
            .as_millis()
            .min(3_000) as u32;
        if remain_ms == 0 {
            return report;
        }
        let mut store_http = http;
        store_http.timeouts.resolve_ms = store_http.timeouts.resolve_ms.min(remain_ms);
        store_http.timeouts.connect_ms = store_http.timeouts.connect_ms.min(remain_ms);
        store_http.timeouts.send_ms = store_http.timeouts.send_ms.min(remain_ms);
        store_http.timeouts.receive_ms = store_http.timeouts.receive_ms.min(remain_ms);
        let store_ids = fetch_dlc_ids_store(main_app, store_http);
        if !store_ids.is_empty() {
            report.list_source = "store".to_owned();
            candidates = store_ids;
        }
    }

    // 已在主 bundle 的 app/depot 不重复处理.
    let mut known: HashSet<AppId> = bundle.apps.iter().copied().collect();
    known.extend(bundle.app_depots.values().flatten().copied());
    known.extend(bundle.depot_keys.keys().copied());

    let mut pending = Vec::new();
    for id in candidates {
        if id == 0 || id == main_app || !known.insert(id) {
            continue;
        }
        if pending.len() >= opts.max_dlc {
            report.skipped.push((id, "cap"));
            continue;
        }
        pending.push(id);
    }

    let mut unlock = Vec::new();
    let mut timed_out = false;
    for dlc_id in pending {
        if timed_out || started.elapsed() >= opts.timeout {
            // 超时后剩余全部仅解锁, 不再打网.
            timed_out = true;
            unlock.push(dlc_id);
            continue;
        }
        match provider.fetch(dlc_id) {
            Ok(dlc_bundle) if is_downloadable_dlc(dlc_id, &dlc_bundle) => {
                merge_dlc_bundle(bundle, &dlc_bundle, dlc_id);
                report.downloadable.push(dlc_id);
            }
            // 探测失败或不可下载 → 仅解锁, 不算 skip.
            Ok(_) | Err(_) => unlock.push(dlc_id),
        }
    }

    let purchase = bundle.purchase_times.get(&main_app).copied().unwrap_or(0);
    for &dlc_id in &unlock {
        if !bundle.apps.contains(&dlc_id) {
            bundle.apps.push(dlc_id);
        }
        if purchase != 0 {
            bundle.purchase_times.entry(dlc_id).or_insert(purchase);
        }
    }
    report.unlock_only = unlock;

    // max_apps 截断: 主 app 必须保留.
    let max_apps = CatalogLimits::default().max_apps;
    if bundle.apps.len() > max_apps {
        let mut keep = Vec::with_capacity(max_apps);
        keep.push(main_app);
        for &id in &bundle.apps {
            if id == main_app {
                continue;
            }
            if keep.len() >= max_apps {
                report.skipped.push((id, "max_apps"));
                report.unlock_only.retain(|x| *x != id);
                report.downloadable.retain(|x| *x != id);
                continue;
            }
            keep.push(id);
        }
        bundle.apps = keep;
        // 清掉被截掉 app 的附属字段 (validate 会再查).
        let kept: HashSet<AppId> = bundle.apps.iter().copied().collect();
        bundle.app_depots.retain(|app, _| kept.contains(app));
        bundle.access_tokens.retain(|app, _| kept.contains(app));
        bundle.purchase_times.retain(|app, _| kept.contains(app));
    }

    // 最终 validate 由 add_to_library 再做一次.
    report
}

fn merge_dlc_bundle(dest: &mut CatalogBundle, src: &CatalogBundle, dlc_id: AppId) {
    if !dest.apps.contains(&dlc_id) {
        dest.apps.push(dlc_id);
    }
    for &app in &src.apps {
        if !dest.apps.contains(&app) {
            dest.apps.push(app);
        }
    }
    for (&app, depots) in &src.app_depots {
        let entry = dest.app_depots.entry(app).or_default();
        for &depot in depots {
            if !entry.contains(&depot) {
                entry.push(depot);
            }
        }
    }
    // 只补缺失, 不覆盖主包已有 key/manifest/token.
    for (&depot, key) in &src.depot_keys {
        dest.depot_keys.entry(depot).or_insert_with(|| key.clone());
    }
    for (&depot, over) in &src.manifests {
        dest.manifests.entry(depot).or_insert_with(|| over.clone());
    }
    for (&app, &token) in &src.access_tokens {
        if token != 0 {
            dest.access_tokens.entry(app).or_insert(token);
        }
    }
    for (&app, &t) in &src.purchase_times {
        if t != 0 {
            dest.purchase_times.entry(app).or_insert(t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stt_catalog::MockCatalogProvider;
    use stt_core::ManifestOverride;

    fn main_bundle() -> CatalogBundle {
        let mut b = CatalogBundle::default();
        b.apps.push(10);
        b.app_depots.insert(10, vec![11]);
        b.depot_keys.insert(11, "ab".repeat(32));
        b.manifests.insert(
            11,
            ManifestOverride {
                manifest_gid: 1,
                size: 100,
            },
        );
        b.purchase_times.insert(10, 99);
        b.requires_token = Some(false);
        b
    }

    #[test]
    fn disabled_leaves_bundle_alone() {
        let mut b = main_bundle();
        let provider = MockCatalogProvider::new();
        let report = expand_dlcs(
            10,
            &mut b,
            &[20, 21],
            &provider,
            DlcExpandOptions::disabled(),
            WinHttpGetOptions::default(),
            None,
        );
        assert_eq!(b.apps, vec![10]);
        assert_eq!(report.total_added(), 0);
    }

    #[test]
    fn unlock_only_when_fetch_fails() {
        let mut b = main_bundle();
        let provider = MockCatalogProvider::new(); // 无 fixture → NotFound
        let report = expand_dlcs(
            10,
            &mut b,
            &[20, 21],
            &provider,
            DlcExpandOptions {
                enabled: true,
                max_dlc: 64,
                timeout: Duration::from_secs(5),
            },
            WinHttpGetOptions::default(),
            None,
        );
        assert!(b.apps.contains(&20) && b.apps.contains(&21));
        assert_eq!(report.unlock_only, vec![20, 21]);
        assert!(report.downloadable.is_empty());
        // 探测失败仍仅解锁, 不算 skip (skip 只表示未写入).
        assert!(report.skipped.is_empty());
        assert_eq!(b.purchase_times.get(&20), Some(&99));
    }

    #[test]
    fn list_source_metadata_when_related_present() {
        let mut b = main_bundle();
        let provider = MockCatalogProvider::new();
        let report = expand_dlcs(
            10,
            &mut b,
            &[20],
            &provider,
            DlcExpandOptions::default(),
            WinHttpGetOptions::default(),
            None,
        );
        assert_eq!(report.list_source, "metadata");
        assert_eq!(report.total_added(), 1);
    }

    #[test]
    fn selected_filter_only_expands_chosen_ids() {
        let mut b = main_bundle();
        let provider = MockCatalogProvider::new();
        let report = expand_dlcs(
            10,
            &mut b,
            &[20, 21, 22],
            &provider,
            DlcExpandOptions::default(),
            WinHttpGetOptions::default(),
            Some(&[21]),
        );
        assert_eq!(report.list_source, "selected");
        assert_eq!(report.unlock_only, vec![21]);
        assert!(b.apps.contains(&21));
        assert!(!b.apps.contains(&20));
        assert!(!b.apps.contains(&22));
    }

    #[test]
    fn downloadable_when_fixture_complete() {
        let mut b = main_bundle();
        let key = "cd".repeat(32);
        let mut dlc = CatalogBundle::default();
        dlc.apps.push(20);
        dlc.app_depots.insert(20, vec![21]);
        dlc.depot_keys.insert(21, key);
        dlc.manifests.insert(
            21,
            ManifestOverride {
                manifest_gid: 55,
                size: 9,
            },
        );
        dlc.requires_token = Some(false);
        let provider = MockCatalogProvider::new().with_fixture(20, dlc);

        let report = expand_dlcs(
            10,
            &mut b,
            &[20],
            &provider,
            DlcExpandOptions::default(),
            WinHttpGetOptions::default(),
            None,
        );
        assert_eq!(report.downloadable, vec![20]);
        assert!(b.apps.contains(&20));
        assert_eq!(b.app_depots.get(&20), Some(&vec![21]));
        assert!(b.depot_keys.contains_key(&21));
        assert!(b.manifests.contains_key(&21));
    }

    #[test]
    fn respects_max_dlc_cap() {
        let mut b = main_bundle();
        let provider = MockCatalogProvider::new();
        let report = expand_dlcs(
            10,
            &mut b,
            &[20, 21, 22],
            &provider,
            DlcExpandOptions {
                enabled: true,
                max_dlc: 1,
                timeout: Duration::from_secs(5),
            },
            WinHttpGetOptions::default(),
            None,
        );
        assert_eq!(report.unlock_only.len(), 1);
        assert!(report.skipped.iter().any(|(_, r)| *r == "cap"));
    }

    #[test]
    fn is_downloadable_requires_manifest_and_keys() {
        let mut b = CatalogBundle::default();
        b.apps.push(1);
        b.app_depots.insert(1, vec![2]);
        b.manifests.insert(
            2,
            ManifestOverride {
                manifest_gid: 1,
                size: 1,
            },
        );
        b.requires_token = Some(false);
        assert!(!is_downloadable_dlc(1, &b));
        b.depot_keys.insert(2, "ab".repeat(32));
        assert!(is_downloadable_dlc(1, &b));
    }
}
