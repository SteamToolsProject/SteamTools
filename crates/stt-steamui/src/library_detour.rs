//! steamui 库 UX 业务 detour (U1: FillIn / RunFrame / BuildComplete).
//!
//! 偏移由本机 SteamUI.dll (sha 55a42fc0…) IDA 实测钉定:
//!   CSteamApp::OwnershipFlags = +28, PurchasedTime = +44, AppStateFlags = +60
//!   CAppOverview_Change::removed_appid = +48 (RepeatedField<u32>: size/cap/elements)
//! 符号 RVA 来自 pattern 缓存 (exact SHA 门禁); 入口签名未校验前不 attach.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::Mutex;

use stt_config::{ToolId, ToolRegistry};
use stt_core::AppId;
use stt_hook::InlineHook;
use stt_metadata::PatternStore;
use stt_platform::module_info;

use crate::install::{
    plan_library_ux_install, LibraryUxInstallReport, LibraryUxInstallStatus, LIBRARY_UX_SYMBOLS,
};
use crate::library_ux::{LibraryUx, RemovalDrainAction};

/// CSteamApp::OwnershipFlags (IDA 实测: GetOwnershipFlags 返回 a1+28).
pub const C_STEAM_APP_OWNERSHIP_FLAGS: usize = 28;
/// CSteamApp::PurchasedTime (IDA 实测: GetPurchaseTime 返回 a1+44).
pub const C_STEAM_APP_PURCHASED_TIME: usize = 44;
/// CSteamApp::AppStateFlags (IDA 实测: GetAppState 返回 a1+60).
pub const C_STEAM_APP_APP_STATE_FLAGS: usize = 60;
/// CAppOverview_Change::removed_appid (RepeatedField<u32>, 布局 size/cap/elements).
pub const C_APP_OVERVIEW_CHANGE_REMOVED_APPID: usize = 48;
/// k_EAppStateUninstalled.
pub const E_APP_STATE_UNINSTALLED: u32 = 1;
/// EAppChangeFlags::AppInfoOrConfig (RunFrame 原调用点实测传 2).
const E_APP_CHANGE_FLAGS_APP_INFO_OR_CONFIG: u64 = 2;

static FN_RUN_FRAME: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static FN_FILL_IN: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static FN_BUILD_COMPLETE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static FN_GET_APP_BY_ID: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static FN_MARK_APP_CHANGE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static FN_REPEATED_ADD: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// MarkAppChange 的 this (CUpdateManager*), 首次调用时捕获.
static APP_CHANGE_SOURCE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

struct LibraryHooks {
    run_frame: InlineHook,
    fill_in: InlineHook,
    build_complete: InlineHook,
    mark_change: InlineHook,
}

// InlineHook 仅含本进程地址; attach/detach 由 HOOKS 锁串行.
unsafe impl Send for LibraryHooks {}

static HOOKS: Mutex<Option<LibraryHooks>> = Mutex::new(None);
static ATTACHED: AtomicBool = AtomicBool::new(false);
static RUN_FRAME_HITS: AtomicU64 = AtomicU64::new(0);
static DRAINED: AtomicU64 = AtomicU64::new(0);
static FILL_IN_HITS: AtomicU64 = AtomicU64::new(0);
static BUILD_COMPLETE_HITS: AtomicU64 = AtomicU64::new(0);
static MARK_CHANGE_HITS: AtomicU64 = AtomicU64::new(0);

static UX: std::sync::OnceLock<&'static LibraryUx> = std::sync::OnceLock::new();

/// host 注入全局库 UX 状态机 (init 时一次).
pub fn register_ux(ux: &'static LibraryUx) {
    let _ = UX.set(ux);
}

fn ux() -> Option<&'static LibraryUx> {
    UX.get().copied()
}

/// 供 host.log / 工具中心统计: (run_frame, drained, fill_in, build_complete, mark_change).
pub fn library_detour_stats() -> (u64, u64, u64, u64, u64) {
    (
        RUN_FRAME_HITS.load(Ordering::Relaxed),
        DRAINED.load(Ordering::Relaxed),
        FILL_IN_HITS.load(Ordering::Relaxed),
        BUILD_COMPLETE_HITS.load(Ordering::Relaxed),
        MARK_CHANGE_HITS.load(Ordering::Relaxed),
    )
}

pub fn is_attached() -> bool {
    ATTACHED.load(Ordering::SeqCst)
}

fn library_detour_enabled_by_env() -> bool {
    match std::env::var("STEAMTOOLS_LIBRARY_UX_DETOUR") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no" | "logic")
        }
        Err(_) => true,
    }
}

// x64 MSVC 成员函数 = fastcall 寄存器布局, 显式 extern "C".
type RunFrameFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FillInFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void;
type BuildCompleteFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void;
type GetAppByIDFn = unsafe extern "C" fn(*mut c_void, u32, u8) -> *mut c_void;
type MarkAppChangeFn = unsafe extern "C" fn(*mut c_void, u32, u64) -> *mut c_void;
type RepeatedFieldAddFn = unsafe extern "C" fn(*mut c_void, *const u32) -> *mut c_void;

/// 规划并在可能时 attach; 返回最终报告 (含 HooksAttached).
pub fn try_install_library_detours(
    tools: &ToolRegistry,
    patterns: &PatternStore,
    ux: &'static LibraryUx,
) -> LibraryUxInstallReport {
    register_ux(ux);
    let mut report = plan_library_ux_install(tools, patterns, "steamui");
    if report.status != LibraryUxInstallStatus::LogicOnly {
        return report;
    }
    if !library_detour_enabled_by_env() {
        return report.with_detail("env STEAMTOOLS_LIBRARY_UX_DETOUR disables attach");
    }
    if !tools.is_enabled(ToolId::LibraryUx) {
        return report;
    }

    let Some(info) = module_info("steamui.dll") else {
        return report.with_detail("steamui.dll not loaded yet");
    };
    let base = info.base as usize;
    let mut addrs: Vec<(&str, usize)> = Vec::with_capacity(LIBRARY_UX_SYMBOLS.len());
    let mut need_scan: Vec<&str> = Vec::new();
    for &name in LIBRARY_UX_SYMBOLS {
        match patterns.find_by_rva_only("steamui", name, base, info.size) {
            Some(a) => addrs.push((name, a)),
            None => need_scan.push(name),
        }
    }
    if !need_scan.is_empty() {
        // # Safety
        // `info` 来自已映射模块; 仅当缺 RVA 时才拷贝.
        let image = unsafe { stt_platform::read_module_bytes(info) };
        for name in need_scan {
            match patterns.find_in_image("steamui", name, &image, base) {
                Some(a) => addrs.push((name, a)),
                None => {
                    report.status = LibraryUxInstallStatus::SymbolsMissing;
                    report.missing = vec![name.to_string()];
                    return report.with_detail(format!("resolve failed: {name}"));
                }
            }
        }
    }

    let addr_of = |n: &str| -> Option<*mut c_void> {
        addrs
            .iter()
            .find(|(name, _)| *name == n)
            .map(|(_, a)| *a as *mut c_void)
    };
    let Some(run_frame_addr) = addr_of("CSteamUIAppControllerRunFrame") else {
        report.status = LibraryUxInstallStatus::SymbolsMissing;
        return report.with_detail("CSteamUIAppControllerRunFrame addr missing");
    };
    let Some(fill_in_addr) = addr_of("FillInAppOverview") else {
        report.status = LibraryUxInstallStatus::SymbolsMissing;
        return report.with_detail("FillInAppOverview addr missing");
    };
    let Some(build_complete_addr) = addr_of("BuildCompleteAppOverviewChange") else {
        report.status = LibraryUxInstallStatus::SymbolsMissing;
        return report.with_detail("BuildCompleteAppOverviewChange addr missing");
    };
    let Some(get_app_by_id_addr) = addr_of("GetAppByID") else {
        report.status = LibraryUxInstallStatus::SymbolsMissing;
        return report.with_detail("GetAppByID addr missing");
    };
    let Some(mark_change_addr) = addr_of("MarkAppChange") else {
        report.status = LibraryUxInstallStatus::SymbolsMissing;
        return report.with_detail("MarkAppChange addr missing");
    };
    let Some(repeated_add_addr) = addr_of("RepeatedFieldUint32_Add") else {
        report.status = LibraryUxInstallStatus::SymbolsMissing;
        return report.with_detail("RepeatedFieldUint32_Add addr missing");
    };

    FN_RUN_FRAME.store(run_frame_addr, Ordering::SeqCst);
    FN_FILL_IN.store(fill_in_addr, Ordering::SeqCst);
    FN_BUILD_COMPLETE.store(build_complete_addr, Ordering::SeqCst);
    FN_GET_APP_BY_ID.store(get_app_by_id_addr, Ordering::SeqCst);
    FN_MARK_APP_CHANGE.store(mark_change_addr, Ordering::SeqCst);
    FN_REPEATED_ADD.store(repeated_add_addr, Ordering::SeqCst);

    let mut slot = HOOKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_some() {
        report.status = LibraryUxInstallStatus::HooksAttached;
        return report.with_detail("already attached");
    }

    // # Safety
    // 地址来自 pattern RVA; InlineHook 只改入口 12 字节绝对 jmp.
    let mut run_frame =
        match unsafe { InlineHook::new(run_frame_addr, hk_run_frame as *const c_void) } {
            Ok(h) => h,
            Err(e) => return report.with_detail(format!("RunFrame hook new: {e}")),
        };
    let mut fill_in = match unsafe { InlineHook::new(fill_in_addr, hk_fill_in as *const c_void) } {
        Ok(h) => h,
        Err(e) => return report.with_detail(format!("FillInAppOverview hook new: {e}")),
    };
    let mut build_complete =
        match unsafe { InlineHook::new(build_complete_addr, hk_build_complete as *const c_void) } {
            Ok(h) => h,
            Err(e) => return report.with_detail(format!("BuildComplete hook new: {e}")),
        };
    let mut mark_change =
        match unsafe { InlineHook::new(mark_change_addr, hk_mark_app_change as *const c_void) } {
            Ok(h) => h,
            Err(e) => return report.with_detail(format!("MarkAppChange hook new: {e}")),
        };

    // # Safety
    // 逐个 attach; 失败时回滚已挂的.
    if let Err(e) = unsafe { run_frame.attach() } {
        return report.with_detail(format!("RunFrame attach failed: {e}"));
    }
    if let Err(e) = unsafe { fill_in.attach() } {
        let _ = unsafe { run_frame.detach() };
        return report.with_detail(format!("FillInAppOverview attach failed: {e}"));
    }
    if let Err(e) = unsafe { build_complete.attach() } {
        let _ = unsafe { run_frame.detach() };
        let _ = unsafe { fill_in.detach() };
        return report.with_detail(format!("BuildComplete attach failed: {e}"));
    }
    if let Err(e) = unsafe { mark_change.attach() } {
        let _ = unsafe { run_frame.detach() };
        let _ = unsafe { fill_in.detach() };
        let _ = unsafe { build_complete.detach() };
        return report.with_detail(format!("MarkAppChange attach failed: {e}"));
    }

    *slot = Some(LibraryHooks {
        run_frame,
        fill_in,
        build_complete,
        mark_change,
    });
    ATTACHED.store(true, Ordering::SeqCst);
    report.status = LibraryUxInstallStatus::HooksAttached;
    report.with_detail(
        "attached RunFrame+FillInAppOverview+BuildCompleteAppOverviewChange+MarkAppChange",
    )
}

/// 卸补丁 → 调原入口 → 再挂上. 持 HOOKS 锁, 避免并发补丁竞态.
///
/// 卸钩失败时 fail-closed: 不调用原函数, 返回 None; 恢复补丁再失败则把
/// ATTACHED 置 false, 后续调用按未挂补丁直通原入口.
///
/// # Safety
/// `f` 必须是已 resolve 的原函数入口; 调用期间补丁已卸.
unsafe fn call_while_unhooked<R>(f: impl FnOnce() -> R) -> Option<R> {
    let mut slot = HOOKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(hooks) = slot.as_mut() {
        // 逐个卸补丁 (只卸仍挂着的); 任一失败立即 fail-closed, 不再调原函数.
        let mut detached: Vec<&mut InlineHook> = Vec::with_capacity(4);
        let mut detach_failed = false;
        for h in [
            &mut hooks.run_frame,
            &mut hooks.fill_in,
            &mut hooks.build_complete,
            &mut hooks.mark_change,
        ] {
            if !h.is_installed() {
                continue;
            }
            if unsafe { h.detach() }.is_err() {
                detach_failed = true;
                break;
            }
            detached.push(h);
        }
        if detach_failed {
            // 卸钩失败: 原入口可能仍带半补丁, 跳过本次调用; 尽量恢复已卸下的.
            reattach_detached(&mut detached);
            return None;
        }
        let out = f();
        reattach_detached(&mut detached);
        Some(out)
    } else {
        Some(f())
    }
}

/// 把已卸下的补丁逐一挂回; 任一失败置 ATTACHED=false (后续调用直通原入口).
fn reattach_detached(detached: &mut [&mut InlineHook]) {
    for h in detached {
        // # Safety
        // 同 InlineHook::attach 的 Safety 要求; 持锁期间无其它写者.
        if unsafe { h.attach() }.is_err() {
            ATTACHED.store(false, Ordering::SeqCst);
        }
    }
}

/// # Safety
/// MarkAppChange detour: 首次调用捕获 this (CUpdateManager*), 供移除 drain 通知 UI.
///
/// # Safety
/// 签名与 steamui MarkAppChange 一致 (fastcall: rcx=this, rdx=app_id, r8=flags).
unsafe extern "C" fn hk_mark_app_change(
    source: *mut c_void,
    app_id: u32,
    flags: u64,
) -> *mut c_void {
    MARK_CHANGE_HITS.fetch_add(1, Ordering::Relaxed);
    if !source.is_null() {
        APP_CHANGE_SOURCE.store(source, Ordering::SeqCst);
    }
    let target = FN_MARK_APP_CHANGE.load(Ordering::SeqCst);
    if target.is_null() {
        return std::ptr::null_mut();
    }
    // # Safety
    // 卸补丁后 target 即原入口; 补丁已摘, 不会再进本 detour.
    unsafe {
        call_while_unhooked(|| {
            let f: MarkAppChangeFn = std::mem::transmute(target);
            f(source, app_id, flags)
        })
        .unwrap_or(std::ptr::null_mut())
    }
}

/// 作为 CSteamUIAppController::RunFrame detour; this 是 controller.
unsafe extern "C" fn hk_run_frame(controller: *mut c_void) -> *mut c_void {
    RUN_FRAME_HITS.fetch_add(1, Ordering::Relaxed);
    if let Some(ux) = ux() {
        let pending = ux.take_pending_removals();
        if !pending.is_empty() {
            DRAINED.fetch_add(pending.len() as u64, Ordering::Relaxed);
            drain_removals(ux, controller, &pending);
        }
    }
    let target = FN_RUN_FRAME.load(Ordering::SeqCst);
    if target.is_null() {
        return std::ptr::null_mut();
    }
    // # Safety
    // 卸补丁后 target 即原入口.
    unsafe {
        call_while_unhooked(|| {
            let f: RunFrameFn = std::mem::transmute(target);
            f(controller)
        })
        .unwrap_or(std::ptr::null_mut())
    }
}

/// RunFrame 内 drain: 清 OwnershipFlags + 记 removed + MarkAppChange.
fn drain_removals(ux: &LibraryUx, controller: *mut c_void, pending: &[AppId]) {
    let get_app = FN_GET_APP_BY_ID.load(Ordering::SeqCst);
    let mark = FN_MARK_APP_CHANGE.load(Ordering::SeqCst);
    if get_app.is_null() || mark.is_null() || controller.is_null() {
        return;
    }
    for &app_id in pending {
        // 配置里又受管: 跳过 (对标 IsOwned).
        if ux.is_owned(app_id) {
            continue;
        }
        let action = LibraryUx::decide_removal(false, false);
        let mark_removed = matches!(
            action,
            RemovalDrainAction::ClearOwnership {
                mark_removed_if_uninstalled: true
            }
        );
        // # Safety
        // 未 hook 原函数, 直接调用.
        let p_app = unsafe {
            let f: GetAppByIDFn = std::mem::transmute(get_app);
            f(controller, app_id, 0)
        };
        if !p_app.is_null() {
            // # Safety
            // p_app 为 CSteamApp* (560 字节对象); 偏移已钉.
            unsafe {
                write_u32(p_app, C_STEAM_APP_OWNERSHIP_FLAGS, 0);
            }
            if mark_removed {
                let state = unsafe { read_u32(p_app, C_STEAM_APP_APP_STATE_FLAGS) };
                if state == E_APP_STATE_UNINSTALLED {
                    ux.mark_removed(app_id);
                }
            }
        }
        // # Safety
        // source 由 MarkAppChange detour 捕获 (对标 CAPTURE_THIS); 调用前卸补丁.
        let source = APP_CHANGE_SOURCE.load(Ordering::Relaxed);
        if !source.is_null() {
            unsafe {
                call_while_unhooked(|| {
                    let f: MarkAppChangeFn = std::mem::transmute(mark);
                    f(source, app_id, E_APP_CHANGE_FLAGS_APP_INFO_OR_CONFIG)
                });
            }
        }
    }
}

/// # Safety
/// 作为 CSteamUIAppController::FillInAppOverview detour; a3 是 CSteamApp*.
unsafe extern "C" fn hk_fill_in(
    controller: *mut c_void,
    overview: *mut c_void,
    p_app: *mut c_void,
) -> *mut c_void {
    FILL_IN_HITS.fetch_add(1, Ordering::Relaxed);
    if !p_app.is_null() {
        if let Some(ux) = ux() {
            // CSteamApp::nAppID 取自 vtable 第 0 方法: 先解引用对象取 vptr,
            // 再解引用 vptr 取第 0 槽 (直接调用 vptr 会跳进 .rdata 数据段 = AV).
            let vtable = *(p_app as *const *const c_void);
            if !vtable.is_null() {
                let first_slot = *(vtable as *const *const c_void);
                let get_app_id: unsafe extern "C" fn(*mut c_void) -> u32 =
                    std::mem::transmute::<*const c_void, _>(first_slot);
                let app_id = unsafe { get_app_id(p_app) };
                if let Some(t) = ux.purchase_time(app_id) {
                    // # Safety
                    // 偏移已钉 (GetPurchaseTime 返回 a1+44).
                    unsafe {
                        write_u32(p_app, C_STEAM_APP_PURCHASED_TIME, t);
                    }
                }
            }
        }
    }
    let target = FN_FILL_IN.load(Ordering::SeqCst);
    if target.is_null() {
        return std::ptr::null_mut();
    }
    // # Safety
    // 卸补丁后 target 即原入口.
    unsafe {
        call_while_unhooked(|| {
            let f: FillInFn = std::mem::transmute(target);
            f(controller, overview, p_app)
        })
        .unwrap_or(std::ptr::null_mut())
    }
}

/// # Safety
/// 作为 BuildCompleteAppOverviewChange detour; a2 是 CAppOverview_Change*.
unsafe extern "C" fn hk_build_complete(
    controller: *mut c_void,
    change: *mut c_void,
    slot: *mut c_void,
) -> *mut c_void {
    BUILD_COMPLETE_HITS.fetch_add(1, Ordering::Relaxed);
    let target = FN_BUILD_COMPLETE.load(Ordering::SeqCst);
    let result = if target.is_null() {
        std::ptr::null_mut()
    } else {
        // # Safety
        // 卸补丁后 target 即原入口.
        unsafe {
            call_while_unhooked(|| {
                let f: BuildCompleteFn = std::mem::transmute(target);
                f(controller, change, slot)
            })
        }
        .unwrap_or(std::ptr::null_mut())
    };

    // 全量重建后重注入 removed_appid, 避免已移除 app 闪回.
    let repeated_add = FN_REPEATED_ADD.load(Ordering::SeqCst);
    if !change.is_null() && !repeated_add.is_null() {
        if let Some(ux) = ux() {
            let removed = ux.removed_app_ids_snapshot();
            if !removed.is_empty() {
                // # Safety
                // change+48 是 removed_appid RepeatedField<u32> (布局已钉).
                let field =
                    (change as *mut u8).add(C_APP_OVERVIEW_CHANGE_REMOVED_APPID) as *mut c_void;
                for id in removed {
                    unsafe {
                        let f: RepeatedFieldAddFn = std::mem::transmute(repeated_add);
                        f(field, &id);
                    }
                }
            }
        }
    }
    result
}

/// # Safety
/// `base` 非空且 `off` 落在可读对象内.
unsafe fn read_u32(base: *mut c_void, off: usize) -> u32 {
    if base.is_null() {
        return 0;
    }
    std::ptr::read_unaligned((base as *const u8).add(off) as *const u32)
}

/// # Safety
/// `base` 非空且 `off` 落在可写对象内.
unsafe fn write_u32(base: *mut c_void, off: usize, v: u32) {
    if base.is_null() {
        return;
    }
    std::ptr::write_unaligned((base as *mut u8).add(off) as *mut u32, v);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_default_allows_attach() {
        let _ = library_detour_enabled_by_env();
    }

    #[test]
    fn layout_constants_match_recon() {
        // 与 IDA 实测保持一致, 防止误改.
        assert_eq!(C_STEAM_APP_OWNERSHIP_FLAGS, 28);
        assert_eq!(C_STEAM_APP_PURCHASED_TIME, 44);
        assert_eq!(C_STEAM_APP_APP_STATE_FLAGS, 60);
        assert_eq!(C_APP_OVERVIEW_CHANGE_REMOVED_APPID, 48);
        assert_eq!(E_APP_STATE_UNINSTALLED, 1);
    }
}
