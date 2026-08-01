//! steamui 侧库 UX: 状态机 + 可降级安装规划.
//!
//! 不在此 crate 写死 CSteamApp 偏移; 业务 detour 待布局确认后再挂.
//!
//! 失败一律建模成状态报告 (`LibraryUxInstallStatus` / `CefDebugStatus`) 而不是
//! `Result`: 缺 pattern / 缺符号都属于"该层降级", 宿主照常起.

mod cdp_bridge;
mod cdp_pipe;
mod cef_debug;
mod config_panel;
mod install;
mod library_detour;
mod library_ux;
mod store_debug;
mod store_inject;

pub use library_detour::{
    is_attached as library_detour_attached, library_detour_stats, register_ux,
    try_install_library_detours, C_APP_OVERVIEW_CHANGE_REMOVED_APPID, C_STEAM_APP_APP_STATE_FLAGS,
    C_STEAM_APP_OWNERSHIP_FLAGS, C_STEAM_APP_PURCHASED_TIME, E_APP_STATE_UNINSTALLED,
};

pub use cdp_bridge::{
    cdp_store_inject_js, poll_store_cdp, poll_store_cdp_default, run_store_cdp_loop,
    run_store_cdp_loop_with_js, store_button_result_js, store_missing_key_warn_js,
    store_teardown_js, StoreCdpPoll, CDP_STORE_INJECT_JS, STORE_TEARDOWN_JS,
};
pub use cdp_pipe::{poll_store_pipe, run_store_pipe_loop, CdpPipeSession, PipeTarget};
pub use cef_debug::{
    cef_debug_rewrites, cef_debug_stats, install_cef_debug_hook, pipe_armed, take_devtools_pipe,
    take_launch_snapshot, wait_cef_debug_hook, CefDebugReport, CefDebugStatus,
};
pub use config_panel::{
    managed_apps_js, nav_tick_js, panel_update_js, parse_panel_tick, snapshot_json, PanelBridge,
    PanelState, PanelTick, ViewRole, LIBRARY_MENU_JS, LIBRARY_MENU_TEARDOWN_JS, NAV_TICK_JS,
    PANEL_JS,
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
