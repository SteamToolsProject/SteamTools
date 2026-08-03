//! 宿主 DLL 入口 (`stbase.dll`).
//!
//! DllMain 只起工作线程, 真正初始化在线程里做.

// crate 名与导出函数名都由产物 DLL 决定 (stbase.dll / DllMain), 不能蛇形.
// 这里只能用 allow: crate 名的 non_snake_case 不被 expect 追踪, 写 expect 反而
// 会报 unfulfilled_lint_expectations.
#![allow(non_snake_case)]

mod host_log;
#[cfg(feature = "download-request-code")]
mod manifest_code;

use std::collections::HashSet;
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use host_log::{HostLogLevel, HostLogger};
use stt_catalog::{
    ensure_community_snapshots, CaigamerCatalogProvider, CatalogError, CatalogLimits,
    CatalogProvider, CatalogProviderChain, CatalogTraceEntry, CatalogTraceOutcome,
    CatmisteamCatalogProvider, CommunityCatalogProvider, CommunitySnapshotState,
    CustomHttpCatalogProvider, MockCatalogProvider, ProviderErrorKind,
};
use stt_config::{
    add_to_library_with_mode, apply_intent, import_local_texts, list_related_dlcs,
    remove_from_library, CatalogDlcMode, CatalogMode, CatalogSection, ConfigIntent, ConfigSnapshot,
    ConfigState, DlcExpandOptions, HostConfig, LuaCatalogProvider, LuaHttpClient, LuaHttpErrorKind,
    LuaHttpMethod, LuaHttpRequest, LuaHttpResponse, MissingDownloadData, StoreAccelEgress,
    StoreAccelSection, ToolId,
};
use stt_core::{AppId, AppRules};
use stt_steamclient::{LicenseQueue, UiLicenseAction};
use stt_steamui::StorePendingJob;

/// 进程内 package 许可队列 (init 时注册).
static LICENSE_QUEUE: OnceLock<Arc<LicenseQueue>> = OnceLock::new();
/// 配置内 app 集合, CheckAppOwnership 钩子只读这份.
static CONFIGURED_APPS: OnceLock<Arc<RwLock<HashSet<AppId>>>> = OnceLock::new();
/// 库 UX 纯逻辑控制器 (CancelRemoval / QueueRemoval).
static LIBRARY_UX: OnceLock<stt_steamui::LibraryUx> = OnceLock::new();
/// 进程内日志写入器, 只在 init 工作线程中创建.
static HOST_LOGGER: OnceLock<HostLogger> = OnceLock::new();
/// 自更新状态一行, 由后台 worker 写, 配置页快照读.
static UPDATE_STATUS: OnceLock<Mutex<String>> = OnceLock::new();
/// 面板 / 入库 / 导入共用的最近一次操作文案.
static SHARED_NOTE: OnceLock<Arc<Mutex<String>>> = OnceLock::new();
/// DLL 内商店代理的唯一运行实例.
static STORE_ACCEL_RUNTIME: OnceLock<Mutex<StoreAccelRuntime>> = OnceLock::new();

#[derive(Default)]
struct StoreAccelRuntime {
    generation: u64,
    active: Option<StoreAccelInstance>,
}

struct StoreAccelInstance {
    generation: u64,
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn store_accel_runtime() -> &'static Mutex<StoreAccelRuntime> {
    STORE_ACCEL_RUNTIME.get_or_init(|| Mutex::new(StoreAccelRuntime::default()))
}

fn license_queue() -> Option<Arc<LicenseQueue>> {
    LICENSE_QUEUE.get().map(Arc::clone)
}

fn library_ux() -> &'static stt_steamui::LibraryUx {
    LIBRARY_UX.get_or_init(stt_steamui::LibraryUx::new)
}

fn configured_apps() -> Option<Arc<RwLock<HashSet<AppId>>>> {
    CONFIGURED_APPS.get().map(Arc::clone)
}

/// 对齐上游 OST `GetAllDepotIds` / `InitFakeLicense`:
/// package0 与 CheckAppOwnership 要同时覆盖 **主 app + 全部 depot id**.
///
/// 只注入 app 时 Steam 认有 license, 但 depot 层 ownership/size 链不完整,
/// 安装对话框会显示 0 B.
fn package_ids_from_state(state: &ConfigState) -> Vec<AppId> {
    state.with_rules(|rules| {
        let mut ids = HashSet::new();
        for app_id in rules.owned_iter() {
            ids.insert(app_id);
            for &depot_id in rules.app_depots(app_id) {
                ids.insert(depot_id);
            }
        }
        for (depot_id, _) in rules.depot_keys_iter() {
            ids.insert(depot_id);
        }
        let mut out: Vec<AppId> = ids.into_iter().collect();
        out.sort_unstable();
        out
    })
}

/// 单次入库要写入 package0 的 id: 主 app + 其 depot.
fn package_ids_for_app(state: &ConfigState, app_id: AppId) -> Vec<AppId> {
    state.with_rules(|rules| {
        let mut ids = vec![app_id];
        for &depot_id in rules.app_depots(app_id) {
            if depot_id != app_id && !ids.contains(&depot_id) {
                ids.push(depot_id);
            }
        }
        ids.sort_unstable();
        ids
    })
}

fn sync_configured_from_state(state: &ConfigState) {
    // 先收集 id, 再锁一次.
    // CONFIGURED_APPS 与 package runtime 共用同一把 Arc<RwLock<HashSet>>,
    // 若持写锁时再调 set_configured_apps 会 **自死锁** (RwLock 写锁不可重入).
    let ids = package_ids_from_state(state);
    if let Some(set) = configured_apps() {
        let mut g = set
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.clear();
        g.extend(ids.iter().copied());
    } else {
        // runtime 尚未注册时仍推一份给 hooks 侧 (若已 register 则写同一把锁).
        stt_steamclient::set_configured_apps(ids);
    }
}

fn apply_ui_license_action(action: UiLicenseAction) {
    let ux = library_ux();
    match action {
        UiLicenseAction::CancelRemoval(id) => ux.cancel_removal(id),
        UiLicenseAction::QueueRemoval(id) => ux.queue_removal(id),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CatalogJobResult {
    Success,
    Partial,
    Failure,
}

impl CatalogJobResult {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Partial => "partial",
            Self::Failure => "failure",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Success => "成功",
            Self::Partial => "部分成功",
            Self::Failure => "失败",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LibraryAddedResult {
    result: CatalogJobResult,
    detail: String,
}

/// 入库成功后: 入队 + notify; package 降级必须反馈为部分成功.
///
/// 对齐 OST: package0 同时注入主 app 与全部 depot id (否则安装体积 0B).
/// `owned_apps` 含主游戏 + 本次并入的 DLC app (单文件模型).
fn on_library_added(
    steam_root: &Path,
    state: &ConfigState,
    primary: AppId,
    owned_apps: &[AppId],
) -> LibraryAddedResult {
    sync_download_runtime(state);
    let mut inject = HashSet::new();
    let apps = if owned_apps.is_empty() {
        std::slice::from_ref(&primary)
    } else {
        owned_apps
    };
    for &app_id in apps {
        for id in package_ids_for_app(state, app_id) {
            inject.insert(id);
        }
    }
    let mut inject_ids: Vec<AppId> = inject.into_iter().collect();
    inject_ids.sort_unstable();
    for &id in &inject_ids {
        stt_steamclient::add_configured_app(id);
    }
    let Some(q) = license_queue() else {
        append_host_log(steam_root, "package=notify skip=no_license_queue");
        return LibraryAddedResult {
            result: CatalogJobResult::Partial,
            detail: "license_queue_unavailable".to_owned(),
        };
    };
    for &id in &inject_ids {
        q.queue_addition(id);
    }
    let plan = stt_steamclient::notify_license_changed(&q);
    append_host_log(
        steam_root,
        &format!(
            "{} package_ids={}",
            plan.summary_line(),
            inject_ids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
    );
    // 库 UI 跟 owned 全集; present 只点主 app 触发刷新.
    state.with_rules(|rules| library_ux().sync_from_rules(rules));
    library_ux().on_rules_app_present(primary);
    if plan.client_applied {
        LibraryAddedResult {
            result: CatalogJobResult::Success,
            detail: "package_notified".to_owned(),
        }
    } else {
        let detail = plan
            .skip_reason
            .map(|reason| format!("package_notify_{reason}"))
            .unwrap_or_else(|| "package_notify_not_applied".to_owned());
        LibraryAddedResult {
            result: CatalogJobResult::Partial,
            detail,
        }
    }
}

fn catalog_job_note(
    result: CatalogJobResult,
    app_id: AppId,
    dlc_suffix: &str,
    detail: &str,
) -> String {
    format!("{}: 入库 {app_id}{dlc_suffix}: {detail}", result.label())
}

fn catalog_button_label(result: CatalogJobResult, app_id: AppId, _dlc_total: usize) -> String {
    match result {
        CatalogJobResult::Success | CatalogJobResult::Partial => "已入库".to_owned(),
        CatalogJobResult::Failure => format!("入库失败 {app_id}"),
    }
}

/// 缺下载数据时的日志后缀 (空 = 齐全).
///
/// 注意: `access_token=` 后跟任何值都会被日志脱敏成 `<redacted>`,
/// 所以状态用 `missing_access_token=1`, 否则 `access_token=missing` 里的
/// `missing` 会被当成 token 值藏掉, 日志反而看不出"缺"。
fn missing_log_suffix(missing: &MissingDownloadData) -> String {
    if missing.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    if !missing.depot_keys.is_empty() {
        let ids: Vec<String> = missing.depot_keys.iter().map(u32::to_string).collect();
        parts.push(format!("depot_keys={}", ids.join(",")));
    }
    if missing.access_token {
        parts.push("missing_access_token=1".to_owned());
    }
    format!(" missing={}", parts.join(" "))
}

/// 移除成功后.
///
/// 调用时 lua/rules 已不含该 app; 用全量 package id 集做差, 顺带清掉孤儿 depot.
fn on_library_removed(steam_root: &Path, state: &ConfigState, app_id: AppId) {
    sync_download_runtime(state);
    sync_configured_from_state(state);
    state.with_rules(|rules| library_ux().sync_from_rules(rules));
    let Some(q) = license_queue() else {
        append_host_log(steam_root, "package=notify skip=no_license_queue");
        return;
    };
    // 主 app 一定要出队; 其余用 reconcile 对齐 (含 depot).
    q.queue_removal(app_id);
    let keep = package_ids_from_state(state);
    q.reconcile_owned(keep.iter().copied());
    let plan = stt_steamclient::notify_license_changed(&q);
    append_host_log(steam_root, &plan.summary_line());
    library_ux().queue_removal(app_id);
}

/// lua 全量重载后: 与 owned 做差再 notify.
fn on_rules_reloaded(
    steam_root: &Path,
    state: &ConfigState,
    patterns: &stt_metadata::PatternStore,
) {
    sync_download_runtime(state);
    append_host_log(
        steam_root,
        &format!(
            "download_kit_reload {}",
            build_download_report(state, patterns).summary_line()
        ),
    );
    sync_configured_from_state(state);
    let Some(q) = license_queue() else {
        return;
    };
    // package0 对齐 app+depot; 库 UI 仍只跟 owned app.
    let package_ids = package_ids_from_state(state);
    let owned: Vec<AppId> = state.with_rules(|r| r.owned_iter().collect());
    q.reconcile_owned(package_ids.iter().copied());
    // 从配置消失的 app 走 UI 移除队列 (RunFrame drain 清 ownership).
    let before: std::collections::HashSet<AppId> =
        library_ux().owned_snapshot().into_iter().collect();
    state.with_rules(|rules| library_ux().sync_from_rules(rules));
    let after: std::collections::HashSet<AppId> =
        library_ux().owned_snapshot().into_iter().collect();
    for id in before.difference(&after) {
        library_ux().queue_removal(*id);
    }
    // 重载后取消仍在配置里的 app 的 UI 移除标记.
    for id in &owned {
        library_ux().on_rules_app_present(*id);
    }
    if q.pending_add_len() > 0 || q.pending_remove_len() > 0 {
        let plan = stt_steamclient::notify_license_changed(&q);
        append_host_log(steam_root, &plan.summary_line());
    }
}

const WATCH_DEBOUNCE: Duration = Duration::from_millis(500);
const WATCH_POLL: Duration = Duration::from_millis(250);

pub fn init_placeholder() -> AppRules {
    AppRules::new()
}

/// 把宿主 toml 载入新的 `ConfigState`.
pub fn bootstrap_config(steam_root: &Path) -> stt_config::Result<ConfigState> {
    let state = ConfigState::new();
    state.load_host_from_steam_root(steam_root)?;
    Ok(state)
}

fn tools_enabled_line(state: &ConfigState) -> String {
    let tools = state.tools();
    let enabled: Vec<&str> = ToolId::ALL
        .iter()
        .copied()
        .filter(|id| tools.is_enabled(*id))
        .map(ToolId::as_str)
        .collect();
    format!("tools_enabled={}\n", enabled.join(","))
}

fn catalog_mode_line(state: &ConfigState) -> String {
    format!("catalog_mode={}", state.host().catalog.mode.as_str())
}

struct WinHttpLuaClient {
    options: stt_platform::WinHttpRequestOptions,
}

impl LuaHttpClient for WinHttpLuaClient {
    fn execute(&self, request: LuaHttpRequest) -> Result<LuaHttpResponse, LuaHttpErrorKind> {
        let method = match request.method {
            LuaHttpMethod::Get => stt_platform::HttpMethod::Get,
            LuaHttpMethod::Post => stt_platform::HttpMethod::Post,
        };
        let response = stt_platform::winhttp_request(
            method,
            &request.url,
            &request.headers,
            &request.body,
            self.options,
        )
        .map_err(map_lua_http_error)?;
        Ok(LuaHttpResponse {
            status: response.status,
            body: response.body,
        })
    }
}

fn map_lua_http_error(error: stt_platform::HttpError) -> LuaHttpErrorKind {
    match error {
        stt_platform::HttpError::Timeout { .. } => LuaHttpErrorKind::Timeout,
        stt_platform::HttpError::RequestTooLarge { .. } => LuaHttpErrorKind::RequestTooLarge,
        stt_platform::HttpError::ResponseTooLarge { .. } => LuaHttpErrorKind::ResponseTooLarge,
        stt_platform::HttpError::InvalidUrl(_) | stt_platform::HttpError::InvalidOptions(_) => {
            LuaHttpErrorKind::InvalidRequest
        }
        stt_platform::HttpError::Windows { .. } => LuaHttpErrorKind::Unavailable,
    }
}

fn catalog_http_options(config: &CatalogSection) -> stt_platform::WinHttpRequestOptions {
    stt_platform::WinHttpRequestOptions {
        timeouts: stt_platform::WinHttpTimeouts {
            resolve_ms: config.timeout_resolve_ms,
            connect_ms: config.timeout_connect_ms,
            send_ms: config.timeout_send_ms,
            receive_ms: config.timeout_recv_ms,
        },
        max_request_body_bytes: 256 * 1024,
        max_response_body_bytes: config
            .max_response_bytes
            .min(CatalogLimits::default().max_wire_bytes),
    }
}

/// 内置社区聚合: 内部已含默认 CatalogEnricher 列表 (CatMisteam → CaiGamer 补全).
fn community_provider(steam_root: &Path, config: &CatalogSection) -> CommunityCatalogProvider {
    let request_options = catalog_http_options(config);
    let options = stt_platform::WinHttpGetOptions {
        timeouts: request_options.timeouts,
        max_body_bytes: request_options.max_response_body_bytes,
    };
    CommunityCatalogProvider::new(stt_platform::data_dir(steam_root).join("cache"), options)
}

/// CatMisteam 完整源兜底: Community 整段失败时, 在 CaiGamer 之前试 lua 直下.
/// 与 Community 内部的 CatMisteam CatalogEnricher 角色不同, 勿合并.
fn catmisteam_provider(config: &CatalogSection) -> CatmisteamCatalogProvider {
    let request_options = catalog_http_options(config);
    CatmisteamCatalogProvider::new(stt_platform::WinHttpGetOptions {
        timeouts: request_options.timeouts,
        max_body_bytes: request_options.max_response_body_bytes,
    })
}

/// 完整 CatalogProvider 兜底: 仅当前链上 Community / CatMisteam 等整段失败时使用.
/// 与 Community 内部的 CaiGamer CatalogEnricher 角色不同, 勿合并.
fn caigamer_provider(config: &CatalogSection) -> CaigamerCatalogProvider {
    let request_options = catalog_http_options(config);
    CaigamerCatalogProvider::new(stt_platform::WinHttpGetOptions {
        timeouts: request_options.timeouts,
        max_body_bytes: request_options.max_response_body_bytes,
    })
}

fn spawn_community_snapshot_refresh(steam_root: &Path, state: &ConfigState) {
    let host = state.host();
    if !matches!(
        host.catalog.mode,
        CatalogMode::CustomHttp | CatalogMode::Lua | CatalogMode::Community
    ) {
        return;
    }
    let root = steam_root.to_path_buf();
    let cache = stt_platform::data_dir(steam_root).join("cache");
    let timeouts = catalog_http_options(&host.catalog).timeouts;
    let spawn = std::thread::Builder::new()
        .name("community-cache".to_owned())
        .spawn(move || {
            let report = ensure_community_snapshots(&cache, timeouts);
            append_host_log(
                &root,
                &format!(
                    "community_cache=depotkeys {}",
                    community_snapshot_text(&report.depot_keys)
                ),
            );
            append_host_log(
                &root,
                &format!(
                    "community_cache=appaccesstokens {}",
                    community_snapshot_text(&report.access_tokens)
                ),
            );
        });
    if let Err(error) = spawn {
        append_host_log(
            steam_root,
            &format!("community_cache=worker unavailable {error}"),
        );
    }
}

/// 后台自更新检查: 发现新版本则下载校验并 swap 进根目录.
///
/// 全程网络在独立线程, 不阻塞 init; 结果写 [`UPDATE_STATUS`] 给配置页, 并进 host.log.
fn spawn_update_worker(steam_root: &Path, state: &ConfigState) {
    let config = state.host().update;
    if !config.enabled {
        set_update_status("已关闭");
        append_host_log(steam_root, "update=disabled");
        return;
    }
    let root = steam_root.to_path_buf();
    let spawn = std::thread::Builder::new()
        .name("update-check".to_owned())
        .spawn(move || {
            let outcome = stt_update::worker::run(&root, &stt_update::PlatformFetcher);
            append_host_log(&root, &outcome.summary());
            set_update_status(&status_for_panel(&outcome));
        });
    if let Err(error) = spawn {
        append_host_log(steam_root, &format!("update=worker unavailable {error}"));
    }
}

/// 配置页显示的一行: 尽量用人话, 不暴露内部细节.
fn status_for_panel(outcome: &stt_update::worker::UpdateOutcome) -> String {
    match outcome {
        stt_update::worker::UpdateOutcome::Applied { tag } => {
            format!("{tag} 已就绪, 重启 Steam 生效")
        }
        stt_update::worker::UpdateOutcome::AppliedPendingRestart { tag } => {
            format!("{tag} 已就绪, 重启 Steam 生效")
        }
        stt_update::worker::UpdateOutcome::Latest => "已是最新版本".to_owned(),
        stt_update::worker::UpdateOutcome::Failed(reason) => {
            format!("更新检查失败 ({reason})")
        }
    }
}

fn set_update_status(text: &str) {
    let slot = UPDATE_STATUS.get_or_init(|| Mutex::new(String::new()));
    let mut guard = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = text.to_owned();
}

fn update_status() -> String {
    let slot = UPDATE_STATUS.get_or_init(|| Mutex::new(String::new()));
    let guard = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.clone()
}

fn community_snapshot_text(state: &CommunitySnapshotState) -> String {
    match state {
        CommunitySnapshotState::Cached { entries } => format!("cached entries={entries}"),
        CommunitySnapshotState::Downloaded { source, entries } => {
            format!("downloaded source={source} entries={entries}")
        }
        CommunitySnapshotState::Unavailable => "unavailable".to_owned(),
    }
}

/// 完整源链: 用户主源 → Community 聚合 → CatMisteam → CaiGamer.
/// Community 成功时内部 enricher 已补 key/token; 外层完整源只在整段失败时跑.
fn with_community_fallback(
    provider: Box<dyn CatalogProvider>,
    steam_root: &Path,
    config: &CatalogSection,
) -> Box<dyn CatalogProvider> {
    Box::new(CatalogProviderChain::new(vec![
        provider,
        Box::new(community_provider(steam_root, config)),
        Box::new(catmisteam_provider(config)),
        Box::new(caigamer_provider(config)),
    ]))
}

fn build_catalog_provider(
    steam_root: &Path,
    config: &CatalogSection,
) -> stt_catalog::CatalogResult<Box<dyn CatalogProvider>> {
    match config.mode {
        CatalogMode::Disabled => Err(CatalogError::Provider {
            provider: "disabled".to_owned(),
            kind: ProviderErrorKind::Unavailable,
            detail: "catalog source is disabled".to_owned(),
        }),
        CatalogMode::Mock => Ok(Box::new(
            MockCatalogProvider::new().with_auto_generate(true),
        )),
        CatalogMode::CustomHttp => {
            let request_options = catalog_http_options(config);
            let options = stt_platform::WinHttpGetOptions {
                timeouts: request_options.timeouts,
                max_body_bytes: request_options.max_response_body_bytes,
            };
            let provider = Box::new(CustomHttpCatalogProvider::new(
                config.url_template.clone(),
                options,
            )?);
            Ok(with_community_fallback(provider, steam_root, config))
        }
        CatalogMode::Lua => {
            let path = ConfigState::default_lua_dir(steam_root).join("catalog.lua");
            let source = std::fs::read_to_string(path).map_err(|_| CatalogError::Provider {
                provider: "lua".to_owned(),
                kind: ProviderErrorKind::Unavailable,
                detail: "config/lua/catalog.lua is unavailable".to_owned(),
            })?;
            let client: Arc<dyn LuaHttpClient> = Arc::new(WinHttpLuaClient {
                options: catalog_http_options(config),
            });
            let provider = Box::new(LuaCatalogProvider::new(source, Some(client))?);
            Ok(with_community_fallback(provider, steam_root, config))
        }
        CatalogMode::Community => Ok(Box::new(CatalogProviderChain::new(vec![
            Box::new(community_provider(steam_root, config)),
            Box::new(catmisteam_provider(config)),
            Box::new(caigamer_provider(config)),
        ]))),
    }
}

fn default_dlc_mode(state: &ConfigState) -> CatalogDlcMode {
    if state.host().catalog.auto_dlc {
        CatalogDlcMode::Full
    } else {
        CatalogDlcMode::GameOnly
    }
}

fn dlc_expand_budget(state: &ConfigState) -> DlcExpandOptions {
    let host = state.host();
    DlcExpandOptions {
        enabled: true,
        max_dlc: host.catalog.max_dlc as usize,
        timeout: std::time::Duration::from_millis(u64::from(host.catalog.dlc_timeout_ms)),
    }
}

fn add_from_config(
    state: &ConfigState,
    steam_root: &Path,
    app_id: AppId,
    dlc_mode: CatalogDlcMode,
) -> stt_config::Result<stt_config::AddToLibraryOutcome> {
    let host = state.host();
    let provider = build_catalog_provider(steam_root, &host.catalog)?;
    let http = catalog_http_get_options(&host.catalog);
    let budget = dlc_expand_budget(state);
    add_to_library_with_mode(
        state,
        steam_root,
        provider.as_ref(),
        app_id,
        dlc_mode,
        budget,
        http,
    )
}

fn list_dlcs_from_config(
    state: &ConfigState,
    steam_root: &Path,
    app_id: AppId,
) -> stt_config::Result<stt_config::DlcListOutcome> {
    let host = state.host();
    let provider = build_catalog_provider(steam_root, &host.catalog)?;
    let http = catalog_http_get_options(&host.catalog);
    Ok(list_related_dlcs(
        steam_root,
        app_id,
        provider.as_ref(),
        http,
        host.catalog.max_dlc as usize,
    ))
}

fn catalog_http_get_options(config: &CatalogSection) -> stt_platform::WinHttpGetOptions {
    let req = catalog_http_options(config);
    stt_platform::WinHttpGetOptions {
        timeouts: req.timeouts,
        max_body_bytes: req.max_response_body_bytes,
    }
}

fn catalog_trace_text(trace: &[CatalogTraceEntry]) -> String {
    trace
        .iter()
        .map(|entry| match entry.outcome {
            CatalogTraceOutcome::Hit => format!("{}:hit", entry.provider),
            CatalogTraceOutcome::Failed(kind) => {
                format!("{}:failed:{kind:?}", entry.provider)
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn catalog_error_text(error: &stt_config::ConfigError) -> String {
    match error {
        stt_config::ConfigError::Catalog(CatalogError::ChainExhausted { trace }) => format!(
            "catalog provider chain exhausted trace={}",
            catalog_trace_text(trace)
        ),
        _ => error.to_string(),
    }
}

/// 调试通道给谁用: 入库按钮和配置页都靠它, 有一个开着就得装 hook.
fn needs_cef_channel(state: &ConfigState) -> bool {
    let tools = state.tools();
    tools.is_enabled(ToolId::CatalogAdd) || tools.is_enabled(ToolId::ConfigUi)
}

/// 本会话的通道名, 给日志和配置页显示.
fn channel_label(use_pipe: bool) -> String {
    if use_pipe {
        "pipe".to_owned()
    } else {
        stt_steamui::cdp_host_port()
    }
}

fn init_host_logger(steam_root: &Path) -> io::Result<()> {
    if HOST_LOGGER.get().is_some() {
        return Ok(());
    }
    let logger = HostLogger::new(&stt_platform::host_log_path(steam_root), "debug")?;
    let _ = HOST_LOGGER.set(logger);
    if HOST_LOGGER.get().is_some() {
        Ok(())
    } else {
        Err(io::Error::other("host logger was not initialized"))
    }
}

fn set_host_log_level(level: &str) -> bool {
    HOST_LOGGER
        .get()
        .map(|logger| logger.set_level(level))
        .unwrap_or(false)
}

fn append_host_log_at(steam_root: &Path, level: HostLogLevel, line: &str) {
    if let Some(logger) = HOST_LOGGER.get() {
        logger.log(steam_root, level, line);
        return;
    }

    // init 前只有测试辅助或极早期错误会走这里, 保留同步兜底.
    let path = stt_platform::host_log_path(steam_root);
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| writeln!(f, "{line}"));
}

fn append_host_log(steam_root: &Path, line: &str) {
    append_host_log_at(steam_root, HostLogLevel::infer_legacy(line), line);
}

/// 初始化: 数据目录, 配置, 扫 lua, 启动日志 writer, 然后阻塞轮询监视.
pub fn run_init(steam_root: &Path) -> std::io::Result<()> {
    let data = stt_platform::ensure_data_dir(steam_root)?;
    init_host_logger(steam_root)?;
    append_host_log(steam_root, "host_init=starting");

    // 抢在 Steam 拉起 steamwebhelper 之前装 CreateProcessW/AsUserW IAT hook.
    // webhelper 在 Steam 启动 ~3s 就被拉起 (登录窗口 00:40:35 vs steam 00:40:32),
    // 而 bootstrap_config (pattern 联网) + reconcile_store_accel 要 4-6s,
    // 排后面每次首拉都错过 (实测 host.log missed_webhelper calls=1 webhelper=0).
    // 先用默认 enable=true 装好; config 就绪后由下方 wait 校正开关 (install 幂等).
    let use_pipe = !stt_platform::cef_pipe_fallback_marker(steam_root).is_file();
    let early = stt_steamui::install_cef_debug_hook(true, use_pipe);
    append_host_log(steam_root, &early.summary_line());

    let state = match bootstrap_config(steam_root) {
        Ok(s) => s,
        Err(e) => {
            append_host_log(
                steam_root,
                &format!(
                    "host_init=failed data_dir={} config_error={e}",
                    data.display()
                ),
            );
            return Ok(());
        }
    };
    let log_level = state.host().log.level;
    if !set_host_log_level(&log_level) {
        append_host_log_at(
            steam_root,
            HostLogLevel::Error,
            &format!("host_init=invalid_log_level value={log_level} fallback=debug"),
        );
    }

    reconcile_store_accel(steam_root, false, None, &state);

    // 上面已幂等装好; 这里校正开关并等 webhelper (或超时).
    let channel_on = needs_cef_channel(&state);
    let cef = stt_steamui::wait_cef_debug_hook(channel_on, use_pipe, Duration::from_millis(2000));

    let lua_report = state.reload_lua_dirs(steam_root);

    let cfg_path = HostConfig::resolve_path(steam_root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(defaults)".into());
    let mut config_lines = tools_enabled_line(&state);
    config_lines.push_str(&catalog_mode_line(&state));
    config_lines.push('\n');

    append_host_log(
        steam_root,
        &format!(
            "host_init=bootstrapped data_dir={} host_config={} {} lua_dirs={} lua_files_ok={} lua_files_err={} owned_count={} rules_epoch={} legacy_data_dir_present={} watch=pending debounce_ms={} poll_ms={}",
            data.display(),
            cfg_path,
            config_lines.trim(),
            lua_report.dirs_scanned,
            lua_report.files_ok,
            lua_report.files_err,
            state.owned_count(),
            state.rules_epoch(),
            stt_platform::legacy_data_dir_exists(steam_root),
            WATCH_DEBOUNCE.as_millis(),
            WATCH_POLL.as_millis(),
        ),
    );

    if lua_report.files_err > 0 {
        for e in &lua_report.errors {
            append_host_log(steam_root, &format!("lua_error={e}"));
        }
    }

    spawn_community_snapshot_refresh(steam_root, &state);
    spawn_update_worker(steam_root, &state);
    log_module_hashes(steam_root);
    let patterns = log_pattern_probe(steam_root);
    let library_ux_report = log_library_ux_plan(steam_root, &state, &patterns);
    // package attach 可能失败/较慢: 先打阶段日志, 再装, 避免 silent hang.
    append_host_log(steam_root, "package=setup begin");
    let package = setup_package_layer(steam_root, &state, &patterns);
    append_host_log(steam_root, "package=setup end");
    #[cfg(feature = "download-request-code")]
    manifest_code::spawn_resolver_worker(steam_root, &state);
    let download = plan_download_layer(steam_root, &state, &patterns);
    match stt_hook::run_harmless_self_test() {
        Ok(n) => append_host_log(steam_root, &format!("hook_self_test=ok calls={n}")),
        Err(e) => append_host_log(steam_root, &format!("hook_self_test=err {e}")),
    }

    if let Ok(inbox) = stt_platform::ensure_inbox_dir(steam_root) {
        append_host_log(
            steam_root,
            &format!(
                "catalog_add=inbox ready path={} (drop *.txt with one app_id per line)",
                inbox.display()
            ),
        );
    }

    // 拖放导入: 在配置面板「脚本目录」页拖入 .lua; 窗口级捕获已不做.
    if state.tools().is_enabled(ToolId::LuaDrop) {
        append_host_log(steam_root, "lua_drop=enabled (panel drop zone on Lua page)");
    } else {
        append_host_log(steam_root, "lua_drop=disabled");
    }

    match stt_platform::write_store_inject_js(steam_root, stt_steamui::STORE_INJECT_JS) {
        Ok(path) => append_host_log(
            steam_root,
            &format!("catalog_add=store_inject ready path={}", path.display()),
        ),
        Err(e) => append_host_log(steam_root, &format!("catalog_add=store_inject err {e}")),
    }

    // 调试端点由 CreateProcessW hook 控制, 不再落 .cef-enable-remote-debugging
    // (ADR 0010): 端口只活在本会话, 且顺手剥掉 Steam 自带的 --remote-allow-origins=*.
    // hook 本身在 init 开头就装好了, 这里只报状态.
    append_host_log(steam_root, &cef.summary_line());
    if stt_platform::cef_remote_debugging_flag_path(steam_root).is_file() {
        append_host_log(
            steam_root,
            "cef_debug=legacy_flag present (delete .cef-enable-remote-debugging; \
             it keeps 8080 open for every Steam session)",
        );
    }
    let details = tool_details(ToolDetailsContext {
        steam_root,
        state: &state,
        library_ux: &library_ux_report,
        package: &package,
        download: &download,
        use_pipe,
        caught: cef.caught_webhelper(),
    });
    spawn_store_cdp_bridge(steam_root, &state, use_pipe, details);
    append_host_log(
        steam_root,
        &format!(
            "config_ui={} channel={} (entry is injected into the client window)",
            if state.tools().is_enabled(ToolId::ConfigUi) {
                "on"
            } else {
                "off"
            },
            channel_label(use_pipe)
        ),
    );
    append_host_log(
        steam_root,
        &format!(
            "catalog_add=store_inject channel={} caught_webhelper={} \
             (store lives in steamwebhelper)",
            if use_pipe {
                "pipe".to_string()
            } else {
                stt_steamui::cdp_host_port()
            },
            cef.caught_webhelper()
        ),
    );

    append_host_log(
        steam_root,
        &format!(
            "status=init complete package={:?} attached={} watch=started",
            package.status,
            stt_steamclient::is_attached()
        ),
    );
    run_watch_loop(steam_root, &state, use_pipe, patterns);
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum CatalogJobSource {
    StoreCdp,
    ConfigRefresh,
    Inbox,
}

impl CatalogJobSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::StoreCdp => "store_cdp",
            Self::ConfigRefresh => "config_refresh",
            Self::Inbox => "inbox",
        }
    }
}

#[derive(Debug, Clone)]
struct CatalogJob {
    app_id: AppId,
    source: CatalogJobSource,
    dlc_mode: CatalogDlcMode,
}

fn queue_catalog_job(sender: &SyncSender<CatalogJob>, job: CatalogJob) -> stt_config::Result<()> {
    sender.try_send(job).map_err(|error| {
        let detail = match error {
            TrySendError::Full(_) => "catalog worker queue is full",
            TrySendError::Disconnected(_) => "catalog worker is unavailable",
        };
        stt_config::ConfigError::Invalid(detail.into())
    })
}

fn spawn_catalog_worker(
    steam_root: &Path,
    state: &ConfigState,
    note: &Arc<Mutex<String>>,
) -> (SyncSender<CatalogJob>, Receiver<String>) {
    const QUEUE_CAPACITY: usize = 32;
    let (jobs_tx, jobs_rx) = mpsc::sync_channel::<CatalogJob>(QUEUE_CAPACITY);
    let (feedback_tx, feedback_rx) = mpsc::channel::<String>();
    let root = steam_root.to_path_buf();
    let state = state.clone();
    let note = Arc::clone(note);

    let spawn = std::thread::Builder::new()
        .name("catalog-worker".into())
        .spawn(move || {
            while let Ok(job) = jobs_rx.recv() {
                match add_from_config(&state, &root, job.app_id, job.dlc_mode.clone()) {
                    Ok(out) => {
                        let applied =
                            on_library_added(&root, &state, job.app_id, &out.owned_apps);
                        let missing_log = missing_log_suffix(&out.missing);
                        let dlc_total = out.dlc.total_added();
                        let dlc_suffix = out.dlc.summary_suffix();
                        append_host_log(
                            &root,
                            &format!(
                                "catalog_add={} result={} source={} app_id={} provider={} trace={} lua={} epoch={} owned={} manifest_files={} dlc_unlock={} dlc_full={} dlc_skip={} dlc_list={} detail={}{missing_log}",
                                if applied.result == CatalogJobResult::Success {
                                    "ok"
                                } else {
                                    "partial"
                                },
                                applied.result.as_str(),
                                job.source.as_str(),
                                job.app_id,
                                out.provider_id,
                                catalog_trace_text(&out.provider_trace),
                                out.lua_path.display(),
                                out.epoch,
                                out.owned_count,
                                out.manifest_files_written,
                                out.dlc.unlock_only.len(),
                                out.dlc.downloadable.len(),
                                out.dlc.skipped.len(),
                                out.dlc.list_source,
                                applied.detail
                            ),
                        );
                        set_shared_note(
                            &note,
                            catalog_job_note(
                                applied.result,
                                job.app_id,
                                &dlc_suffix,
                                &format!(
                                    "已落盘; provider={}; owned={}; {}{}",
                                    out.provider_id,
                                    out.owned_count,
                                    applied.detail,
                                    if out.missing.is_empty() {
                                        String::new()
                                    } else {
                                        format!("; 缺 {}", out.missing.describe())
                                    }
                                ),
                            ),
                        );
                        if matches!(job.source, CatalogJobSource::StoreCdp) {
                            let _ = feedback_tx.send(stt_steamui::store_button_result_js(
                                job.app_id,
                                true,
                                &catalog_button_label(applied.result, job.app_id, dlc_total),
                            ));
                            // 有清单但缺下载数据 (key/token): 商店页弹窗提示,
                            // 免得用户以为能直接下载.
                            if !out.missing.is_empty() {
                                let text = out.missing.describe();
                                let _ = feedback_tx.send(stt_steamui::store_missing_key_warn_js(
                                    job.app_id,
                                    &text,
                                ));
                            }
                        }
                    }
                    Err(error) => {
                        let error_text = catalog_error_text(&error);
                        append_host_log(
                            &root,
                            &format!(
                                "catalog_add=err result={} source={} app_id={} {error_text}",
                                CatalogJobResult::Failure.as_str(),
                                job.source.as_str(),
                                job.app_id
                            ),
                        );
                        set_shared_note(
                            &note,
                            catalog_job_note(
                                CatalogJobResult::Failure,
                                job.app_id,
                                "",
                                &error_text,
                            ),
                        );
                        if matches!(job.source, CatalogJobSource::StoreCdp) {
                            let _ = feedback_tx.send(stt_steamui::store_button_result_js(
                                job.app_id,
                                false,
                                &catalog_button_label(CatalogJobResult::Failure, job.app_id, 0),
                            ));
                        }
                    }
                }
            }
        });

    if let Err(error) = spawn {
        append_host_log(steam_root, &format!("catalog_worker=spawn_err {error}"));
    }
    (jobs_tx, feedback_rx)
}

/// 配置页的宿主侧: 出快照, 收意图, 落盘.
struct HostPanel {
    steam_root: std::path::PathBuf,
    state: ConfigState,
    channel: String,
    /// 最近一次操作结果 (配置保存 / 商店入库), 下一份快照带给页面.
    ///
    /// 用 `Arc<Mutex<_>>`: 商店点击回调与面板快照不在同一条借用链上,
    /// 但必须看到同一条 note, 否则入库成败只进 host.log.
    note: Arc<Mutex<String>>,
    /// 勘察样本只留第一份.
    recon_done: bool,
    /// 各工具的运行状态 (init 时算一次), 给工具中心显示.
    details: stt_config::ToolDetails,
    /// Catalog 拉取只入队, 不阻塞 CDP/面板回调.
    catalog_jobs: SyncSender<CatalogJob>,
    /// 受管 app 的缓存与它对应的 rules epoch —— 算一次要扫目录, 别每轮来.
    managed: Vec<u32>,
    managed_names: std::collections::BTreeMap<u32, String>,
    managed_epoch: Option<u64>,
}

fn set_shared_note(note: &Arc<Mutex<String>>, text: impl Into<String>) {
    // poison 也恢复: note 只是 UI 文案, 丢一次旧值比卡死回调划算.
    let mut g = note
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *g = text.into();
}

fn shared_note(note: &Arc<Mutex<String>>) -> String {
    note.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// 进程内唯一 note 槽: 面板 / catalog / 导入都写这儿, 配置页才能看见.
fn global_note() -> Arc<Mutex<String>> {
    SHARED_NOTE
        .get_or_init(|| Arc::new(Mutex::new(String::new())))
        .clone()
}

impl HostPanel {
    /// 受管列表; rules 没变就用上一次的.
    fn managed_cached(&mut self) -> &[u32] {
        let epoch = self.state.rules_epoch();
        if self.managed_epoch != Some(epoch) {
            self.managed = stt_config::managed_apps(&self.state, &self.steam_root);
            self.managed_names = stt_config::app_names(&self.steam_root, &self.managed);
            self.managed_epoch = Some(epoch);
        }
        &self.managed
    }

    /// 针对某个 app 的意图: 刷新 = 重新拉一次清单覆盖 lua; 移除 = 删掉那份 lua.
    fn apply_app_intent(&self, intent: &ConfigIntent, app_id: u32) -> stt_config::Result<String> {
        match intent {
            ConfigIntent::RefreshApp(_) => {
                set_shared_note(&self.note, format!("正在刷新 {app_id}"));
                queue_catalog_job(
                    &self.catalog_jobs,
                    CatalogJob {
                        app_id,
                        source: CatalogJobSource::ConfigRefresh,
                        dlc_mode: default_dlc_mode(&self.state),
                    },
                )?;
                Ok(format!("已排队刷新 {app_id}"))
            }
            ConfigIntent::RemoveApp(_) => {
                let out = remove_from_library(&self.state, &self.steam_root, app_id)?;
                on_library_removed(&self.steam_root, &self.state, app_id);
                Ok(format!("已移除 {app_id} (owned={})", out.owned_count))
            }
            _ => Err(stt_config::ConfigError::Invalid("不是 app 意图".into())),
        }
    }

    /// 面板拖入的 lua 文本导入.
    fn apply_import_intent(&self, intent: &ConfigIntent) -> stt_config::Result<String> {
        let ConfigIntent::ImportLua { files } = intent else {
            return Err(stt_config::ConfigError::Invalid("不是导入意图".into()));
        };
        if !self.state.tools().is_enabled(ToolId::LuaDrop) {
            return Err(stt_config::ConfigError::Invalid(
                "lua_drop tool is disabled".into(),
            ));
        }
        set_shared_note(&self.note, "正在导入本地 lua…");
        let report = import_local_texts(&self.state, &self.steam_root, files)?;
        append_host_log(
            &self.steam_root,
            &format!("lua_drop=panel {}", report.summary_line()),
        );
        if !report.lua_written.is_empty() {
            // 面板线程没有 pattern store; reload 后走与手改 lua 相同的热重载路径
            // (watch 循环会在下一轮 rescan 再对齐 package). 这里先触库 present.
            for &app_id in &report.apps {
                library_ux().on_rules_app_present(app_id);
            }
            // 立刻对齐 package/configured, 不等 watch.
            sync_download_runtime(&self.state);
            sync_configured_from_state(&self.state);
            if let Some(q) = license_queue() {
                let package_ids = package_ids_from_state(&self.state);
                q.reconcile_owned(package_ids.iter().copied());
                if q.pending_add_len() > 0 || q.pending_remove_len() > 0 {
                    let plan = stt_steamclient::notify_license_changed(&q);
                    append_host_log(&self.steam_root, &plan.summary_line());
                }
            }
            self.state
                .with_rules(|rules| library_ux().sync_from_rules(rules));
        }
        let note = report.note_line();
        set_shared_note(&self.note, note.clone());
        Ok(note)
    }
}

impl stt_steamui::PanelBridge for HostPanel {
    fn enabled(&mut self) -> bool {
        self.state.tools().is_enabled(ToolId::ConfigUi)
    }

    fn snapshot(&mut self) -> Option<ConfigSnapshot> {
        let facts = stt_config::HostFacts {
            tool_details: self.details.clone(),
            update_status: update_status(),
            managed: self.managed_cached().to_vec(),
            managed_names: self.managed_names.clone(),
        };
        let note = shared_note(&self.note);
        Some(ConfigSnapshot::new(
            &self.state,
            &self.steam_root,
            &self.channel,
            &note,
            &facts,
        ))
    }

    fn managed_apps(&mut self) -> Vec<u32> {
        self.managed_cached().to_vec()
    }

    fn on_intents(&mut self, intents: &[ConfigIntent]) {
        for intent in intents {
            let was_store_accel = self.state.tools().is_enabled(ToolId::StoreAccel);
            let previous_store_accel = self.state.host().store_accel;
            // app / 导入意图不写 toml, 只有宿主这儿能处理.
            let done = if intent.is_import() {
                self.apply_import_intent(intent)
            } else {
                match intent.app_target() {
                    Some(app_id) => self.apply_app_intent(intent, app_id),
                    None => apply_intent(&self.state, &self.steam_root, intent),
                }
            };
            match done {
                Ok(done) => {
                    reconcile_store_accel(
                        &self.steam_root,
                        was_store_accel,
                        Some(&previous_store_accel),
                        &self.state,
                    );
                    append_host_log(&self.steam_root, &format!("config_ui=saved {done}"));
                    // 导入/刷新自己已经写过 note; 其它意图统一 "已保存".
                    if !matches!(
                        intent,
                        ConfigIntent::RefreshApp(_) | ConfigIntent::ImportLua { .. }
                    ) {
                        set_shared_note(&self.note, format!("已保存 {done}"));
                    }
                }
                Err(e) => {
                    append_host_log(&self.steam_root, &format!("config_ui=err {e}"));
                    set_shared_note(&self.note, format!("失败: {e}"));
                }
            }
        }
    }

    fn on_recon(&mut self, sample: &str) {
        // 每份都写 (后一份是界面渲染完之后取的, 更有用); 日志只提一次.
        let path = stt_platform::data_dir(&self.steam_root).join("ui-recon.txt");
        if std::fs::write(&path, clip_recon_sample(sample)).is_ok() && !self.recon_done {
            self.recon_done = true;
            append_host_log(
                &self.steam_root,
                &format!("config_ui=recon path={}", path.display()),
            );
        }
    }
}

/// recon 样本最多留 4 KiB 并去掉控制字符: 页面内容不可信,
/// 别让一份大样本把磁盘占满或把控制字符写进文件.
fn clip_recon_sample(sample: &str) -> String {
    const MAX_RECON_BYTES: usize = 4096;
    let mut clipped = String::new();
    for ch in sample.chars().filter(|ch| !ch.is_control()) {
        if clipped.len() + ch.len_utf8() > MAX_RECON_BYTES {
            break;
        }
        clipped.push(ch);
    }
    clipped
}

/// 后台: CEF CDP 向商店页注入按钮并取回点击, 顺带把配置页挂进客户端界面.
///
/// `use_pipe` 时先试无端口的管道通道; 它明确走不通才落标记文件并回退到端口,
/// 这样最多一个会话入库不可用, 不会永久卡死.
fn spawn_store_cdp_bridge(
    steam_root: &Path,
    state: &ConfigState,
    use_pipe: bool,
    details: stt_config::ToolDetails,
) {
    let root = steam_root.to_path_buf();
    let state = state.clone();
    let _ = std::thread::Builder::new()
        .name("store-cdp".into())
        .spawn(move || {
            let note = global_note();
            let (catalog_jobs, catalog_feedback) = spawn_catalog_worker(&root, &state, &note);
            let mut panel = HostPanel {
                steam_root: root.clone(),
                state: state.clone(),
                channel: channel_label(use_pipe),
                note: Arc::clone(&note),
                recon_done: false,
                details,
                catalog_jobs: catalog_jobs.clone(),
                managed: Vec::new(),
                managed_names: std::collections::BTreeMap::new(),
                managed_epoch: None,
            };
            // 短脚本: 大 STORE_INJECT_JS 在 CEF evaluate 上易挂起.
            // 工具关掉就换成摘按钮的脚本, 让开关当场看得见.
            let mut make_js = || {
                // 商店按钮「已入库」依赖 managed 列表; 直接扫盘, 不借 panel
                // (panel 同时要交给 loop 作 PanelBridge).
                let managed = stt_config::managed_apps(&state, &root);
                let mut script = if state.tools().is_enabled(ToolId::CatalogAdd) {
                    let mut s = stt_steamui::managed_apps_js(&managed);
                    s.push_str(";\n");
                    s.push_str(&stt_steamui::cdp_store_inject_js());
                    s
                } else {
                    stt_steamui::store_teardown_js()
                };
                while let Ok(feedback) = catalog_feedback.try_recv() {
                    script.push_str(";\n");
                    script.push_str(&feedback);
                }
                                script
            };
            // 点击只入有界队列; HTTP/落盘由 catalog worker 执行.
            // select_dlc/list 在回调里同步拉列表并回写 picker.
            let mut on_app = |pending: StorePendingJob| -> Option<String> {
                let app_id = pending.app_id;
                if pending.is_dlc_list() {
                    match list_dlcs_from_config(&state, &root, app_id) {
                        Ok(list) => {
                            let items: Vec<serde_json::Value> = list
                                .items
                                .iter()
                                .map(|it| serde_json::json!({"id": it.app_id, "name": it.name}))
                                .collect();
                            let items_json =
                                serde_json::to_string(&items).unwrap_or_else(|_| "[]".into());
                            append_host_log(
                                &root,
                                &format!(
                                    "catalog_dlc_list=ok app_id={app_id} count={} source={} truncated={}",
                                    list.items.len(),
                                    list.list_source,
                                    list.truncated
                                ),
                            );
                            Some(stt_steamui::store_dlc_picker_js(
                                app_id,
                                &items_json,
                                list.truncated,
                            ))
                        }
                        Err(error) => {
                            append_host_log(
                                &root,
                                &format!("catalog_dlc_list=err app_id={app_id} {error}"),
                            );
                            Some(stt_steamui::store_dlc_picker_error_js(app_id, "DLC 列表失败"))
                        }
                    }
                } else {
                    let dlc_mode = match pending.mode.as_str() {
                        "game_only" => CatalogDlcMode::GameOnly,
                        "select_dlc" => CatalogDlcMode::Selected(pending.dlc_ids.clone()),
                        _ => CatalogDlcMode::Full,
                    };
                    let job = CatalogJob {
                        app_id,
                        source: CatalogJobSource::StoreCdp,
                        dlc_mode,
                    };
                    match queue_catalog_job(&catalog_jobs, job) {
                        Ok(()) => Some(stt_steamui::store_button_result_js(
                            app_id, false, "正在拉取",
                        )),
                        Err(error) => {
                            append_host_log(
                                &root,
                                &format!(
                                    "catalog_add=queue_err source=store_cdp app_id={app_id} {error}"
                                ),
                            );
                            set_shared_note(&note, format!("失败: 入库 {app_id}: {error}"));
                            Some(stt_steamui::store_button_result_js(
                                app_id, false, "队列不可用",
                            ))
                        }
                    }
                }
            };
            let mut on_log = |line: String| append_host_log(&root, &line);

            if use_pipe {
                // webhelper 起来 + CEF 初始化完要点时间, 给够 30s.
                let ok = stt_steamui::run_store_pipe_loop(
                    Duration::from_millis(600),
                    &mut make_js,
                    &mut on_app,
                    &mut on_log,
                    Duration::from_secs(30),
                    Some(&mut panel),
                );
                if ok {
                    return; // 通道好使, 循环自己会长驻; 走到这说明 Steam 要退了
                }
                // 明确不可用: 记下来, 下次直接用端口, 免得每次都赔一个会话.
                let marker = stt_platform::cef_pipe_fallback_marker(&root);
                let _ = std::fs::write(
                    &marker,
                    "CDP over --remote-debugging-pipe did not come up; \
                     delete this file to retry pipe mode.\n",
                );
                append_host_log(
                    &root,
                    &format!(
                        "cef_debug=pipe_fallback marker={} (restart Steam to use the port channel)",
                        marker.display()
                    ),
                );
                return;
            }

            stt_steamui::run_store_cdp_loop_with_js(
                Duration::from_millis(600),
                &mut make_js,
                &mut on_app,
                &mut on_log,
                Some(&mut panel),
            );
        });
}

fn log_module_hashes(steam_root: &Path) {
    for name in ["steamui.dll", "steamclient64.dll"] {
        let path = steam_root.join(name);
        if !path.is_file() {
            append_host_log(steam_root, &format!("sha256_{name}=missing"));
            continue;
        }
        match stt_platform::sha256_file(&path) {
            Ok(h) => append_host_log(steam_root, &format!("sha256_{name}={h}")),
            Err(e) => append_host_log(steam_root, &format!("sha256_{name}=err {e}")),
        }
    }
}

/// 更新 pattern 时随发布流程改这个 commit (git ls-remote 取 main 最新),
/// 别直接指 main: manifest 没有完整性校验, 钉死 commit 才算钉死信任根.
const PATTERN_MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/SteamToolsProject/SteamTools-Patterns/a17f3a98db60b2f8427125e673aa0d2e9b5fef39/manifests/stable.json";
const PATTERN_RAW_BASE_URL: &str =
    "https://raw.githubusercontent.com/SteamToolsProject/SteamTools-Patterns/main";

#[derive(Debug, thiserror::Error)]
enum PatternFetchError {
    #[error("pattern HTTP request failed: {0}")]
    Http(#[from] stt_platform::HttpError),
    #[error("pattern metadata is invalid: {0}")]
    Metadata(#[from] stt_metadata::MetadataError),
    #[error("pattern response is not UTF-8: {0}")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("{resource} HTTP status {status}")]
    UnexpectedStatus { resource: &'static str, status: u16 },
    #[error("manifest channel is {actual}, expected stable")]
    UnexpectedChannel { actual: String },
    #[error("pattern response has no entries")]
    EmptyPattern,
    #[error("pattern cache write failed: {0}")]
    Cache(#[from] io::Error),
}

fn pattern_http_options() -> stt_platform::WinHttpGetOptions {
    stt_platform::WinHttpGetOptions {
        timeouts: stt_platform::WinHttpTimeouts {
            resolve_ms: 2_000,
            connect_ms: 2_000,
            send_ms: 2_000,
            receive_ms: 3_000,
        },
        max_body_bytes: 512 * 1024,
    }
}

fn fetch_pattern_manifest() -> Result<stt_metadata::PatternManifest, PatternFetchError> {
    let response = stt_platform::winhttp_get(PATTERN_MANIFEST_URL, pattern_http_options())?;
    if !(200..300).contains(&response.status) {
        return Err(PatternFetchError::UnexpectedStatus {
            resource: "manifest",
            status: response.status,
        });
    }
    let manifest = stt_metadata::PatternManifest::parse(&response.body)?;
    if manifest.channel() != "stable" {
        return Err(PatternFetchError::UnexpectedChannel {
            actual: manifest.channel().to_owned(),
        });
    }
    Ok(manifest)
}

fn fetch_remote_pattern(
    steam_root: &Path,
    component: &str,
    dll: &str,
    sha: &str,
    manifest: &stt_metadata::PatternManifest,
) -> Result<bool, PatternFetchError> {
    let Some(relative_path) = manifest.matching_pattern(component, dll, sha)? else {
        return Ok(false);
    };
    let url = format!("{PATTERN_RAW_BASE_URL}/{relative_path}");
    let response = stt_platform::winhttp_get(&url, pattern_http_options())?;
    if !(200..300).contains(&response.status) {
        return Err(PatternFetchError::UnexpectedStatus {
            resource: "pattern",
            status: response.status,
        });
    }
    let text = std::str::from_utf8(&response.body)?;
    let map = stt_metadata::PatternMap::parse_str(component, text)?;
    if map.is_empty() {
        return Err(PatternFetchError::EmptyPattern);
    }

    let path = stt_platform::pattern_cache_file(steam_root, component, sha);
    write_pattern_cache(&path, response.body.as_slice())?;
    Ok(true)
}

fn write_pattern_cache(path: &Path, body: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pattern path has no parent"))?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "pattern path has no UTF-8 filename",
            )
        })?;
    std::fs::create_dir_all(parent)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let temporary = parent.join(format!(".{name}.{}-{nonce}.tmp", std::process::id()));
    let write_result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(body)?;
        file.sync_all()
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }

    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_file() => {
            let _ = std::fs::remove_file(&temporary);
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}

fn log_pattern_probe(steam_root: &Path) -> stt_metadata::PatternStore {
    let mut store = stt_metadata::PatternStore::new();
    let mut remote_manifest: Option<Result<stt_metadata::PatternManifest, PatternFetchError>> =
        None;
    for component in ["steamui", "steamclient"] {
        let dll = if component == "steamui" {
            "steamui.dll"
        } else {
            "steamclient64.dll"
        };
        let path = steam_root.join(dll);
        let sha = match stt_platform::sha256_file(&path) {
            Ok(h) => h,
            Err(_) => {
                append_host_log(
                    steam_root,
                    &format!("pattern_{component}=skip (dll hash unavailable)"),
                );
                continue;
            }
        };

        let primary = stt_platform::pattern_cache_file(steam_root, component, &sha);
        if !primary.is_file() {
            match remote_manifest.get_or_insert_with(fetch_pattern_manifest) {
                Ok(manifest) => {
                    match fetch_remote_pattern(steam_root, component, dll, &sha, manifest) {
                        Ok(true) => append_host_log(
                            steam_root,
                            &format!(
                                "pattern_{component}=remote path={} build={} sha={sha}",
                                primary.display(),
                                manifest.steam_build()
                            ),
                        ),
                        Ok(false) => append_host_log(
                            steam_root,
                            &format!("pattern_{component}=remote miss sha={sha}"),
                        ),
                        Err(error) => append_host_log(
                            steam_root,
                            &format!("pattern_{component}=remote unavailable ({error}) sha={sha}"),
                        ),
                    }
                }
                Err(error) => append_host_log(
                    steam_root,
                    &format!("pattern_{component}=remote unavailable ({error}) sha={sha}"),
                ),
            }
        }

        let legacy = stt_platform::legacy_pattern_cache_file(steam_root, component, &sha);
        match store.load_with_fallback(component, &primary, Some(&legacy)) {
            Ok(loaded) => append_host_log(
                steam_root,
                &format!(
                    "pattern_{component}=loaded entries={} path={} legacy={}",
                    store.map(component).map(|m| m.len()).unwrap_or(0),
                    loaded.path.display(),
                    loaded.legacy
                ),
            ),
            Err(e) => append_host_log(
                steam_root,
                &format!("pattern_{component}=disabled ({e}) sha={sha}"),
            ),
        }
    }
    store
}

fn log_library_ux_plan(
    steam_root: &Path,
    state: &ConfigState,
    patterns: &stt_metadata::PatternStore,
) -> stt_steamui::LibraryUxInstallReport {
    let tools = state.tools();
    let ux = library_ux();
    // 受管集合与购买时间同步给状态机 (RunFrame/FillIn 只读它).
    state.with_rules(|rules| {
        ux.sync_from_rules(rules);
        for app_id in rules.owned_iter() {
            ux.on_rules_app_present(app_id);
        }
    });
    let report = stt_steamui::try_install_library_detours(&tools, patterns, ux);
    append_host_log(steam_root, &report.summary_line());
    if let Some(detail) = &report.detail {
        append_host_log(steam_root, &format!("library_ux=detail {detail}"));
    }
    report
}

/// 注册 LicenseQueue / 配置集, 尝试 attach package hooks, 打日志.
fn setup_package_layer(
    steam_root: &Path,
    state: &ConfigState,
    patterns: &stt_metadata::PatternStore,
) -> stt_steamclient::PackageInstallReport {
    let queue = Arc::new(LicenseQueue::new());
    let configured = Arc::new(RwLock::new(HashSet::new()));
    // OST InitFakeLicense: seed app + depot ids, 不只 owned 主 app.
    let package_ids = package_ids_from_state(state);
    {
        let mut g = configured
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.extend(package_ids.iter().copied());
    }
    queue.seed_injected_from_owned(package_ids.iter().copied());

    let _ = LICENSE_QUEUE.set(Arc::clone(&queue));
    let _ = CONFIGURED_APPS.set(Arc::clone(&configured));
    stt_steamclient::register_runtime(Arc::clone(&queue), Arc::clone(&configured));
    stt_steamclient::set_ui_action_handler(apply_ui_license_action);
    // 已与 runtime 共享 configured Arc, 只填本地锁即可 (勿再嵌套 set_configured_apps).
    append_host_log(
        steam_root,
        &format!(
            "package=license_queue seeded_injected={} configured={}",
            queue.injected_len(),
            configured
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
        ),
    );

    append_host_log(steam_root, "package=try_install begin");
    let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        stt_steamclient::try_install_package_hooks(&state.tools(), patterns)
    }));
    let report = match report {
        Ok(r) => r,
        Err(payload) => {
            let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_owned()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_owned()
            };
            append_host_log(steam_root, &format!("package=try_install PANIC {msg}"));
            stt_steamclient::plan_package_install(&state.tools(), patterns, "steamclient")
                .with_detail(format!("panic during attach: {msg}"))
        }
    };
    append_host_log(steam_root, &report.summary_line());
    if let Some(d) = report.attach_detail() {
        append_host_log(steam_root, &format!("package=detail {d}"));
    }
    if stt_steamclient::is_attached() {
        let (checks, forges) = stt_steamclient::hook_stats();
        append_host_log(
            steam_root,
            &format!(
                "package=hooks attached (CheckAppOwnership+GetPackageInfo) \
                 check_hits={checks} forge_hits={forges}"
            ),
        );
    } else {
        append_host_log(
            steam_root,
            "package=hooks not attached (logic-only or failed)",
        );
    }
    report
}

fn download_env_enabled(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => {
            let value = value.trim().to_ascii_lowercase();
            !matches!(value.as_str(), "0" | "off" | "false" | "no")
        }
        Err(_) => true,
    }
}

fn plan_download_layer(
    steam_root: &Path,
    state: &ConfigState,
    patterns: &stt_metadata::PatternStore,
) -> stt_steamclient::DownloadKitReport {
    sync_download_runtime(state);
    let report = build_download_report(state, patterns);
    #[cfg(any(
        feature = "download-manifest",
        feature = "download-key",
        feature = "download-token",
        feature = "download-request-code"
    ))]
    let report = {
        let mut report = report;
        #[cfg(feature = "download-manifest")]
        stt_steamclient::try_install_manifest_hook(&mut report, patterns);
        #[cfg(feature = "download-key")]
        stt_steamclient::try_install_depot_key_hook(&mut report, patterns);
        #[cfg(feature = "download-token")]
        stt_steamclient::try_install_access_token_hook(&mut report, patterns);
        #[cfg(feature = "download-request-code")]
        stt_steamclient::try_install_manifest_code_hooks(&mut report, patterns);
        report
    };
    append_host_log(
        steam_root,
        &format!("download_kit {}", report.summary_line()),
    );
    report
}

fn build_download_report(
    state: &ConfigState,
    patterns: &stt_metadata::PatternStore,
) -> stt_steamclient::DownloadKitReport {
    let request_code = matches!(
        state.host().manifest.url.as_str(),
        "opensteamtool" | "steamrun" | "wudrm"
    );
    let data = state.with_rules(|rules| stt_steamclient::DownloadDataAvailability {
        manifest: rules.has_manifest_overrides(),
        key: rules.has_depot_keys(),
        token: has_configured_access_token(rules),
        request_code,
    });
    let switches = stt_steamclient::DownloadRuntimeSwitches {
        manifest: download_env_enabled("STEAMTOOLS_DOWNLOAD_MANIFEST"),
        key: download_env_enabled("STEAMTOOLS_DOWNLOAD_KEY"),
        token: download_env_enabled("STEAMTOOLS_DOWNLOAD_TOKEN"),
        request_code: download_env_enabled("STEAMTOOLS_DOWNLOAD_REQUEST_CODE"),
    };
    stt_steamclient::plan_download_kit(
        &state.tools(),
        patterns,
        "steamclient",
        stt_steamclient::DownloadFeatureSet::compiled(),
        switches,
        data,
    )
}

fn has_configured_access_token(rules: &AppRules) -> bool {
    rules
        .owned_iter()
        .any(|app_id| rules.access_token(app_id).is_some_and(|token| token != 0))
}

#[cfg(any(
    feature = "download-manifest",
    feature = "download-key",
    feature = "download-token",
    feature = "download-request-code"
))]
#[derive(Debug, Default)]
struct DownloadRuntimeSnapshot {
    #[cfg(feature = "download-manifest")]
    manifests: std::collections::HashMap<stt_core::DepotId, stt_core::ManifestOverride>,
    #[cfg(feature = "download-key")]
    keys: std::collections::HashMap<stt_core::DepotId, String>,
    #[cfg(feature = "download-token")]
    tokens: std::collections::HashMap<AppId, u64>,
    #[cfg(feature = "download-request-code")]
    request_code_depots: HashSet<stt_core::DepotId>,
}

#[cfg(any(
    feature = "download-manifest",
    feature = "download-key",
    feature = "download-token",
    feature = "download-request-code"
))]
fn capture_download_runtime_snapshot(
    state: &ConfigState,
    switches: stt_steamclient::DownloadRuntimeSwitches,
) -> DownloadRuntimeSnapshot {
    let tool_enabled = state.tools().is_enabled(ToolId::DownloadKit);
    #[cfg(feature = "download-manifest")]
    let manifest_enabled = tool_enabled && switches.manifest;
    #[cfg(feature = "download-key")]
    let key_enabled = tool_enabled && switches.key;
    #[cfg(feature = "download-token")]
    let token_enabled = tool_enabled && switches.token;
    #[cfg(feature = "download-request-code")]
    let request_code_enabled = tool_enabled && switches.request_code;

    state.with_rules(|rules| DownloadRuntimeSnapshot {
        #[cfg(feature = "download-manifest")]
        manifests: if manifest_enabled {
            rules
                .manifest_overrides_iter()
                .map(|(depot_id, over)| (depot_id, over.clone()))
                .collect()
        } else {
            std::collections::HashMap::new()
        },
        #[cfg(feature = "download-key")]
        keys: if key_enabled {
            rules
                .depot_keys_iter()
                .map(|(depot_id, key)| (depot_id, key.to_owned()))
                .collect()
        } else {
            std::collections::HashMap::new()
        },
        #[cfg(feature = "download-token")]
        tokens: if token_enabled {
            rules
                .owned_iter()
                .filter_map(|app_id| {
                    rules
                        .access_token(app_id)
                        .filter(|token| *token != 0)
                        .map(|token| (app_id, token))
                })
                .collect()
        } else {
            std::collections::HashMap::new()
        },
        #[cfg(feature = "download-request-code")]
        request_code_depots: if request_code_enabled {
            rules
                .owned_iter()
                .flat_map(|app_id| rules.app_depots(app_id).iter().copied())
                .collect()
        } else {
            HashSet::new()
        },
    })
}

fn sync_download_runtime(state: &ConfigState) {
    #[cfg(any(
        feature = "download-manifest",
        feature = "download-key",
        feature = "download-token",
        feature = "download-request-code"
    ))]
    let snapshot = capture_download_runtime_snapshot(
        state,
        stt_steamclient::DownloadRuntimeSwitches {
            manifest: download_env_enabled("STEAMTOOLS_DOWNLOAD_MANIFEST"),
            key: download_env_enabled("STEAMTOOLS_DOWNLOAD_KEY"),
            token: download_env_enabled("STEAMTOOLS_DOWNLOAD_TOKEN"),
            request_code: download_env_enabled("STEAMTOOLS_DOWNLOAD_REQUEST_CODE"),
        },
    );

    #[cfg(feature = "download-manifest")]
    stt_steamclient::replace_manifest_overrides(snapshot.manifests);

    #[cfg(feature = "download-key")]
    {
        let report = stt_steamclient::replace_depot_keys(snapshot.keys);
        // accepted/rejected 不进常规日志 (热路径); snapshot 长度看 download_key_stats.
        let _ = report;
    }

    #[cfg(feature = "download-token")]
    let _ = stt_steamclient::replace_access_tokens(snapshot.tokens);

    #[cfg(feature = "download-request-code")]
    let _ = stt_steamclient::replace_manifest_code_depots(snapshot.request_code_depots);

    #[cfg(not(any(
        feature = "download-manifest",
        feature = "download-key",
        feature = "download-token",
        feature = "download-request-code"
    )))]
    let _ = state;
}

#[cfg(any(
    feature = "download-manifest",
    feature = "download-key",
    feature = "download-token",
    feature = "download-request-code"
))]
fn download_hook_rearm_pending() -> bool {
    #[cfg(feature = "download-manifest")]
    if !stt_steamclient::is_manifest_hook_attached() {
        return true;
    }
    #[cfg(feature = "download-key")]
    if !stt_steamclient::is_depot_key_hook_attached() {
        return true;
    }
    #[cfg(feature = "download-token")]
    if !stt_steamclient::is_access_token_hook_attached() {
        return true;
    }
    #[cfg(feature = "download-request-code")]
    if !stt_steamclient::is_manifest_code_hook_attached() {
        return true;
    }
    false
}

struct ToolDetailsContext<'a> {
    steam_root: &'a Path,
    state: &'a ConfigState,
    library_ux: &'a stt_steamui::LibraryUxInstallReport,
    package: &'a stt_steamclient::PackageInstallReport,
    download: &'a stt_steamclient::DownloadKitReport,
    use_pipe: bool,
    caught: bool,
}

/// 各工具此刻的运行状态 —— 开着不等于跑起来了, 这些原来只进 host.log.
fn tool_details(context: ToolDetailsContext<'_>) -> stt_config::ToolDetails {
    let ToolDetailsContext {
        steam_root,
        state,
        library_ux,
        package,
        download,
        use_pipe,
        caught,
    } = context;
    let tools = state.tools();
    let catalog = match state.host().catalog.mode {
        CatalogMode::Disabled => "Catalog 已禁用",
        CatalogMode::CustomHttp => "Catalog: CustomHttp",
        CatalogMode::Lua => "Catalog: Lua (config/lua/catalog.lua)",
        CatalogMode::Community => "Catalog: Community 多源聚合",
        CatalogMode::Mock => "Catalog: Mock 开发模式",
    };
    let mut d = stt_config::ToolDetails::new();
    d.insert(
        ToolId::CatalogAdd.as_str(),
        if !tools.is_enabled(ToolId::CatalogAdd) {
            "已关闭, 商店页不挂入库按钮".to_owned()
        } else if caught {
            format!(
                "{catalog}; 经 {} 注入商店页; {}",
                channel_label(use_pipe),
                package.detail_for_ui()
            )
        } else {
            format!(
                "{catalog}; 调试通道没截到 steamwebhelper; {}",
                package.detail_for_ui()
            )
        },
    );
    d.insert(
        ToolId::LibraryUx.as_str(),
        match library_ux.status {
            stt_steamui::LibraryUxInstallStatus::Disabled => "已关闭".to_owned(),
            stt_steamui::LibraryUxInstallStatus::PatternMissing => {
                "缺 steamui pattern, 本层已降级".to_owned()
            }
            stt_steamui::LibraryUxInstallStatus::SymbolsMissing => {
                format!("缺符号: {}", library_ux.missing.join(","))
            }
            stt_steamui::LibraryUxInstallStatus::LogicOnly => {
                format!(
                    "状态机就绪 ({} 个符号), 业务 detour 未挂",
                    library_ux.resolved.len()
                )
            }
            stt_steamui::LibraryUxInstallStatus::HooksAttached => "detour 已挂上".to_owned(),
        },
    );
    d.insert(
        ToolId::ConfigUi.as_str(),
        format!("就是这个界面, 经 {}", channel_label(use_pipe)),
    );
    d.insert(ToolId::DownloadKit.as_str(), download.detail_for_ui());
    d.insert(
        ToolId::StoreAccel.as_str(),
        store_accel_detail(steam_root, state),
    );
    d.insert(
        ToolId::LuaDrop.as_str(),
        if !tools.is_enabled(ToolId::LuaDrop) {
            "已关闭, 面板拖入不会导入".to_owned()
        } else {
            "已启用: 在「脚本目录」页拖入 .lua 文件".to_owned()
        },
    );
    d
}

fn store_accel_runtime_active(port: u16) -> bool {
    store_accel_runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .active
        .as_ref()
        .is_some_and(|instance| instance.port == port)
}

fn store_accel_detail(_steam_root: &Path, state: &ConfigState) -> String {
    if !state.tools().is_enabled(ToolId::StoreAccel) {
        return "已关闭".to_owned();
    }
    let config = state.host().store_accel;
    match config.egress {
        StoreAccelEgress::Disabled => {
            "degraded: 未配置出口, 不会修改系统 PAC".to_owned()
        }
        StoreAccelEgress::DirectDns if !store_accel_runtime_active(config.listen_port) => format!(
            "degraded: DLL 内代理线程未运行; local_direct: 直连 DNS 兼容模式, 监听 127.0.0.1:{}",
            config.listen_port
        ),
        StoreAccelEgress::DirectDns => {
            "local_direct: 直连 DNS 兼容模式, 当前网络可能不稳定".to_owned()
        }
        StoreAccelEgress::LocalCdn
            if !store_accel_runtime_active(config.listen_port)
                && !config.clash_fallback.is_empty() =>
        {
            format!(
                "degraded: DLL 内代理线程未运行; local_direct: 本地 CDN/DNS 优选; clash_fallback: 已配置; 监听 127.0.0.1:{}",
                config.listen_port
            )
        }
        StoreAccelEgress::LocalCdn if !store_accel_runtime_active(config.listen_port) => format!(
            "degraded: DLL 内代理线程未运行; local_direct: 本地 CDN/DNS 优选, 监听 127.0.0.1:{}",
            config.listen_port
        ),
        StoreAccelEgress::LocalCdn if config.clash_fallback.is_empty() => {
            format!(
                "local_direct: 本地 CDN/DNS 优选已配置, 动态请求失败后无 Clash 回退; 监听 127.0.0.1:{}",
                config.listen_port
            )
        }
        StoreAccelEgress::LocalCdn => format!(
            "local_direct: 本地 CDN/DNS 优先; clash_fallback: 动态和必要静态请求失败最多回退一次; 监听 127.0.0.1:{}",
            config.listen_port
        ),
        StoreAccelEgress::HttpConnect if !store_accel_runtime_active(config.listen_port) => {
            format!(
                "degraded: DLL 内代理线程未运行; user_connect: 自有中继已配置, 监听 127.0.0.1:{}",
                config.listen_port
            )
        }
        StoreAccelEgress::HttpConnect => format!(
            "user_connect: 自有中继已配置, DLL 内 PAC + CONNECT 正在监听 127.0.0.1:{}",
            config.listen_port
        ),
    }
}

/// 在 DLL 内按工具开关启停商店访问线程.
fn reconcile_store_accel(
    steam_root: &Path,
    was_enabled: bool,
    previous: Option<&StoreAccelSection>,
    state: &ConfigState,
) {
    let host = state.host();
    let enabled = state.tools().is_enabled(ToolId::StoreAccel);
    let config_changed = previous.is_some_and(|old| old != &host.store_accel);
    if !enabled && !was_enabled {
        return;
    }
    if !enabled {
        stop_store_accel(
            steam_root,
            previous.map_or(host.store_accel.listen_port, |old| old.listen_port),
        );
        return;
    }
    if was_enabled && !config_changed {
        return;
    }
    if was_enabled {
        stop_store_accel(
            steam_root,
            previous.map_or(host.store_accel.listen_port, |old| old.listen_port),
        );
    }
    if host.store_accel.egress == StoreAccelEgress::Disabled {
        append_host_log(steam_root, "store_accel=not_configured egress=disabled");
        return;
    }
    start_store_accel(steam_root, &host.store_accel);
}

fn start_store_accel(steam_root: &Path, config: &StoreAccelSection) {
    let stop = Arc::new(AtomicBool::new(false));
    let port = config.listen_port;
    let egress = store_accel_egress_name(config.egress);
    if stt_store_accel::request_stop(port).is_ok() {
        append_host_log(
            steam_root,
            &format!("store_accel=legacy_helper_stop port={port}"),
        );
        wait_for_store_accel_port_release(port);
    }
    let generation = {
        let mut runtime = store_accel_runtime()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runtime.generation = runtime.generation.wrapping_add(1);
        let generation = runtime.generation;
        runtime.active = Some(StoreAccelInstance {
            generation,
            port,
            stop: Arc::clone(&stop),
            thread: None,
        });
        generation
    };
    let root = steam_root.to_path_buf();
    let thread_stop = Arc::clone(&stop);
    let worker = std::thread::Builder::new()
        .name("store-accel".into())
        .spawn(move || {
            let result = stt_store_accel::run(&root, thread_stop);
            clear_store_accel_runtime(generation);
            match result {
                Ok(()) => append_host_log(
                    &root,
                    &format!("store_accel=stopped generation={generation}"),
                ),
                Err(error) => append_host_log(
                    &root,
                    &format!("store_accel=runtime_error generation={generation} error={error}"),
                ),
            }
        });
    match worker {
        Ok(thread) => {
            let mut thread = Some(thread);
            let retained = {
                let mut runtime = store_accel_runtime()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(instance) = runtime
                    .active
                    .as_mut()
                    .filter(|instance| instance.generation == generation)
                {
                    instance.thread = thread.take();
                    true
                } else {
                    false
                }
            };
            if retained {
                append_host_log(
                    steam_root,
                    &format!(
                        "store_accel=start_requested generation={generation} port={port} egress={egress} mode=in_process"
                    ),
                );
            }
        }
        Err(error) => {
            clear_store_accel_runtime(generation);
            append_host_log(
                steam_root,
                &format!("store_accel=start_error generation={generation} error={error}"),
            );
        }
    }
}

fn clear_store_accel_runtime(generation: u64) {
    let mut runtime = store_accel_runtime()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if runtime
        .active
        .as_ref()
        .is_some_and(|instance| instance.generation == generation)
    {
        runtime.active = None;
    }
}

fn stop_store_accel(steam_root: &Path, fallback_port: u16) {
    let active = {
        let mut runtime = store_accel_runtime()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runtime.active.take()
    };
    let port = active
        .as_ref()
        .map_or(fallback_port, |instance| instance.port);
    match stt_store_accel::request_stop(port) {
        Ok(()) => append_host_log(
            steam_root,
            &format!("store_accel=stop_requested port={port} mode=in_process"),
        ),
        Err(error) => append_host_log(
            steam_root,
            &format!("store_accel=stop_signal port={port} error={error}"),
        ),
    }
    if let Some(mut instance) = active {
        instance.stop.store(true, Ordering::Release);
        if let Some(thread) = instance.thread.take() {
            let _ = thread.join();
        }
    }
    wait_for_store_accel_port_release(port);
}

fn wait_for_store_accel_port_release(port: u16) {
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    for _ in 0..10 {
        if TcpListener::bind(address).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

const fn store_accel_egress_name(egress: StoreAccelEgress) -> &'static str {
    match egress {
        StoreAccelEgress::Disabled => "disabled",
        StoreAccelEgress::DirectDns => "direct_dns",
        StoreAccelEgress::LocalCdn => "local_cdn",
        StoreAccelEgress::HttpConnect => "http_connect",
    }
}

/// inbox 单文件大小上限: 超过就当噪音跳过, 不读.
const MAX_INBOX_FILE_BYTES: u64 = 1024 * 1024;

/// bad_line 日志不回显整行内容: 去掉控制字符再截到 120 字符.
fn clip_bad_line(line: &str) -> String {
    const MAX_BAD_LINE_CHARS: usize = 120;
    line.chars()
        .filter(|ch| !matches!(ch, '\0' | '\r' | '\n'))
        .take(MAX_BAD_LINE_CHARS)
        .collect()
}

/// 处理 steamtools/inbox/*.txt: 每行一个 app_id, 按当前 Catalog 配置入库.
///
/// 有效行只入有界 catalog worker 队列 (容量 32), 由 worker 线程跑 provider 链;
/// 队列满就记日志放弃, 不让 watch 线程同步网络, 也不阻塞轮询.
fn process_inbox(
    steam_root: &Path,
    state: &ConfigState,
    catalog_jobs: &SyncSender<CatalogJob>,
    seen: &mut std::collections::HashSet<std::path::PathBuf>,
) {
    let dir = stt_platform::inbox_dir(steam_root);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    for ent in rd.flatten() {
        let path = ent.path();
        if !path.is_file() {
            continue;
        }
        let is_txt = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("txt"));
        if !is_txt {
            continue;
        }
        // 符号链接 / 超过 1 MiB 的文件都不读: 前者防路径被指向别处,
        // 后者防把大文件整个拖进内存再逐行解析.
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() || meta.len() > MAX_INBOX_FILE_BYTES {
            append_host_log(
                steam_root,
                &format!(
                    "catalog_add=inbox skip path={} reason={}",
                    path.display(),
                    if meta.file_type().is_symlink() {
                        "symlink"
                    } else {
                        "large"
                    }
                ),
            );
            continue;
        }
        if !seen.insert(path.clone()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            append_host_log(
                steam_root,
                &format!("catalog_add=inbox read_err path={}", path.display()),
            );
            continue;
        };
        let mut any = false;
        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Ok(app_id) = line.parse::<u32>() else {
                append_host_log(
                    steam_root,
                    &format!(
                        "catalog_add=inbox bad_line path={} line={} text={}",
                        path.display(),
                        lineno + 1,
                        clip_bad_line(line)
                    ),
                );
                continue;
            };
            any = true;
            let job = CatalogJob {
                app_id,
                source: CatalogJobSource::Inbox,
                dlc_mode: default_dlc_mode(state),
            };
            if let Err(error) = queue_catalog_job(catalog_jobs, job) {
                // 有界队列, 满就丢这一次, 不阻塞 watch 轮询.
                append_host_log(
                    steam_root,
                    &format!("catalog_add=queue_err source=inbox app_id={app_id} {error}"),
                );
            }
        }
        if !any {
            append_host_log(
                steam_root,
                &format!("catalog_add=inbox empty path={}", path.display()),
            );
        }
        // 处理完挪到 done, 避免反复触发; 挪成功才从 seen 摘掉,
        // 不然集合会随长会话里不断出现的 inbox 文件越攒越大.
        let done_dir = dir.join("done");
        let _ = std::fs::create_dir_all(&done_dir);
        if let Some(name) = path.file_name() {
            let dest = done_dir.join(name);
            if std::fs::rename(&path, &dest).is_ok() {
                seen.remove(&path);
            }
        }
    }
}

/// 持续补挂 CreateProcessW hook, 并在始终截不到 webhelper 时写明诊断.
///
/// 不设终点: 模块是陆续加载的, webhelper 崩了 Steam 还会重拉一个.
/// 但 2s 一轮就够, 不占满 250ms 的 watch 节拍.
#[derive(Default)]
struct CefRearm {
    ticks: u32,
    modules: usize,
    caught: bool,
    warned: bool,
    /// 与 init 时一致, 否则补挂会把通道悄悄切回端口.
    use_pipe: bool,
}

impl CefRearm {
    /// 每 8 个 watch tick 补挂一次 (~2s).
    const EVERY: u32 = 8;

    /// 日志里怎么称呼当前通道.
    ///
    /// pipe 模式下不存在端口, 早先这里无脑打 `cdp_host_port()`, 结果日志显示
    /// `cdp=127.0.0.1:8080` — 明明走的是管道, 读日志的人会以为端口还开着.
    fn channel_label(&self) -> String {
        channel_label(self.use_pipe)
    }
    /// 这么久还没截到就写诊断 (~20s), 够覆盖冷启动.
    const WARN_AFTER: u32 = 80;

    fn tick(&mut self, steam_root: &Path, state: &ConfigState) {
        self.ticks += 1;
        if !self.ticks.is_multiple_of(Self::EVERY) {
            return;
        }
        let r = stt_steamui::install_cef_debug_hook(needs_cef_channel(state), self.use_pipe);
        if r.modules.len() > self.modules {
            self.modules = r.modules.len();
            append_host_log(steam_root, &format!("{} (rearm)", r.summary_line()));
        }
        if !self.caught && r.caught_webhelper() {
            self.caught = true;
            append_host_log(
                steam_root,
                &format!("cef_debug=caught webhelper via={}", self.channel_label()),
            );
            // pipe 方案探路: Steam 给的 bInheritHandles / STARTUPINFO 决定
            // 我们能不能干净地塞进 fd 3/4.
            if let Some(snap) = stt_steamui::take_launch_snapshot() {
                append_host_log(steam_root, &format!("cef_debug=launch_params {snap}"));
            }
        }
        if !self.caught && !self.warned && self.ticks >= Self::WARN_AFTER {
            self.warned = true;
            let (calls, seen, rewrites) = stt_steamui::cef_debug_stats();
            // calls=0: hook 没挂到发起调用的模块.
            // calls>0 且 seen=0: webhelper 走的不是 CreateProcessW.
            append_host_log(
                steam_root,
                &format!(
                    "cef_debug=missed_webhelper calls={calls} webhelper={seen} rewrites={rewrites} \
                     channel={} (webhelper kept its own arguments; no debug channel of ours)",
                    self.channel_label()
                ),
            );
        }
    }
}

fn run_watch_loop(
    steam_root: &Path,
    state: &ConfigState,
    use_pipe: bool,
    patterns: stt_metadata::PatternStore,
) {
    let mut toml_watch = stt_config::host_toml_watcher(steam_root, WATCH_DEBOUNCE);
    let mut lua_watch = {
        let host = state.host();
        stt_config::lua_files_watcher(steam_root, &host, WATCH_DEBOUNCE)
    };
    let mut inbox_seen: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    // inbox 只入队, 由这里独立的有界 worker 处理; 反馈通道没人读, 丢弃即可.
    let watch_note = global_note();
    let (catalog_jobs, _catalog_feedback) = spawn_catalog_worker(steam_root, state, &watch_note);

    // 定期重扫目录, 好把新建的 .lua 纳入监视.
    let mut rescan_ticks: u32 = 0;
    let mut cef_rearm = CefRearm {
        use_pipe,
        ..CefRearm::default()
    };
    // steamclient64 常比 host 晚加载; init 时没挂上就在 watch 里补.
    let mut package_rearm_ticks: u32 = 0;
    let mut package_attached_logged = stt_steamclient::is_attached();
    let mut last_package_hook_stats = ((0, 0), (0, None));
    let mut last_library_stats = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut library_rearm_ticks: u32 = 0;
    let mut library_attached_logged = stt_steamui::library_detour_attached();
    #[cfg(feature = "download-manifest")]
    let mut manifest_attached_logged = stt_steamclient::is_manifest_hook_attached();
    #[cfg(feature = "download-manifest")]
    let mut last_manifest_stats = stt_steamclient::manifest_hook_stats();
    #[cfg(feature = "download-key")]
    let mut key_attached_logged = stt_steamclient::is_depot_key_hook_attached();
    #[cfg(feature = "download-key")]
    let mut last_key_stats = stt_steamclient::depot_key_hook_stats();
    #[cfg(feature = "download-token")]
    let mut token_attached_logged = stt_steamclient::is_access_token_hook_attached();
    #[cfg(feature = "download-token")]
    let mut last_token_stats = stt_steamclient::access_token_hook_stats();
    #[cfg(feature = "download-request-code")]
    let mut request_code_attached_logged = stt_steamclient::is_manifest_code_hook_attached();
    #[cfg(feature = "download-request-code")]
    let mut last_request_code_stats = stt_steamclient::manifest_code_hook_stats();

    loop {
        std::thread::sleep(WATCH_POLL);
        process_inbox(steam_root, state, &catalog_jobs, &mut inbox_seen);
        cef_rearm.tick(steam_root, state);

        // ~2s 一轮: 未 attach 且 catalog_add 开着则重试 package hooks.
        package_rearm_ticks = package_rearm_ticks.wrapping_add(1);
        library_rearm_ticks = library_rearm_ticks.wrapping_add(1);
        let package_hook_stats = (
            stt_steamclient::hook_stats(),
            stt_steamclient::package_info_stats(),
        );
        if package_hook_stats != last_package_hook_stats {
            let ((checks, forged), (package0_calls, package0_status)) = package_hook_stats;
            append_host_log(
                steam_root,
                &format!(
                    "package_stats checks={} forged={} package0_calls={} package0_status={}",
                    checks,
                    forged,
                    package0_calls,
                    package0_status
                        .map_or_else(|| "unknown".to_owned(), |status| status.to_string())
                ),
            );
            last_package_hook_stats = package_hook_stats;
        }

        // 库 UX detour 统计: 有变化才写.
        let library_stats = stt_steamui::library_detour_stats();
        if library_stats != last_library_stats && library_stats != (0, 0, 0, 0, 0) {
            append_host_log(
                steam_root,
                &format!(
                    "library_ux_stats run_frame={} drained={} fill_in={} build_complete={} mark_changes={}",
                    library_stats.0, library_stats.1, library_stats.2, library_stats.3, library_stats.4
                ),
            );
            last_library_stats = library_stats;
        }
        // steamui 常驻, 一般不会晚加载; 万一没挂上仍按周期重试一次.
        if !library_attached_logged
            && library_rearm_ticks.is_multiple_of(16)
            && state.tools().is_enabled(ToolId::LibraryUx)
        {
            let report =
                stt_steamui::try_install_library_detours(&state.tools(), &patterns, library_ux());
            if stt_steamui::library_detour_attached() {
                library_attached_logged = true;
                append_host_log(steam_root, &report.summary_line());
            }
        }
        if !package_attached_logged
            && package_rearm_ticks.is_multiple_of(8)
            && state.tools().is_enabled(ToolId::CatalogAdd)
        {
            let report = stt_steamclient::try_install_package_hooks(&state.tools(), &patterns);
            if stt_steamclient::is_attached() {
                package_attached_logged = true;
                append_host_log(steam_root, &report.summary_line());
                if let Some(d) = report.attach_detail() {
                    append_host_log(steam_root, &format!("package=rearm detail {d}"));
                }
                append_host_log(
                    steam_root,
                    "package=hooks attached on rearm (steamclient64 became available)",
                );
            } else if package_rearm_ticks == 8
                || package_rearm_ticks == 40
                || package_rearm_ticks.is_multiple_of(80)
            {
                // 少打点: 首轮 / ~10s / 之后偶发.
                append_host_log(
                    steam_root,
                    &format!(
                        "package=rearm still waiting ({})",
                        report.attach_detail().unwrap_or("not attached")
                    ),
                );
            }
        }
        // package0 周期 sync: 启动补注入 + Steam 原生卸载后的 wipe 自愈.
        // notify 内部会 resync AppIdVec 真值再 reconcile configured.
        if package_rearm_ticks.is_multiple_of(8) && stt_steamclient::is_attached() {
            if let Some(queue) = license_queue() {
                let was_ready = queue.is_fake_license_ready();
                let plan = stt_steamclient::notify_license_changed(&queue);
                if !was_ready && queue.is_fake_license_ready() {
                    append_host_log(
                        steam_root,
                        &format!("package=startup_sync {}", plan.summary_line()),
                    );
                } else if plan.should_mark_license_changed && !plan.insert_ids.is_empty() {
                    // 常见于: 卸载某游戏后 Steam 清了 package0, 我们把仍入库的 id 补回.
                    append_host_log(
                        steam_root,
                        &format!("package=heal_sync {}", plan.summary_line()),
                    );
                }
            }
        }

        #[cfg(feature = "download-manifest")]
        if manifest_attached_logged && !stt_steamclient::is_manifest_hook_attached() {
            manifest_attached_logged = false;
            append_host_log(steam_root, "download_manifest=hook lost, scheduling rearm");
        }

        #[cfg(feature = "download-key")]
        if key_attached_logged && !stt_steamclient::is_depot_key_hook_attached() {
            key_attached_logged = false;
            append_host_log(steam_root, "download_key=hook lost, scheduling rearm");
        }

        #[cfg(feature = "download-token")]
        if token_attached_logged && !stt_steamclient::is_access_token_hook_attached() {
            token_attached_logged = false;
            append_host_log(steam_root, "download_token=hook lost, scheduling rearm");
        }

        #[cfg(feature = "download-request-code")]
        if request_code_attached_logged && !stt_steamclient::is_manifest_code_hook_attached() {
            request_code_attached_logged = false;
            append_host_log(
                steam_root,
                "download_request_code=hook lost, scheduling rearm",
            );
        }

        #[cfg(any(
            feature = "download-manifest",
            feature = "download-key",
            feature = "download-token",
            feature = "download-request-code"
        ))]
        if package_rearm_ticks.is_multiple_of(8)
            && state.tools().is_enabled(ToolId::DownloadKit)
            && download_hook_rearm_pending()
        {
            sync_download_runtime(state);
        }

        #[cfg(feature = "download-manifest")]
        if !manifest_attached_logged
            && package_rearm_ticks.is_multiple_of(8)
            && state.tools().is_enabled(ToolId::DownloadKit)
        {
            let mut report = build_download_report(state, &patterns);
            stt_steamclient::try_install_manifest_hook(&mut report, &patterns);
            let manifest = report.capabilities.iter().find(|item| {
                item.capability == stt_steamclient::DownloadCapability::ManifestOverride
            });
            if stt_steamclient::is_manifest_hook_attached() {
                manifest_attached_logged = true;
                append_host_log(steam_root, "download_manifest=hook attached on rearm");
            } else if package_rearm_ticks == 8
                || package_rearm_ticks == 40
                || package_rearm_ticks.is_multiple_of(80)
            {
                let status = manifest
                    .map(|item| format!("{:?}", item.status))
                    .unwrap_or_else(|| "MissingReport".to_owned());
                let detail = manifest
                    .and_then(|item| item.detail.as_deref())
                    .unwrap_or("not attached");
                append_host_log(
                    steam_root,
                    &format!("download_manifest=rearm waiting status={status} detail={detail}"),
                );
            }
        }

        #[cfg(feature = "download-key")]
        if !key_attached_logged
            && package_rearm_ticks.is_multiple_of(8)
            && state.tools().is_enabled(ToolId::DownloadKit)
        {
            let mut report = build_download_report(state, &patterns);
            stt_steamclient::try_install_depot_key_hook(&mut report, &patterns);
            let key = report
                .capabilities
                .iter()
                .find(|item| item.capability == stt_steamclient::DownloadCapability::DepotKey);
            if stt_steamclient::is_depot_key_hook_attached() {
                key_attached_logged = true;
                append_host_log(steam_root, "download_key=hook attached on rearm");
            } else if package_rearm_ticks == 8
                || package_rearm_ticks == 40
                || package_rearm_ticks.is_multiple_of(80)
            {
                let status = key
                    .map(|item| format!("{:?}", item.status))
                    .unwrap_or_else(|| "MissingReport".to_owned());
                let detail = key
                    .and_then(|item| item.detail.as_deref())
                    .unwrap_or("not attached");
                append_host_log(
                    steam_root,
                    &format!("download_key=rearm waiting status={status} detail={detail}"),
                );
            }
        }

        #[cfg(feature = "download-token")]
        if !token_attached_logged
            && package_rearm_ticks.is_multiple_of(8)
            && state.tools().is_enabled(ToolId::DownloadKit)
        {
            let mut report = build_download_report(state, &patterns);
            stt_steamclient::try_install_access_token_hook(&mut report, &patterns);
            let token = report
                .capabilities
                .iter()
                .find(|item| item.capability == stt_steamclient::DownloadCapability::AccessToken);
            if stt_steamclient::is_access_token_hook_attached() {
                token_attached_logged = true;
                append_host_log(steam_root, "download_token=hook attached on rearm");
            } else if package_rearm_ticks == 8
                || package_rearm_ticks == 40
                || package_rearm_ticks.is_multiple_of(80)
            {
                let status = token
                    .map(|item| format!("{:?}", item.status))
                    .unwrap_or_else(|| "MissingReport".to_owned());
                let detail = token
                    .and_then(|item| item.detail.as_deref())
                    .unwrap_or("not attached");
                append_host_log(
                    steam_root,
                    &format!("download_token=rearm waiting status={status} detail={detail}"),
                );
            }
        }

        #[cfg(feature = "download-request-code")]
        if !request_code_attached_logged
            && package_rearm_ticks.is_multiple_of(8)
            && state.tools().is_enabled(ToolId::DownloadKit)
        {
            let mut report = build_download_report(state, &patterns);
            stt_steamclient::try_install_manifest_code_hooks(&mut report, &patterns);
            let request_code = report
                .capabilities
                .iter()
                .find(|item| item.capability == stt_steamclient::DownloadCapability::RequestCode);
            if stt_steamclient::is_manifest_code_hook_attached() {
                request_code_attached_logged = true;
                append_host_log(steam_root, "download_request_code=hooks attached on rearm");
            } else if package_rearm_ticks == 8
                || package_rearm_ticks == 40
                || package_rearm_ticks.is_multiple_of(80)
            {
                let status = request_code
                    .map(|item| format!("{:?}", item.status))
                    .unwrap_or_else(|| "MissingReport".to_owned());
                let detail = request_code
                    .and_then(|item| item.detail.as_deref())
                    .unwrap_or("not attached");
                append_host_log(
                    steam_root,
                    &format!("download_request_code=rearm waiting status={status} detail={detail}"),
                );
            }
        }

        #[cfg(feature = "download-manifest")]
        {
            let stats = stt_steamclient::manifest_hook_stats();
            if stats != last_manifest_stats {
                append_host_log(
                    steam_root,
                    &format!(
                        "download_manifest_stats calls={} patched={}",
                        stats.0, stats.1
                    ),
                );
                last_manifest_stats = stats;
            }
        }

        #[cfg(feature = "download-key")]
        {
            let stats = stt_steamclient::depot_key_hook_stats();
            if stats != last_key_stats {
                append_host_log(
                    steam_root,
                    &format!(
                        "download_key_stats calls={} served={} path_hit_miss={} snapshot={}",
                        stats.0, stats.1, stats.2, stats.3
                    ),
                );
                last_key_stats = stats;
            }
        }

        #[cfg(feature = "download-token")]
        {
            let stats = stt_steamclient::access_token_hook_stats();
            if stats != last_token_stats {
                append_host_log(
                    steam_root,
                    &format!(
                        "download_token_stats calls={} frames={} apps={}",
                        stats.0, stats.1, stats.2
                    ),
                );
                last_token_stats = stats;
            }
        }

        #[cfg(feature = "download-request-code")]
        {
            let stats = stt_steamclient::manifest_code_hook_stats();
            if stats != last_request_code_stats {
                append_host_log(
                    steam_root,
                    &format!(
                        "download_request_code_stats calls={} submitted={} dropped={} completed={} patched={}",
                        stats.0, stats.1, stats.2, stats.3, stats.4
                    ),
                );
                last_request_code_stats = stats;
            }
        }

        let mut host_changed = false;
        if let Some(w) = toml_watch.as_mut() {
            if !w.poll().is_empty() {
                host_changed = true;
            }
        } else if HostConfig::resolve_path(steam_root).is_some() {
            toml_watch = stt_config::host_toml_watcher(steam_root, WATCH_DEBOUNCE);
            host_changed = toml_watch.is_some();
        }

        if host_changed {
            let was_store_accel = state.tools().is_enabled(ToolId::StoreAccel);
            let previous_store_accel = state.host().store_accel;
            match state.load_host_from_steam_root(steam_root) {
                Ok(()) => {
                    reconcile_store_accel(
                        steam_root,
                        was_store_accel,
                        Some(&previous_store_accel),
                        state,
                    );
                    let log_level = state.host().log.level;
                    if !set_host_log_level(&log_level) {
                        append_host_log_at(
                            steam_root,
                            HostLogLevel::Error,
                            &format!(
                                "reload=invalid_log_level value={log_level} keeping_previous_level"
                            ),
                        );
                    }
                    append_host_log(
                        steam_root,
                        &format!(
                            "reload=host_toml {} {}",
                            tools_enabled_line(state).trim(),
                            catalog_mode_line(state)
                        ),
                    );
                    let host = state.host();
                    lua_watch = stt_config::lua_files_watcher(steam_root, &host, WATCH_DEBOUNCE);
                    let report = state.reload_lua_dirs(steam_root);
                    append_host_log(
                        steam_root,
                        &format!(
                            "reload=lua_after_toml ok={} err={} owned={} epoch={}",
                            report.files_ok,
                            report.files_err,
                            state.owned_count(),
                            state.rules_epoch()
                        ),
                    );
                    on_rules_reloaded(steam_root, state, &patterns);
                }
                Err(e) => append_host_log(steam_root, &format!("reload=host_toml error={e}")),
            }
        }

        rescan_ticks = rescan_ticks.wrapping_add(1);
        if rescan_ticks.is_multiple_of(20) {
            let host = state.host();
            let files =
                stt_config::list_lua_files_in_dirs(&stt_config::lua_search_dirs(steam_root, &host));
            let current = lua_watch.path_list();
            if files != current {
                lua_watch.set_paths(files);
                let report = state.reload_lua_dirs(steam_root);
                append_host_log(
                    steam_root,
                    &format!(
                        "reload=lua_rescan ok={} err={} owned={} epoch={}",
                        report.files_ok,
                        report.files_err,
                        state.owned_count(),
                        state.rules_epoch()
                    ),
                );
                on_rules_reloaded(steam_root, state, &patterns);
            }
        }

        if !lua_watch.poll().is_empty() {
            let report = state.reload_lua_dirs(steam_root);
            append_host_log(
                steam_root,
                &format!(
                    "reload=lua ok={} err={} owned={} epoch={}",
                    report.files_ok,
                    report.files_err,
                    state.owned_count(),
                    state.rules_epoch()
                ),
            );
            on_rules_reloaded(steam_root, state, &patterns);
            let host = state.host();
            lua_watch = stt_config::lua_files_watcher(steam_root, &host, WATCH_DEBOUNCE);
        }
    }
}

#[cfg(windows)]
mod windows_entry {
    use std::ffi::c_void;

    use super::run_init;

    const DLL_PROCESS_ATTACH: u32 = 1;
    const DLL_PROCESS_DETACH: u32 = 0;

    unsafe extern "system" fn init_thread_proc(param: *mut c_void) -> u32 {
        if let Some(root) = stt_platform::steam_root_from_raw(param) {
            if let Err(e) = run_init(&root) {
                let fallback = root.join("SteamTools-host-error.txt");
                let _ = std::fs::write(fallback, format!("run_init failed: {e}"));
            }
        }
        0
    }

    #[no_mangle]
    pub extern "system" fn DllMain(hinst: *mut c_void, reason: u32, _reserved: *mut c_void) -> i32 {
        match reason {
            DLL_PROCESS_ATTACH => {
                unsafe {
                    stt_platform::disable_thread_library_calls_raw(hinst);
                    let _ = stt_platform::spawn_thread_raw(init_thread_proc, hinst);
                }
                1
            }
            DLL_PROCESS_DETACH => 1,
            _ => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(all(
        feature = "download-manifest",
        feature = "download-key",
        feature = "download-token",
        feature = "download-request-code"
    ))]
    use std::io::{Read, Write};
    #[cfg(all(
        feature = "download-manifest",
        feature = "download-key",
        feature = "download-token",
        feature = "download-request-code"
    ))]
    use std::net::TcpListener;
    #[cfg(all(
        feature = "download-manifest",
        feature = "download-key",
        feature = "download-token",
        feature = "download-request-code"
    ))]
    use std::thread::JoinHandle;
    #[cfg(all(
        feature = "download-manifest",
        feature = "download-key",
        feature = "download-token",
        feature = "download-request-code"
    ))]
    use std::time::Instant;

    #[cfg(all(
        feature = "download-manifest",
        feature = "download-key",
        feature = "download-token",
        feature = "download-request-code"
    ))]
    struct FakeCatalogServer {
        template: String,
        thread: Option<JoinHandle<()>>,
    }

    #[cfg(all(
        feature = "download-manifest",
        feature = "download-key",
        feature = "download-token",
        feature = "download-request-code"
    ))]
    impl FakeCatalogServer {
        fn spawn(body: Vec<u8>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let thread = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => return,
                    }
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request);
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n",
                    body.len()
                );
                stream.write_all(headers.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
            });
            Self {
                template: format!("http://127.0.0.1:{port}/catalog/{{app_id}}"),
                thread: Some(thread),
            }
        }
    }

    #[cfg(all(
        feature = "download-manifest",
        feature = "download-key",
        feature = "download-token",
        feature = "download-request-code"
    ))]
    impl Drop for FakeCatalogServer {
        fn drop(&mut self) {
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }

    #[test]
    fn placeholder_init_empty() {
        let rules = init_placeholder();
        assert_eq!(rules.epoch(), 0);
    }

    #[test]
    fn pattern_cache_write_leaves_only_the_final_file() {
        let root = std::env::temp_dir().join(format!(
            "steamtools-pattern-cache-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let path = root.join("steamtools/pattern/steamui/test.toml");

        write_pattern_cache(&path, b"[0x1]\nname = \"Demo\"\n")
            .expect("pattern cache write should succeed");

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[0x1]\nname = \"Demo\"\n"
        );
        let files: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(files, vec![std::ffi::OsString::from("test.toml")]);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bootstrap_loads_lua_without_watch_loop() {
        let dir = std::env::temp_dir().join(format!("steamtools-host-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("config").join("lua")).unwrap();
        fs::write(
            dir.join("config").join("lua").join("t.lua"),
            "addappid(42)\n",
        )
        .unwrap();
        fs::write(
            dir.join("steamtools.toml"),
            "[tools.enabled]\ncatalog_add = true\n",
        )
        .unwrap();

        let state = bootstrap_config(&dir).unwrap();
        let report = state.reload_lua_dirs(&dir);
        assert_eq!(report.files_ok, 1);
        assert!(state.with_rules(|r| r.is_owned(42)));

        stt_platform::ensure_data_dir(&dir).unwrap();
        let log = stt_platform::host_log_path(&dir);
        let body = format!(
            "status=init complete\nrules_epoch={}\nowned_count={}\n",
            state.rules_epoch(),
            state.owned_count()
        );
        fs::write(&log, body).unwrap();
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.contains("init complete"), "{text}");
        assert!(text.contains("owned_count=1"), "{text}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_catalog_does_not_fall_back_to_synthetic_mock() {
        let dir = Path::new("unused");
        let config = CatalogSection::default();

        // 默认走内置社区链, 不静默回退 synthetic Mock.
        let provider = build_catalog_provider(dir, &config).unwrap();
        assert_eq!(provider.id(), "chain");
        assert_ne!(provider.id(), "mock");
    }

    #[test]
    fn mock_provider_requires_explicit_mode() {
        let dir = Path::new("unused");
        let config = CatalogSection::default();
        let provider = build_catalog_provider(dir, &config).unwrap();
        assert_ne!(provider.id(), "mock");

        let config = CatalogSection {
            mode: CatalogMode::Mock,
            ..CatalogSection::default()
        };
        let provider = build_catalog_provider(dir, &config).unwrap();

        assert_eq!(provider.id(), "mock");
    }

    #[test]
    fn lua_catalog_uses_fixed_file_and_reports_final_source() {
        let root = std::env::temp_dir().join(format!(
            "steamtools-host-lua-catalog-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let lua_dir = ConfigState::default_lua_dir(&root);
        fs::create_dir_all(&lua_dir).unwrap();
        fs::write(
            lua_dir.join("catalog.lua"),
            r#"
function fetch_catalog(app_id)
  return '{"schema_version":1,"apps":[{"app_id":' .. app_id .. '}]}'
end
"#,
        )
        .unwrap();
        let config = CatalogSection {
            mode: CatalogMode::Lua,
            ..CatalogSection::default()
        };

        let provider = build_catalog_provider(&root, &config).unwrap();
        let outcome = provider.fetch_with_trace(42).unwrap();

        assert_eq!(outcome.source, "lua");
        assert_eq!(outcome.trace.len(), 1);
        assert_eq!(outcome.trace[0].outcome, CatalogTraceOutcome::Hit);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn download_kit_runs_by_default() {
        let tools = stt_config::ToolRegistry::with_defaults();
        let report = stt_steamclient::plan_download_kit(
            &tools,
            &stt_metadata::PatternStore::new(),
            "steamclient",
            stt_steamclient::DownloadFeatureSet::compiled(),
            stt_steamclient::DownloadRuntimeSwitches::default(),
            stt_steamclient::DownloadDataAvailability::default(),
        );

        assert!(!report
            .capabilities
            .iter()
            .any(|item| item.status == stt_steamclient::DownloadCapabilityStatus::ToolDisabled));
    }

    #[test]
    fn store_accel_detail_does_not_claim_pac_without_an_egress() {
        let state = ConfigState::new();
        let mut host = HostConfig::default();
        host.tools.enabled.insert("store_accel".into(), true);
        state.apply_host(host);
        assert!(store_accel_detail(Path::new("C:/steam"), &state).contains("不会修改系统 PAC"));

        let mut host = state.host();
        host.store_accel.egress = StoreAccelEgress::DirectDns;
        state.apply_host(host);
        assert!(store_accel_detail(Path::new("C:/steam"), &state).contains("兼容模式"));

        let mut host = state.host();
        host.store_accel.egress = StoreAccelEgress::LocalCdn;
        host.store_accel.clash_fallback = "127.0.0.1:7890".into();
        state.apply_host(host);
        let detail = store_accel_detail(Path::new("C:/steam"), &state);
        assert!(detail.contains("degraded"));
        assert!(detail.contains("local_direct"));
        assert!(detail.contains("clash_fallback"));

        let mut host = state.host();
        host.store_accel.egress = StoreAccelEgress::HttpConnect;
        host.store_accel.upstream = "203.0.113.9:443".into();
        state.apply_host(host);
        let detail = store_accel_detail(Path::new("C:/steam"), &state);
        assert!(detail.contains("degraded"));
        assert!(detail.contains("user_connect"));
    }

    #[test]
    fn token_without_configured_app_is_not_available() {
        let mut rules = AppRules::new();
        rules.set_access_token(42, 123);

        assert!(!has_configured_access_token(&rules));
    }

    #[test]
    fn configured_app_with_nonzero_token_is_available() {
        let mut rules = AppRules::new();
        rules.add_app(42);
        rules.set_access_token(42, 123);

        assert!(has_configured_access_token(&rules));
    }

    #[test]
    fn configured_app_with_zero_token_is_not_available() {
        let mut rules = AppRules::new();
        rules.add_app(42);
        rules.set_access_token(42, 0);

        assert!(!has_configured_access_token(&rules));
    }

    #[cfg(all(
        feature = "download-manifest",
        feature = "download-key",
        feature = "download-token",
        feature = "download-request-code"
    ))]
    #[test]
    fn store_catalog_job_reaches_atomic_lua_core_snapshots_and_license_notify() {
        let body = format!(
            r#"{{"schema_version":1,"apps":[{{"app_id":42,"access_token":"123","depots":[{{"depot_id":43,"key":"{}","manifest":{{"gid":"99","size":"100"}}}}]}}]}}"#,
            "ab".repeat(32)
        )
        .into_bytes();
        let server = FakeCatalogServer::spawn(body);
        let root = tempfile::tempdir().unwrap();
        stt_platform::ensure_data_dir(root.path()).unwrap();
        let state = ConfigState::new();
        let mut host = HostConfig::default();
        host.catalog.mode = CatalogMode::CustomHttp;
        host.catalog.url_template = server.template.clone();
        // 单测不走 DLC 扩展 (否则 related 空会打 store 兜底网, 易超时).
        host.catalog.auto_dlc = false;
        host.tools.enabled.insert("download_kit".to_owned(), true);
        state.apply_host(host);

        let queue = Arc::new(LicenseQueue::new());
        let configured = Arc::new(RwLock::new(HashSet::new()));
        // 进程内 OnceLock 只能 set 一次; 已有就清空复用.
        let queue = match LICENSE_QUEUE.set(Arc::clone(&queue)) {
            Ok(()) => queue,
            Err(_) => {
                let existing = LICENSE_QUEUE.get().unwrap().clone();
                existing.clear_for_test();
                existing
            }
        };
        let configured = match CONFIGURED_APPS.set(Arc::clone(&configured)) {
            Ok(()) => configured,
            Err(_) => {
                let existing = CONFIGURED_APPS.get().unwrap().clone();
                existing
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clear();
                existing
            }
        };
        stt_steamclient::register_runtime(Arc::clone(&queue), Arc::clone(&configured));
        stt_steamclient::set_ui_action_handler(apply_ui_license_action);

        let note = Arc::new(Mutex::new(String::new()));
        let (jobs, feedback) = spawn_catalog_worker(root.path(), &state, &note);
        queue_catalog_job(
            &jobs,
            CatalogJob {
                app_id: 42,
                source: CatalogJobSource::StoreCdp,
                dlc_mode: CatalogDlcMode::GameOnly,
            },
        )
        .unwrap();

        let feedback = feedback.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(feedback.contains("已入库"), "{feedback}");
        let lua_path = stt_config::catalog_lua_path(root.path(), 42);
        let lua = fs::read_to_string(&lua_path).unwrap();
        assert!(lua.contains("addappid(43, 0,"), "{lua}");
        assert!(!lua_path.with_file_name("stt_42.lua.tmp").exists());
        assert!(state.rules_epoch() > 0);
        assert!(state.with_rules(|rules| rules.is_owned(42)));

        let snapshot = capture_download_runtime_snapshot(
            &state,
            stt_steamclient::DownloadRuntimeSwitches::default(),
        );
        assert_eq!(snapshot.manifests.get(&43).unwrap().manifest_gid, 99);
        assert_eq!(snapshot.keys.get(&43).map(String::len), Some(64));
        assert_eq!(snapshot.tokens.get(&42), Some(&123));
        assert_eq!(snapshot.request_code_depots, HashSet::from([43]));
        // OST 对齐: package0 注入主 app + depot.
        assert!(queue.injected_contains(42));
        assert!(queue.injected_contains(43));
        let configured_ids = configured
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(configured_ids.contains(&42));
        assert!(configured_ids.contains(&43));

        let note = note
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(note.starts_with("部分成功: 入库 42"), "{note}");
        let panel = ConfigSnapshot::from_state(&state, root.path(), "test", &note);
        assert_eq!(panel.note, note);
        let log = fs::read_to_string(stt_platform::host_log_path(root.path())).unwrap();
        assert!(
            log.contains("catalog_add=partial result=partial source=store_cdp app_id=42"),
            "{log}"
        );
        assert!(log.contains("provider=custom_http"), "{log}");
        // insert=2: app 42 + depot 43.
        assert!(log.contains("package=notify mode=logic insert=2"), "{log}");
        assert!(log.contains("package_ids=42,43"), "{log}");
    }

    #[cfg(all(
        feature = "download-manifest",
        feature = "download-key",
        feature = "download-token",
        feature = "download-request-code"
    ))]
    #[test]
    fn store_catalog_job_prunes_keyless_depot_and_still_imports() {
        // provider 只给 manifest 不给 key: prune 后下载面为空, 仍入库成功, 不弹缺 key.
        let body = r#"{"schema_version":1,"apps":[{"app_id":42,"depots":[{"depot_id":43,"manifest":{"gid":"99","size":"100"}}]}]}"#
            .as_bytes()
            .to_vec();
        let server = FakeCatalogServer::spawn(body);

        let root = tempfile::tempdir().unwrap();
        stt_platform::ensure_data_dir(root.path()).unwrap();
        let state = ConfigState::new();
        let mut host = HostConfig::default();
        host.catalog.mode = CatalogMode::CustomHttp;
        host.catalog.url_template = server.template.clone();
        host.catalog.auto_dlc = false;
        state.apply_host(host);

        let queue = Arc::new(LicenseQueue::new());
        let configured = Arc::new(RwLock::new(HashSet::new()));
        let queue = match LICENSE_QUEUE.set(Arc::clone(&queue)) {
            Ok(()) => queue,
            Err(_) => {
                let existing = LICENSE_QUEUE.get().unwrap().clone();
                existing.clear_for_test();
                existing
            }
        };
        let configured = match CONFIGURED_APPS.set(Arc::clone(&configured)) {
            Ok(()) => configured,
            Err(_) => {
                let existing = CONFIGURED_APPS.get().unwrap().clone();
                existing
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clear();
                existing
            }
        };
        stt_steamclient::register_runtime(Arc::clone(&queue), Arc::clone(&configured));
        stt_steamclient::set_ui_action_handler(apply_ui_license_action);

        let note = Arc::new(Mutex::new(String::new()));
        let (jobs, feedback) = spawn_catalog_worker(root.path(), &state, &note);
        queue_catalog_job(
            &jobs,
            CatalogJob {
                app_id: 42,
                source: CatalogJobSource::StoreCdp,
                dlc_mode: CatalogDlcMode::GameOnly,
            },
        )
        .unwrap();

        // 只有按钮回写; keyless depot 已被 prune, 不再二次弹缺 key.
        let first = feedback.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(first.contains("已入库"), "{first}");
        assert!(
            feedback.recv_timeout(Duration::from_millis(200)).is_err(),
            "不应再发 missing-key 弹窗"
        );

        let note = note
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(note.contains("已落盘"), "{note}");
        assert!(!note.contains("缺 下载密钥"), "{note}");

        // 落盘 lua 只有 app 行, 没有无 key 的 depot.
        let lua = fs::read_to_string(root.path().join("config/lua/stt_42.lua")).unwrap();
        assert!(lua.contains("addappid(42"), "{lua}");
        assert!(!lua.contains("addappid(43"), "{lua}");
        assert!(!lua.contains("setmanifestid(43"), "{lua}");
        let _ = queue;
        let _ = configured;
    }

    #[test]
    fn missing_log_suffix_flags_each_missing_kind() {
        let empty = MissingDownloadData::default();
        assert_eq!(missing_log_suffix(&empty), "");
        let keys = MissingDownloadData {
            depot_keys: vec![43, 7],
            access_token: false,
        };
        assert_eq!(missing_log_suffix(&keys), " missing=depot_keys=43,7");
        let token = MissingDownloadData {
            depot_keys: vec![],
            access_token: true,
        };
        assert_eq!(
            missing_log_suffix(&token),
            " missing=missing_access_token=1"
        );
    }

    #[test]
    fn catalog_feedback_uses_the_same_three_outcome_labels() {
        for (result, note_label, button_part) in [
            (CatalogJobResult::Success, "成功", "已入库"),
            (CatalogJobResult::Partial, "部分成功", "已入库"),
            (CatalogJobResult::Failure, "失败", "失败"),
        ] {
            assert!(catalog_job_note(result, 42, "", "detail").starts_with(note_label));
            assert!(catalog_button_label(result, 42, 0).contains(button_part));
            assert!(!result.as_str().is_empty());
        }
    }

    #[cfg(all(
        feature = "download-manifest",
        feature = "download-key",
        feature = "download-token",
        feature = "download-request-code"
    ))]
    #[test]
    fn each_runtime_switch_clears_only_its_own_snapshot() {
        let state = ConfigState::new();
        let mut host = HostConfig::default();
        host.tools.enabled.insert("download_kit".to_owned(), true);
        state.apply_host(host);
        state.apply_lua(
            concat!(
                "addappid(42)\n",
                "addappid(43, 0, \"abababababababababababababababababababababababababababababababab\")\n",
                "addtoken(42, \"123\")\n",
                "setmanifestid(43, \"99\")\n",
                "setappdepots(42, {43})\n"
            ),
        )
        .unwrap();

        let cases = [
            stt_steamclient::DownloadRuntimeSwitches {
                manifest: false,
                ..stt_steamclient::DownloadRuntimeSwitches::default()
            },
            stt_steamclient::DownloadRuntimeSwitches {
                key: false,
                ..stt_steamclient::DownloadRuntimeSwitches::default()
            },
            stt_steamclient::DownloadRuntimeSwitches {
                token: false,
                ..stt_steamclient::DownloadRuntimeSwitches::default()
            },
            stt_steamclient::DownloadRuntimeSwitches {
                request_code: false,
                ..stt_steamclient::DownloadRuntimeSwitches::default()
            },
        ];
        for (index, switches) in cases.into_iter().enumerate() {
            let snapshot = capture_download_runtime_snapshot(&state, switches);
            assert_eq!(snapshot.manifests.is_empty(), index == 0);
            assert_eq!(snapshot.keys.is_empty(), index == 1);
            assert_eq!(snapshot.tokens.is_empty(), index == 2);
            assert_eq!(snapshot.request_code_depots.is_empty(), index == 3);
        }
    }
}
