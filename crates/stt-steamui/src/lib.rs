//! steamui 侧库 UX: 状态机 + 可降级安装规划.
//!
//! 不在此 crate 写死 CSteamApp 偏移; 业务 detour 待布局确认后再挂.
//!
//! 失败一律建模成状态报告 (`LibraryUxInstallStatus`) 而不是 `Result`:
//! 缺 pattern / 缺符号都属于"该层降级", 宿主照常起.

mod install;
mod library_ux;

pub use install::{
    plan_library_ux_install, LibraryUxInstallReport, LibraryUxInstallStatus,
    LIBRARY_UX_HOOK_SYMBOLS, LIBRARY_UX_SYMBOLS,
};
pub use library_ux::{LibraryUx, RemovalDrainAction};
