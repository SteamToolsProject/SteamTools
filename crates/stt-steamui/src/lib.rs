//! steamui 侧库 UX: 状态机 + 可降级安装规划.
//!
//! 不在此 crate 写死 CSteamApp 偏移; 业务 detour 待布局确认后再挂.
//!
//! 失败一律建模成状态报告 (`LibraryUxInstallStatus` / `StoreNativeStatus`) 而不是
//! `Result`: 缺 pattern / 缺符号都属于"该层降级", 宿主照常起.

mod cdp_bridge;
mod cdp_pipe;
mod cef_debug;
mod click_bridge;
mod install;
mod library_ux;
mod store_debug;
mod store_inject;
mod store_native;

pub use cdp_bridge::{
    cdp_store_inject_js, poll_store_cdp, poll_store_cdp_default, run_store_cdp_loop,
    run_store_cdp_loop_with_js, StoreCdpPoll, CDP_STORE_INJECT_JS,
};
pub use cdp_pipe::{poll_store_pipe, run_store_pipe_loop, CdpPipeSession, PipeTarget};
pub use cef_debug::{
    cef_debug_rewrites, cef_debug_stats, install_cef_debug_hook, pipe_armed, take_devtools_pipe,
    take_launch_snapshot, wait_cef_debug_hook, CefDebugReport, CefDebugStatus,
};
pub use click_bridge::{
    click_bridge_port, click_bridge_token, ensure_click_bridge, store_inject_js_with_bridge,
};
pub use install::{
    plan_library_ux_install, LibraryUxInstallReport, LibraryUxInstallStatus,
    LIBRARY_UX_HOOK_SYMBOLS, LIBRARY_UX_SYMBOLS,
};
pub use library_ux::{LibraryUx, RemovalDrainAction};
pub use store_debug::{
    alloc_cef_debug_port, cdp_host_port, cef_debug_port, is_webhelper_launch,
    rewrite_webhelper_cmdline, session_port_live, DebugChannel, LEGACY_CDP_PORT,
};
pub use store_inject::{app_id_from_store_path, STORE_INJECT_JS};
pub use store_native::{
    builtin_steamui_pattern, ensure_builtin_steamui_pattern, is_observing, remembered_window_count,
    store_native_stats, store_native_stats_ext, try_install_store_native,
    try_install_store_native_with_mode, StoreNativeMode, StoreNativeReport, StoreNativeStatus,
    STORE_CTOR_SYMBOL, STORE_DTOR_SYMBOL, STORE_INJECT_SYMBOL, STORE_POSTURL_SYMBOL,
};
