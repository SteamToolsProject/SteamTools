//! 宿主 DLL 入口 (`SteamTools.dll`).
//!
//! DllMain 只起工作线程, 真正初始化在线程里做.

// crate 名与导出函数名都由产物 DLL 决定 (SteamTools.dll / DllMain), 不能蛇形.
// 这里只能用 allow: crate 名的 non_snake_case 不被 expect 追踪, 写 expect 反而
// 会报 unfulfilled_lint_expectations.
#![allow(non_snake_case)]

use std::collections::HashSet;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use stt_catalog::{
    CatalogError, CatalogLimits, CatalogProvider, CatalogProviderChain, CatalogTraceEntry,
    CatalogTraceOutcome, CommunityCatalogProvider, CustomHttpCatalogProvider, MockCatalogProvider,
    ProviderErrorKind,
};
use stt_config::{
    add_to_library, apply_intent, remove_from_library, CatalogMode, CatalogSection, ConfigIntent,
    ConfigSnapshot, ConfigState, HostConfig, LuaCatalogProvider, LuaHttpClient, LuaHttpErrorKind,
    LuaHttpMethod, LuaHttpRequest, LuaHttpResponse, ToolId,
};
use stt_core::{AppId, AppRules};
use stt_steamclient::{LicenseQueue, UiLicenseAction};

/// 进程内 package 许可队列 (init 时注册).
static LICENSE_QUEUE: OnceLock<Arc<LicenseQueue>> = OnceLock::new();
/// 配置内 app 集合, CheckAppOwnership 钩子只读这份.
static CONFIGURED_APPS: OnceLock<Arc<Mutex<HashSet<AppId>>>> = OnceLock::new();
/// 库 UX 纯逻辑控制器 (CancelRemoval / QueueRemoval).
static LIBRARY_UX: OnceLock<stt_steamui::LibraryUx> = OnceLock::new();

fn license_queue() -> Option<Arc<LicenseQueue>> {
    LICENSE_QUEUE.get().map(Arc::clone)
}

fn library_ux() -> &'static stt_steamui::LibraryUx {
    LIBRARY_UX.get_or_init(stt_steamui::LibraryUx::new)
}

fn configured_apps() -> Option<Arc<Mutex<HashSet<AppId>>>> {
    CONFIGURED_APPS.get().map(Arc::clone)
}

fn sync_configured_from_state(state: &ConfigState) {
    // 先收集 id, 再锁 mutex 一次.
    // CONFIGURED_APPS 与 package runtime 共用同一把 Arc<Mutex<HashSet>>,
    // 若持锁时再调 set_configured_apps 会 **自死锁** (非可重入 Mutex).
    let ids: Vec<AppId> = state.with_rules(|rules| rules.owned_iter().collect());
    if let Some(set) = configured_apps() {
        let mut g = set
            .lock()
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

/// 入库成功后: 入队 + notify (有 hook 则改 client, 否则纯逻辑).
fn on_library_added(steam_root: &Path, app_id: AppId) {
    stt_steamclient::add_configured_app(app_id);
    let Some(q) = license_queue() else {
        append_host_log(steam_root, "package=notify skip=no_license_queue");
        return;
    };
    q.queue_addition(app_id);
    let plan = stt_steamclient::notify_license_changed(&q);
    append_host_log(steam_root, &plan.summary_line());
    library_ux().on_rules_app_present(app_id);
}

/// 移除成功后.
fn on_library_removed(steam_root: &Path, app_id: AppId) {
    stt_steamclient::remove_configured_app(app_id);
    let Some(q) = license_queue() else {
        append_host_log(steam_root, "package=notify skip=no_license_queue");
        return;
    };
    q.queue_removal(app_id);
    let plan = stt_steamclient::notify_license_changed(&q);
    append_host_log(steam_root, &plan.summary_line());
}

/// lua 全量重载后: 与 owned 做差再 notify.
fn on_rules_reloaded(steam_root: &Path, state: &ConfigState) {
    sync_configured_from_state(state);
    let Some(q) = license_queue() else {
        return;
    };
    let owned: Vec<AppId> = state.with_rules(|r| r.owned_iter().collect());
    q.reconcile_owned(owned.iter().copied());
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

fn with_community_fallback(provider: Box<dyn CatalogProvider>) -> Box<dyn CatalogProvider> {
    Box::new(CatalogProviderChain::new(vec![
        provider,
        Box::new(CommunityCatalogProvider),
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
            Ok(with_community_fallback(provider))
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
            Ok(with_community_fallback(provider))
        }
        CatalogMode::Community => Ok(Box::new(CatalogProviderChain::new(vec![Box::new(
            CommunityCatalogProvider,
        )]))),
    }
}

fn add_from_config(
    state: &ConfigState,
    steam_root: &Path,
    app_id: AppId,
) -> stt_config::Result<stt_config::AddToLibraryOutcome> {
    let host = state.host();
    let provider = build_catalog_provider(steam_root, &host.catalog)?;
    add_to_library(state, steam_root, provider.as_ref(), app_id)
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

fn append_host_log(steam_root: &Path, line: &str) {
    let path = stt_platform::host_log_path(steam_root);
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "{line}")
        });
}

/// 初始化: 数据目录, 配置, 扫 lua, 写 host.log, 然后阻塞轮询监视.
pub fn run_init(steam_root: &Path) -> std::io::Result<()> {
    let data = stt_platform::ensure_data_dir(steam_root)?;
    let log_path = stt_platform::host_log_path(steam_root);

    let legacy_note = if stt_platform::legacy_data_dir_exists(steam_root) {
        format!(
            "legacy_data_dir_present={}\n",
            stt_platform::legacy_data_dir(steam_root).display()
        )
    } else {
        String::new()
    };

    let state = match bootstrap_config(steam_root) {
        Ok(s) => s,
        Err(e) => {
            let body = format!(
                "SteamTools host init\n\
                 steam_root={}\n\
                 data_dir={}\n\
                 host_config=(config error: {e})\n\
                 {legacy}\
                 status=init failed\n",
                steam_root.display(),
                data.display(),
                legacy = legacy_note,
            );
            std::fs::write(&log_path, body)?;
            return Ok(());
        }
    };

    // 抢在 Steam 拉起 steamwebhelper 之前装 CreateProcessW hook (ADR 0010).
    // 必须是配置就绪后的第一件事: 后面 SHA-256 两个大 DLL 要几百毫秒, 等不起.
    // 发起调用的模块可能比 host 晚加载, 而 webhelper 约 300ms 就被拉起,
    // 所以这里忙等着补挂; watch 里的 CefRearm 负责后续新模块与 webhelper 重启.
    // 首选 pipe (不开任何端口); 上次证明走不通才回退到端口.
    let channel_on = needs_cef_channel(&state);
    let use_pipe = !stt_platform::cef_pipe_fallback_marker(steam_root).is_file();
    let cef = stt_steamui::wait_cef_debug_hook(channel_on, use_pipe, Duration::from_millis(2000));

    let lua_report = state.reload_lua_dirs(steam_root);

    let cfg_path = HostConfig::resolve_path(steam_root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(defaults)".into());
    let mut config_lines = tools_enabled_line(&state);
    config_lines.push_str(&catalog_mode_line(&state));
    config_lines.push('\n');

    let body = format!(
        "SteamTools host init\n\
         steam_root={steam_root}\n\
         data_dir={data_dir}\n\
         host_config={cfg_path}\n\
         {tools}\
         lua_dirs={lua_dirs}\n\
         lua_files_ok={lua_files_ok}\n\
         lua_files_err={lua_files_err}\n\
         owned_count={owned_count}\n\
         rules_epoch={rules_epoch}\n\
         {legacy}\
         status=init bootstrapped (hooks/package still loading)\n\
         watch=pending debounce_ms={debounce_ms} poll_ms={poll_ms}\n",
        steam_root = steam_root.display(),
        data_dir = data.display(),
        cfg_path = cfg_path,
        tools = config_lines,
        lua_dirs = lua_report.dirs_scanned,
        lua_files_ok = lua_report.files_ok,
        lua_files_err = lua_report.files_err,
        owned_count = state.owned_count(),
        rules_epoch = state.rules_epoch(),
        legacy = legacy_note,
        debounce_ms = WATCH_DEBOUNCE.as_millis(),
        poll_ms = WATCH_POLL.as_millis(),
    );
    std::fs::write(&log_path, body)?;

    if lua_report.files_err > 0 {
        for e in &lua_report.errors {
            append_host_log(steam_root, &format!("lua_error={e}"));
        }
    }

    log_module_hashes(steam_root);
    let patterns = log_pattern_probe(steam_root);
    let library_ux_report = log_library_ux_plan(steam_root, &state, &patterns);
    // package attach 可能失败/较慢: 先打阶段日志, 再装, 避免 silent hang.
    append_host_log(steam_root, "package=setup begin");
    let package = setup_package_layer(steam_root, &state, &patterns);
    append_host_log(steam_root, "package=setup end");
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

    match stt_platform::write_store_inject_js(steam_root, stt_steamui::STORE_INJECT_JS) {
        Ok(path) => append_host_log(
            steam_root,
            &format!("catalog_add=store_inject ready path={}", path.display()),
        ),
        Err(e) => append_host_log(steam_root, &format!("catalog_add=store_inject err {e}")),
    }

    // 商店页在 steamwebhelper CEF, 不在 steam.exe CHTMLWindow — 主路径走 CDP.
    // native hook 默认关 (STEAMTOOLS_STORE_NATIVE=inject 才开).
    let native = stt_steamui::try_install_store_native(&state.tools(), &patterns);
    append_host_log(steam_root, &native.summary_line());

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
    let details = tool_details(
        &state,
        &library_ux_report,
        &package,
        &native,
        use_pipe,
        cef.caught_webhelper(),
    );
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
}

impl CatalogJobSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::StoreCdp => "store_cdp",
            Self::ConfigRefresh => "config_refresh",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CatalogJob {
    app_id: AppId,
    source: CatalogJobSource,
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
        .name("stt-catalog-worker".into())
        .spawn(move || {
            while let Ok(job) = jobs_rx.recv() {
                match add_from_config(&state, &root, job.app_id) {
                    Ok(out) => {
                        append_host_log(
                            &root,
                            &format!(
                                "catalog_add=ok source={} app_id={} provider={} trace={} lua={} epoch={} owned={}",
                                job.source.as_str(),
                                job.app_id,
                                out.provider_id,
                                catalog_trace_text(&out.provider_trace),
                                out.lua_path.display(),
                                out.epoch,
                                out.owned_count
                            ),
                        );
                        on_library_added(&root, job.app_id);
                        set_shared_note(
                            &note,
                            format!(
                                "已入库 {} ({}, owned={})",
                                job.app_id, out.provider_id, out.owned_count
                            ),
                        );
                        if matches!(job.source, CatalogJobSource::StoreCdp) {
                            let _ = feedback_tx.send(stt_steamui::store_button_result_js(
                                job.app_id,
                                true,
                                &format!("已入库 {}", job.app_id),
                            ));
                        }
                    }
                    Err(error) => {
                        let error_text = catalog_error_text(&error);
                        append_host_log(
                            &root,
                            &format!(
                                "catalog_add=err source={} app_id={} {error_text}",
                                job.source.as_str(),
                                job.app_id
                            ),
                        );
                        set_shared_note(&note, format!("失败: 入库 {}: {error_text}", job.app_id));
                        if matches!(job.source, CatalogJobSource::StoreCdp) {
                            let _ = feedback_tx.send(stt_steamui::store_button_result_js(
                                job.app_id,
                                false,
                                "入库失败",
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
    managed_epoch: Option<u64>,
}

fn set_shared_note(note: &Arc<Mutex<String>>, text: impl Into<String>) {
    // poison 也恢复: note 只是 UI 文案, 丢一次旧值比卡死回调划算.
    let mut g = note.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    *g = text.into();
}

fn shared_note(note: &Arc<Mutex<String>>) -> String {
    note.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

impl HostPanel {
    /// 受管列表; rules 没变就用上一次的.
    fn managed_cached(&mut self) -> &[u32] {
        let epoch = self.state.rules_epoch();
        if self.managed_epoch != Some(epoch) {
            self.managed = stt_config::managed_apps(&self.state, &self.steam_root);
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
                    },
                )?;
                Ok(format!("已排队刷新 {app_id}"))
            }
            ConfigIntent::RemoveApp(_) => {
                let out = remove_from_library(&self.state, &self.steam_root, app_id)?;
                on_library_removed(&self.steam_root, app_id);
                Ok(format!("已移除 {app_id} (owned={})", out.owned_count))
            }
            _ => Err(stt_config::ConfigError::Invalid("不是 app 意图".into())),
        }
    }
}

impl stt_steamui::PanelBridge for HostPanel {
    fn enabled(&mut self) -> bool {
        self.state.tools().is_enabled(ToolId::ConfigUi)
    }

    fn snapshot(&mut self) -> Option<ConfigSnapshot> {
        let facts = stt_config::HostFacts {
            tool_details: self.details.clone(),
            managed: self.managed_cached().to_vec(),
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
            // 针对某个 app 的意图不写 toml, 要 provider, 只有宿主这儿有.
            let done = match intent.app_target() {
                Some(app_id) => self.apply_app_intent(intent, app_id),
                None => apply_intent(&self.state, &self.steam_root, intent),
            };
            match done {
                Ok(done) => {
                    append_host_log(&self.steam_root, &format!("config_ui=saved {done}"));
                    if !matches!(intent, ConfigIntent::RefreshApp(_)) {
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
        if std::fs::write(&path, sample).is_ok() && !self.recon_done {
            self.recon_done = true;
            append_host_log(
                &self.steam_root,
                &format!("config_ui=recon path={}", path.display()),
            );
        }
    }
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
        .name("stt-store-cdp".into())
        .spawn(move || {
            let note = Arc::new(Mutex::new(String::new()));
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
                managed_epoch: None,
            };
            // 短脚本: 大 STORE_INJECT_JS 在 CEF evaluate 上易挂起.
            // 工具关掉就换成摘按钮的脚本, 让开关当场看得见.
            let mut make_js = || {
                let mut script = if state.tools().is_enabled(ToolId::CatalogAdd) {
                    stt_steamui::cdp_store_inject_js()
                } else {
                    stt_steamui::store_teardown_js()
                };
                while let Ok(feedback) = catalog_feedback.try_recv() {
                    script.push_str(";\n");
                    script.push_str(&feedback);
                }
                script
            };
            // 点击只入有界队列; HTTP 和落盘由 catalog worker 执行.
            let mut on_app = |app_id: u32| -> Option<String> {
                let job = CatalogJob {
                    app_id,
                    source: CatalogJobSource::StoreCdp,
                };
                match queue_catalog_job(&catalog_jobs, job) {
                    Ok(()) => Some(stt_steamui::store_button_result_js(
                        app_id,
                        false,
                        "正在拉取",
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
                            app_id,
                            false,
                            "队列不可用",
                        ))
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

fn log_pattern_probe(steam_root: &Path) -> stt_metadata::PatternStore {
    let mut store = stt_metadata::PatternStore::new();
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
        // steamui: 已知 sha 自动落盘内置 pattern, 用户不用手拷.
        if component == "steamui" {
            if let Some(p) = stt_steamui::ensure_builtin_steamui_pattern(steam_root, &sha) {
                append_host_log(
                    steam_root,
                    &format!("pattern_steamui=auto path={} sha={sha}", p.display()),
                );
            }
        }
        let primary = stt_platform::pattern_cache_file(steam_root, component, &sha);
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
    let report = stt_steamui::plan_library_ux_install(&tools, patterns, "steamui");
    append_host_log(steam_root, &report.summary_line());
    // 配置里已有的 app 取消移除标记 (纯逻辑; steamui 写内存 detour 仍未挂).
    let ux = library_ux();
    state.with_rules(|rules| {
        for app_id in rules.owned_iter() {
            ux.on_rules_app_present(app_id);
        }
    });
    report
}

/// 注册 LicenseQueue / 配置集, 尝试 attach package hooks, 打日志.
fn setup_package_layer(
    steam_root: &Path,
    state: &ConfigState,
    patterns: &stt_metadata::PatternStore,
) -> stt_steamclient::PackageInstallReport {
    let queue = Arc::new(LicenseQueue::new());
    let configured = Arc::new(Mutex::new(HashSet::new()));
    let owned: Vec<AppId> = state.with_rules(|rules| rules.owned_iter().collect());
    {
        let mut g = configured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.extend(owned.iter().copied());
    }
    queue.seed_injected_from_owned(owned.iter().copied());

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
                .lock()
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
        append_host_log(steam_root, "package=hooks not attached (logic-only or failed)");
    }
    report
}

/// 各工具此刻的运行状态 —— 开着不等于跑起来了, 这些原来只进 host.log.
fn tool_details(
    state: &ConfigState,
    library_ux: &stt_steamui::LibraryUxInstallReport,
    package: &stt_steamclient::PackageInstallReport,
    native: &stt_steamui::StoreNativeReport,
    use_pipe: bool,
    caught: bool,
) -> stt_config::ToolDetails {
    let tools = state.tools();
    let catalog = match state.host().catalog.mode {
        CatalogMode::Disabled => "Catalog 未配置",
        CatalogMode::CustomHttp => "Catalog: CustomHttp",
        CatalogMode::Lua => "Catalog: Lua (config/lua/catalog.lua)",
        CatalogMode::Community => "Catalog: Community 暂不可用",
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
    d.insert(
        ToolId::StoreAccel.as_str(),
        format!("尚未实现; 原生注入路径: {:?}", native.status),
    );
    d
}

/// 处理 steamtools/inbox/*.txt: 每行一个 app_id, 按当前 Catalog 配置入库.
fn process_inbox(
    steam_root: &Path,
    state: &ConfigState,
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
        if !is_txt || !seen.insert(path.clone()) {
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
                        "catalog_add=inbox bad_line path={} line={} text={line}",
                        path.display(),
                        lineno + 1
                    ),
                );
                continue;
            };
            any = true;
            match add_from_config(state, steam_root, app_id) {
                Ok(out) => {
                    append_host_log(
                        steam_root,
                        &format!(
                            "catalog_add=ok app_id={app_id} provider={} trace={} lua={} epoch={} owned={}",
                            out.provider_id,
                            catalog_trace_text(&out.provider_trace),
                            out.lua_path.display(),
                            out.epoch,
                            out.owned_count
                        ),
                    );
                    on_library_added(steam_root, app_id);
                }
                Err(error) => append_host_log(
                    steam_root,
                    &format!(
                        "catalog_add=err app_id={app_id} {}",
                        catalog_error_text(&error)
                    ),
                ),
            }
        }
        if !any {
            append_host_log(
                steam_root,
                &format!("catalog_add=inbox empty path={}", path.display()),
            );
        }
        // 处理完挪到 done, 避免反复触发.
        let done_dir = dir.join("done");
        let _ = std::fs::create_dir_all(&done_dir);
        if let Some(name) = path.file_name() {
            let dest = done_dir.join(name);
            let _ = std::fs::rename(&path, &dest);
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

    // 定期重扫目录, 好把新建的 .lua 纳入监视.
    let mut rescan_ticks: u32 = 0;
    let mut last_stats = (0u64, 0u64, 0u64, 0usize);
    let mut cef_rearm = CefRearm {
        use_pipe,
        ..CefRearm::default()
    };
    // steamclient64 常比 host 晚加载; init 时没挂上就在 watch 里补.
    let mut package_rearm_ticks: u32 = 0;
    let mut package_attached_logged = stt_steamclient::is_attached();

    loop {
        std::thread::sleep(WATCH_POLL);
        process_inbox(steam_root, state, &mut inbox_seen);
        cef_rearm.tick(steam_root, state);

        // ~2s 一轮: 未 attach 且 catalog_add 开着则重试 package hooks.
        package_rearm_ticks = package_rearm_ticks.wrapping_add(1);
        if !package_attached_logged
            && package_rearm_ticks.is_multiple_of(8)
            && state.tools().is_enabled(ToolId::CatalogAdd)
        {
            let report =
                stt_steamclient::try_install_package_hooks(&state.tools(), &patterns);
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

        // 商店注入诊断: 有变化才写 log.
        let stats = stt_steamui::store_native_stats();
        if stats != last_stats && (stats.0 > 0 || stats.1 > 0 || stats.2 > 0 || stats.3 > 0) {
            let ext = stt_steamui::store_native_stats_ext();
            append_host_log(
                steam_root,
                &format!(
                    "store_native_stats ctor={} exec={} posturl={} inject={} skip={} windows={}",
                    ext.0, ext.1, ext.2, ext.3, ext.4, ext.5
                ),
            );
            last_stats = stats;
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
            match state.load_host_from_steam_root(steam_root) {
                Ok(()) => {
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
                    on_rules_reloaded(steam_root, state);
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
                on_rules_reloaded(steam_root, state);
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
            on_rules_reloaded(steam_root, state);
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

    #[test]
    fn placeholder_init_empty() {
        let rules = init_placeholder();
        assert_eq!(rules.epoch(), 0);
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
        let dir = std::env::temp_dir().join(format!(
            "steamtools-host-catalog-disabled-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let state = ConfigState::new();

        let error = add_from_config(&state, &dir, 42).unwrap_err();

        assert!(error.to_string().contains("disabled"));
        assert!(!stt_config::catalog_lua_path(&dir, 42).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mock_provider_requires_explicit_mode() {
        let dir = Path::new("unused");
        let mut config = CatalogSection::default();
        assert!(build_catalog_provider(dir, &config).is_err());

        config.mode = CatalogMode::Mock;
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
}
