//! Host DLL entry (`SteamTools.dll`).
//!
//! DllMain only kicks a worker thread; real init runs there.

use std::path::Path;
use std::time::Duration;

use stt_config::{ConfigState, HostConfig, ToolId};
use stt_core::AppRules;

const WATCH_DEBOUNCE: Duration = Duration::from_millis(500);
const WATCH_POLL: Duration = Duration::from_millis(250);

pub fn init_placeholder() -> AppRules {
    AppRules::new()
}

/// Load host toml into a new `ConfigState`.
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

/// Init: data dir, config, lua scan, host.log, then poll watchers (blocks).
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
    log_pattern_probe(steam_root);
    match stt_hook::run_harmless_self_test() {
        Ok(n) => append_host_log(steam_root, &format!("hook_self_test=ok calls={n}")),
        Err(e) => append_host_log(steam_root, &format!("hook_self_test=err {e}")),
    }

    run_watch_loop(steam_root, &state);
    Ok(())
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

fn log_pattern_probe(steam_root: &Path) {
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
}

fn run_watch_loop(steam_root: &Path, state: &ConfigState) {
    let mut toml_watch = stt_config::host_toml_watcher(steam_root, WATCH_DEBOUNCE);
    let mut lua_watch = {
        let host = state.host();
        stt_config::lua_files_watcher(steam_root, &host, WATCH_DEBOUNCE)
    };

    // Rescan directory listing periodically so newly created .lua files are tracked.
    let mut rescan_ticks: u32 = 0;

    loop {
        std::thread::sleep(WATCH_POLL);

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
