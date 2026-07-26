//! 宿主 DLL 入口 (`SteamTools.dll`).
//!
//! DllMain 只起工作线程, 真正初始化在线程里做.

// crate 名与导出函数名都由产物 DLL 决定 (SteamTools.dll / DllMain), 不能蛇形.
// 这里只能用 allow: crate 名的 non_snake_case 不被 expect 追踪, 写 expect 反而
// 会报 unfulfilled_lint_expectations.
#![allow(non_snake_case)]

use std::path::Path;
use std::time::Duration;

use stt_config::{add_to_library, ConfigState, HostConfig, MockCatalogProvider, ToolId};
use stt_core::AppRules;

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
    let enabled: Vec<&str> = [ToolId::CatalogAdd, ToolId::LibraryUx, ToolId::StoreAccel]
        .into_iter()
        .filter(|id| tools.is_enabled(*id))
        .map(ToolId::as_str)
        .collect();
    format!("tools_enabled={}\n", enabled.join(","))
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
    let catalog_on = state.tools().is_enabled(ToolId::CatalogAdd);
    let use_pipe = !stt_platform::cef_pipe_fallback_marker(steam_root).is_file();
    let cef = stt_steamui::wait_cef_debug_hook(catalog_on, use_pipe, Duration::from_millis(2000));

    let lua_report = state.reload_lua_dirs(steam_root);

    let cfg_path = HostConfig::resolve_path(steam_root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(defaults)".into());

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
         status=init complete\n\
         watch=started debounce_ms={debounce_ms} poll_ms={poll_ms}\n",
        steam_root = steam_root.display(),
        data_dir = data.display(),
        cfg_path = cfg_path,
        tools = tools_enabled_line(&state),
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
    log_library_ux_plan(steam_root, &state, &patterns);
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

    // 点击回传: 本机 HTTP 桥 (商店页 fetch, 不依赖 CEF 8080).
    {
        let root = steam_root.to_path_buf();
        let state_cb = state.clone();
        stt_steamui::ensure_click_bridge(move |app_id| {
            let provider = MockCatalogProvider::new().with_auto_generate(true);
            match add_to_library(&state_cb, &root, &provider, app_id) {
                Ok(out) => append_host_log(
                    &root,
                    &format!(
                        "catalog_add=ok source=store_btn app_id={app_id} provider={} lua={} epoch={} owned={}",
                        out.provider_id,
                        out.lua_path.display(),
                        out.epoch,
                        out.owned_count
                    ),
                ),
                Err(e) => append_host_log(
                    &root,
                    &format!("catalog_add=err source=store_btn app_id={app_id} {e}"),
                ),
            }
        });
        append_host_log(
            steam_root,
            &format!(
                "catalog_add=click_bridge port={}",
                stt_steamui::click_bridge_port()
            ),
        );
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
    spawn_store_cdp_bridge(steam_root, &state, use_pipe);
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

    run_watch_loop(steam_root, &state, use_pipe);
    Ok(())
}

/// 后台: CEF CDP 向商店页注入按钮; 点击走 click_bridge, pending 队列作兜底.
///
/// `use_pipe` 时先试无端口的管道通道; 它明确走不通才落标记文件并回退到端口,
/// 这样最多一个会话入库不可用, 不会永久卡死.
fn spawn_store_cdp_bridge(steam_root: &Path, state: &ConfigState, use_pipe: bool) {
    let root = steam_root.to_path_buf();
    let state = state.clone();
    let _ = std::thread::Builder::new()
        .name("stt-store-cdp".into())
        .spawn(move || {
            let provider = MockCatalogProvider::new().with_auto_generate(true);
            let mut make_js = || {
                // 短脚本: 大 STORE_INJECT_JS 在 CEF evaluate 上易挂起.
                let port = stt_steamui::click_bridge_port();
                stt_steamui::cdp_store_inject_js(port)
            };
            let mut on_app = |app_id: u32| {
                match add_to_library(&state, &root, &provider, app_id) {
                    Ok(out) => append_host_log(
                        &root,
                        &format!(
                            "catalog_add=ok source=store_cdp app_id={app_id} provider={} lua={} epoch={} owned={}",
                            out.provider_id,
                            out.lua_path.display(),
                            out.epoch,
                            out.owned_count
                        ),
                    ),
                    Err(e) => append_host_log(
                        &root,
                        &format!("catalog_add=err source=store_cdp app_id={app_id} {e}"),
                    ),
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
                make_js,
                on_app,
                on_log,
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
) {
    let tools = state.tools();
    let report = stt_steamui::plan_library_ux_install(&tools, patterns, "steamui");
    append_host_log(steam_root, &report.summary_line());
    // 配置里已有的 app 取消移除标记 (纯逻辑, 无 detour).
    let ux = stt_steamui::LibraryUx::new();
    state.with_rules(|rules| {
        for app_id in rules.owned_iter() {
            ux.on_rules_app_present(app_id);
        }
    });
}

/// 处理 steamtools/inbox/*.txt: 每行一个 app_id, 走 Mock AddToLibrary.
fn process_inbox(
    steam_root: &Path,
    state: &ConfigState,
    provider: &MockCatalogProvider,
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
            match add_to_library(state, steam_root, provider, app_id) {
                Ok(out) => append_host_log(
                    steam_root,
                    &format!(
                        "catalog_add=ok app_id={app_id} provider={} lua={} epoch={} owned={}",
                        out.provider_id,
                        out.lua_path.display(),
                        out.epoch,
                        out.owned_count
                    ),
                ),
                Err(e) => {
                    append_host_log(steam_root, &format!("catalog_add=err app_id={app_id} {e}"))
                }
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
        if self.use_pipe {
            "pipe".to_owned()
        } else {
            stt_steamui::cdp_host_port()
        }
    }
    /// 这么久还没截到就写诊断 (~20s), 够覆盖冷启动.
    const WARN_AFTER: u32 = 80;

    fn tick(&mut self, steam_root: &Path, state: &ConfigState) {
        self.ticks += 1;
        if !self.ticks.is_multiple_of(Self::EVERY) {
            return;
        }
        let r = stt_steamui::install_cef_debug_hook(
            state.tools().is_enabled(ToolId::CatalogAdd),
            self.use_pipe,
        );
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

fn run_watch_loop(steam_root: &Path, state: &ConfigState, use_pipe: bool) {
    let mut toml_watch = stt_config::host_toml_watcher(steam_root, WATCH_DEBOUNCE);
    let mut lua_watch = {
        let host = state.host();
        stt_config::lua_files_watcher(steam_root, &host, WATCH_DEBOUNCE)
    };
    let provider = MockCatalogProvider::new().with_auto_generate(true);
    let mut inbox_seen: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();

    // 定期重扫目录, 好把新建的 .lua 纳入监视.
    let mut rescan_ticks: u32 = 0;
    let mut last_stats = (0u64, 0u64, 0u64, 0usize);
    let mut cef_rearm = CefRearm {
        use_pipe,
        ..CefRearm::default()
    };

    loop {
        std::thread::sleep(WATCH_POLL);
        process_inbox(steam_root, state, &provider, &mut inbox_seen);
        cef_rearm.tick(steam_root, state);

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
                        &format!("reload=host_toml {}", tools_enabled_line(state).trim()),
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
}
