//! steamclient package hooks (可降级 attach).
//!
//! 默认在 P0 符号齐时尝试挂 `CheckAppOwnership` + capture `GetPackageInfo`.
//! `STEAMTOOLS_PACKAGE=off` 可强制只跑纯逻辑.

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use stt_config::{ToolId, ToolRegistry};
use stt_core::AppId;
use stt_hook::InlineHook;
use stt_metadata::PatternStore;
use stt_platform::module_info;

use crate::install::{
    plan_package_install, PackageInstallReport, PackageInstallStatus, PACKAGE_P0_SYMBOLS,
};
use crate::layout::{
    app_ownership, package_info, utl_vector, APP_RELEASE_STATE_RELEASED,
    INJECTED_PACKAGE_ACCESS_TOKEN, INJECTED_PACKAGE_ID, PACKAGE_STATUS_AVAILABLE,
};
use crate::license::{LicenseNotifyPlan, LicenseQueue, UiLicenseAction};
use crate::ownership::{decide_ownership_rewrite, ForgedOwnershipFields, OwnershipRewrite};

// 不用 TrampolineHook 调原函数: detour 里 CALL trampoline 会多压 8 字节,
// 序言 mov rax,rsp / mov [rsp+disp] 会写坏调用方栈 (实机 steam 必崩).
// 改为 InlineHook + 短暂卸补丁后直接 CALL 原入口 (与可靠 detours 的 "call through" 等价且栈正确).

static FN_CHECK: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static FN_GET_PACKAGE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static FN_GROW: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static FN_MARK: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static FN_PROCESS: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

static C_USER: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static C_PACKAGE_INFO: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static INJECTED_PKG: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

struct PackageHooks {
    check: InlineHook,
    get_package: InlineHook,
}

// InlineHook 仅含本进程地址; attach/detach 由 HOOKS 锁串行.
unsafe impl Send for PackageHooks {}

static HOOKS: Mutex<Option<PackageHooks>> = Mutex::new(None);
static ATTACHED: AtomicBool = AtomicBool::new(false);
static CHECK_HITS: AtomicU64 = AtomicU64::new(0);
static FORGE_HITS: AtomicU64 = AtomicU64::new(0);
static PACKAGE0_HITS: AtomicU64 = AtomicU64::new(0);
static PACKAGE0_STATUS: AtomicU32 = AtomicU32::new(u32::MAX);
/// grow 校验失败计数 (与 PACKAGE0_HITS 同模式的统计上报, 供诊断).
pub(crate) static APPEND_FAILURES: AtomicU64 = AtomicU64::new(0);

static RUNTIME: OnceLock<PackageRuntime> = OnceLock::new();

/// UI 联动回调 (host 注入).
type UiActionHandler = Box<dyn Fn(UiLicenseAction) + Send>;

struct PackageRuntime {
    queue: Arc<LicenseQueue>,
    /// 配置内 app (owned / 应拦截 CheckAppOwnership).
    configured: Arc<RwLock<HashSet<AppId>>>,
    on_ui: Mutex<Option<UiActionHandler>>,
}

// x64 MSVC thiscall 等价于 fastcall: rcx=this, rdx/r8/… 其余参数.
// Steam client 成员函数用此约定; 与 `extern "C"` 在 x64 Windows 上寄存器布局相同,
// 这里显式写 C 以匹配 stt-steamui 既有 hook 风格.
type CheckAppOwnershipFn = unsafe extern "C" fn(*mut c_void, u32, *mut u8) -> u8;
type GetPackageInfoFn = unsafe extern "C" fn(*mut c_void, u32, u64) -> *mut c_void;
type CUtlMemoryGrowFn = unsafe extern "C" fn(*mut c_void, i32) -> *mut c_void;
type MarkLicenseFn = unsafe extern "C" fn(*mut c_void, u32, u8) -> i64;
type ProcessPendingFn = unsafe extern "C" fn(*mut c_void) -> u8;

fn package_hooks_enabled_by_env() -> bool {
    match std::env::var("STEAMTOOLS_PACKAGE") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "no" | "logic")
        }
        Err(_) => true,
    }
}

/// 进程内共享运行时 (host init 时注册一次).
///
/// host 可能先把 id seed 进 `configured` Arc 再 register;
/// 这里必须 refresh 非空闸门, 否则 CheckAppOwnership 永远 forged=0.
pub fn register_runtime(queue: Arc<LicenseQueue>, configured: Arc<RwLock<HashSet<AppId>>>) {
    let _ = RUNTIME.set(PackageRuntime {
        queue,
        configured,
        on_ui: Mutex::new(None),
    });
    refresh_configured_nonempty();
}

/// 注册 UI 联动回调 (CancelRemoval / QueueRemoval).
pub fn set_ui_action_handler(handler: impl Fn(UiLicenseAction) + Send + 'static) {
    if let Some(rt) = RUNTIME.get() {
        let mut g = rt
            .on_ui
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *g = Some(Box::new(handler));
    }
}

pub fn runtime_queue() -> Option<Arc<LicenseQueue>> {
    RUNTIME.get().map(|r| Arc::clone(&r.queue))
}

/// 配置集合非空标志: 热路径先看它, 为空时完全跳过查询与重写.
static CONFIGURED_NONEMPTY: AtomicBool = AtomicBool::new(false);

fn refresh_configured_nonempty() {
    let nonempty = RUNTIME
        .get()
        .map(|rt| {
            !rt.configured
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        })
        .unwrap_or(false);
    CONFIGURED_NONEMPTY.store(nonempty, Ordering::Relaxed);
}

pub fn set_configured_apps(apps: impl IntoIterator<Item = AppId>) {
    if let Some(rt) = RUNTIME.get() {
        let mut g = rt
            .configured
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.clear();
        g.extend(apps);
        drop(g);
        refresh_configured_nonempty();
    }
}

pub fn add_configured_app(app_id: AppId) {
    if let Some(rt) = RUNTIME.get() {
        rt.configured
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(app_id);
        refresh_configured_nonempty();
    }
}

pub fn remove_configured_app(app_id: AppId) {
    if let Some(rt) = RUNTIME.get() {
        rt.configured
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&app_id);
        refresh_configured_nonempty();
    }
}

/// host 直写共享 `configured` HashSet 后调用 (不走 set/add API 时).
pub fn refresh_configured_gate() {
    refresh_configured_nonempty();
}

pub fn is_attached() -> bool {
    ATTACHED.load(Ordering::SeqCst)
}

pub fn hook_stats() -> (u64, u64) {
    (
        CHECK_HITS.load(Ordering::Relaxed),
        FORGE_HITS.load(Ordering::Relaxed),
    )
}

pub fn package_info_stats() -> (u64, Option<u32>) {
    let status = PACKAGE0_STATUS.load(Ordering::Relaxed);
    (
        PACKAGE0_HITS.load(Ordering::Relaxed),
        (status != u32::MAX).then_some(status),
    )
}

fn is_configured(app_id: AppId) -> bool {
    // 热路径快速返回: 无受管 app 时不取读锁.
    if !CONFIGURED_NONEMPTY.load(Ordering::Relaxed) {
        return false;
    }
    RUNTIME
        .get()
        .map(|rt| {
            rt.configured
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&app_id)
        })
        .unwrap_or(false)
}

fn emit_ui(action: UiLicenseAction) {
    if let Some(rt) = RUNTIME.get() {
        let g = rt
            .on_ui
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cb) = g.as_ref() {
            cb(action);
        }
    }
}

pub fn apply_ui_actions(plan: &LicenseNotifyPlan) {
    for a in &plan.ui_actions {
        emit_ui(*a);
    }
}

/// 规划并在可能时 attach; 返回最终报告 (含 HooksAttached).
pub fn try_install_package_hooks(
    tools: &ToolRegistry,
    patterns: &PatternStore,
) -> PackageInstallReport {
    let mut report = plan_package_install(tools, patterns, "steamclient");
    if report.status != PackageInstallStatus::LogicOnly {
        return report;
    }
    if !package_hooks_enabled_by_env() {
        return report.with_detail("env STEAMTOOLS_PACKAGE disables attach");
    }
    if !tools.is_enabled(ToolId::CatalogAdd) {
        return report;
    }

    let Some(info) = module_info("steamclient64.dll") else {
        // pattern 齐但模块尚未映射: 保持 LogicOnly, 供 host 稍后重试.
        return report.with_detail("steamclient64.dll not loaded yet");
    };

    // 优先只用 pattern RVA (本机 sha 已钉), 避免把 ~25MB 映像拷进堆再扫.
    // RVA 缺失时再 fallback 到整模块扫描.
    let base = info.base as usize;
    let mut addrs = Vec::with_capacity(PACKAGE_P0_SYMBOLS.len());
    let mut need_scan: Vec<&str> = Vec::new();
    for &name in PACKAGE_P0_SYMBOLS {
        match patterns.find_by_rva_only("steamclient", name, base, info.size) {
            Some(a) => addrs.push((name, a)),
            None => need_scan.push(name),
        }
    }
    if !need_scan.is_empty() {
        // # Safety
        // `info` 来自已映射模块; 仅当缺 RVA 时才拷贝.
        let image = unsafe { stt_platform::read_module_bytes(info) };
        for name in need_scan {
            match patterns.find_in_image("steamclient", name, &image, base) {
                Some(a) => addrs.push((name, a)),
                None => {
                    report.status = PackageInstallStatus::SymbolsMissing;
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

    let Some(check_addr) = addr_of("CheckAppOwnership") else {
        report.status = PackageInstallStatus::SymbolsMissing;
        return report.with_detail("CheckAppOwnership addr missing after resolve");
    };
    let Some(gpi_addr) = addr_of("GetPackageInfo") else {
        report.status = PackageInstallStatus::SymbolsMissing;
        return report.with_detail("GetPackageInfo addr missing after resolve");
    };
    let Some(grow_addr) = addr_of("CUtlMemoryGrow") else {
        report.status = PackageInstallStatus::SymbolsMissing;
        return report.with_detail("CUtlMemoryGrow addr missing after resolve");
    };
    let Some(mark_addr) = addr_of("MarkLicenseAsChanged") else {
        report.status = PackageInstallStatus::SymbolsMissing;
        return report.with_detail("MarkLicenseAsChanged addr missing after resolve");
    };
    let Some(proc_addr) = addr_of("ProcessPendingLicenseUpdates") else {
        report.status = PackageInstallStatus::SymbolsMissing;
        return report.with_detail("ProcessPendingLicenseUpdates addr missing after resolve");
    };

    FN_CHECK.store(check_addr, Ordering::SeqCst);
    FN_GET_PACKAGE.store(gpi_addr, Ordering::SeqCst);
    FN_GROW.store(grow_addr, Ordering::SeqCst);
    FN_MARK.store(mark_addr, Ordering::SeqCst);
    FN_PROCESS.store(proc_addr, Ordering::SeqCst);

    let mut slot = HOOKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_some() {
        report.status = PackageInstallStatus::HooksAttached;
        return report.with_detail("already attached");
    }

    // # Safety
    // 地址来自 pattern RVA; InlineHook 只改入口 12 字节绝对 jmp.
    let mut get_package =
        match unsafe { InlineHook::new(gpi_addr, hk_get_package_info as *const c_void) } {
            Ok(h) => h,
            Err(e) => return report.with_detail(format!("GetPackageInfo hook new: {e}")),
        };
    if let Err(e) = unsafe { get_package.attach() } {
        return report.with_detail(format!("GetPackageInfo attach failed: {e}"));
    }

    let mut check =
        match unsafe { InlineHook::new(check_addr, hk_check_app_ownership as *const c_void) } {
            Ok(h) => h,
            Err(e) => {
                let _ = unsafe { get_package.detach() };
                return report.with_detail(format!("CheckAppOwnership hook new: {e}"));
            }
        };
    if let Err(e) = unsafe { check.attach() } {
        let _ = unsafe { get_package.detach() };
        return report.with_detail(format!("CheckAppOwnership attach failed: {e}"));
    }

    *slot = Some(PackageHooks { check, get_package });
    ATTACHED.store(true, Ordering::SeqCst);
    report.status = PackageInstallStatus::HooksAttached;
    report.with_detail(
        "attached CheckAppOwnership+GetPackageInfo (inline + unhook-call-rehook original)",
    )
}

/// 卸补丁 → 调原入口 → 再挂上. 持 HOOKS 锁, 避免并发补丁竞态.
///
/// # Safety
/// `f` 必须是已 resolve 的原函数入口; 调用期间补丁已卸.
unsafe fn call_while_unhooked<R>(f: impl FnOnce() -> R) -> R {
    let mut slot = HOOKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(hooks) = slot.as_mut() {
        let _ = hooks.check.detach();
        let _ = hooks.get_package.detach();
        let out = f();
        let _ = hooks.check.attach();
        let _ = hooks.get_package.attach();
        out
    } else {
        f()
    }
}

/// # Safety
/// 作为 GetPackageInfo detour: 捕获 this 后卸补丁调原函数.
unsafe extern "C" fn hk_get_package_info(
    this: *mut c_void,
    package_id: u32,
    access_token: u64,
) -> *mut c_void {
    if !this.is_null() {
        // 捕获指针无顺序要求, Relaxed 即可 (热路径).
        C_PACKAGE_INFO.store(this, Ordering::Relaxed);
    }
    let target = FN_GET_PACKAGE.load(Ordering::SeqCst);
    if target.is_null() {
        return std::ptr::null_mut();
    }
    // # Safety
    // 卸补丁后 target 即原入口.
    let p = unsafe {
        call_while_unhooked(|| {
            let f: GetPackageInfoFn = std::mem::transmute(target);
            f(this, package_id, access_token)
        })
    };
    if package_id == INJECTED_PACKAGE_ID && !p.is_null() {
        INJECTED_PKG.store(p, Ordering::SeqCst);
        PACKAGE0_HITS.fetch_add(1, Ordering::Relaxed);
        // # Safety
        // p 是原 GetPackageInfo 返回的 package0, 布局由 exact-SHA 门禁钉死.
        PACKAGE0_STATUS.store(
            unsafe { read_u32(p as *mut u8, package_info::STATUS) },
            Ordering::Relaxed,
        );
    }
    p
}

/// # Safety
/// 作为 CheckAppOwnership detour; `p_own` 指向至少 APP_OWNERSHIP_SIZE 的可写缓冲.
unsafe extern "C" fn hk_check_app_ownership(this: *mut c_void, app_id: u32, p_own: *mut u8) -> u8 {
    CHECK_HITS.fetch_add(1, Ordering::Relaxed);
    if !this.is_null() {
        // 捕获指针无顺序要求, Relaxed 即可 (热路径).
        C_USER.store(this, Ordering::Relaxed);
    }

    let target = FN_CHECK.load(Ordering::SeqCst);
    let result = if target.is_null() {
        0
    } else {
        // # Safety
        // 卸补丁后直接 CALL 原入口, 栈布局与未 hook 时一致.
        unsafe {
            call_while_unhooked(|| {
                let f: CheckAppOwnershipFn = std::mem::transmute(target);
                f(this, app_id, p_own)
            })
        }
    };

    // 假 license 不在热路径初始化 (Grow/Mark 可能重入).

    if p_own.is_null() || !is_configured(app_id) {
        return result;
    }

    // # Safety
    // p_own 非空; 偏移来自本机 IDA 钉死的 AppOwnership.
    let exist = unsafe { read_u32(p_own, app_ownership::EXIST_IN_PACKAGE_NUMS) };
    match decide_ownership_rewrite(true, result != 0, exist) {
        OwnershipRewrite::LeaveOriginal => result,
        OwnershipRewrite::MarkSteamOwned => {
            unsafe {
                write_u32(
                    p_own,
                    app_ownership::RELEASE_STATE,
                    APP_RELEASE_STATE_RELEASED,
                );
            }
            result
        }
        OwnershipRewrite::ForgeInjected => {
            FORGE_HITS.fetch_add(1, Ordering::Relaxed);
            let forged = ForgedOwnershipFields::injected();
            unsafe {
                write_u32(p_own, app_ownership::PACKAGE_ID, forged.package_id);
                write_u32(p_own, app_ownership::RELEASE_STATE, forged.release_state);
                write_u8(
                    p_own,
                    app_ownership::B_OWNS_LICENSE,
                    u8::from(forged.owns_license),
                );
                write_u8(
                    p_own,
                    app_ownership::B_FREE_LICENSE,
                    u8::from(forged.free_license),
                );
            }
            1
        }
    }
}

fn try_init_fake_license_once() {
    let Some(rt) = RUNTIME.get() else {
        return;
    };
    if rt.queue.is_fake_license_ready() {
        return;
    }
    let pkg = ensure_injected_package();
    if pkg.is_null() {
        return;
    }
    // # Safety
    // pkg 来自 GetPackageInfo 返回的 PackageInfo*.
    let status = unsafe { read_u32(pkg as *mut u8, package_info::STATUS) };
    if status != PACKAGE_STATUS_AVAILABLE {
        return;
    }
    let apps: Vec<AppId> = rt
        .configured
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .copied()
        .collect();
    if apps.is_empty() {
        let _ = rt.queue.mark_fake_license_ready(status);
        return;
    }
    // # Safety
    // Status 已确认 Available; apps 为配置快照.
    if unsafe { append_app_ids(pkg, &apps) } {
        let _ = rt.queue.mark_fake_license_ready(status);
        rt.queue.seed_injected_from_owned(apps.iter().copied());
        mark_license_and_process();
    }
}

fn ensure_injected_package() -> *mut c_void {
    let existing = INJECTED_PKG.load(Ordering::SeqCst);
    if !existing.is_null() {
        return existing;
    }
    let mgr = C_PACKAGE_INFO.load(Ordering::SeqCst);
    let gpi = FN_GET_PACKAGE.load(Ordering::SeqCst);
    if mgr.is_null() || gpi.is_null() {
        return std::ptr::null_mut();
    }
    // # Safety
    // 卸补丁后调原 GetPackageInfo; mgr 为捕获的 CPackageInfo this.
    unsafe {
        let p = call_while_unhooked(|| {
            let f: GetPackageInfoFn = std::mem::transmute(gpi);
            f(mgr, INJECTED_PACKAGE_ID, INJECTED_PACKAGE_ACCESS_TOKEN)
        });
        if !p.is_null() {
            INJECTED_PKG.store(p, Ordering::SeqCst);
        }
        p
    }
}

/// 读 package0 AppIdVec 当前内容 (上限防异常 size).
///
/// # Safety
/// `pkg` 须为有效 PackageInfo*.
unsafe fn read_app_id_vec(pkg: *mut c_void) -> Vec<AppId> {
    const MAX_IDS: usize = 65_536;
    if pkg.is_null() {
        return Vec::new();
    }
    let size = read_u32(pkg as *mut u8, package_info::APP_ID_VEC_SIZE) as usize;
    let mem = read_usize(pkg as *mut u8, package_info::APP_ID_VEC_MEMORY) as *const u32;
    if mem.is_null() || size == 0 {
        return Vec::new();
    }
    let n = size.min(MAX_IDS);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let id = *mem.add(i);
        if id != 0 {
            out.push(id);
        }
    }
    out
}

/// # Safety
/// `pkg` 须为有效 PackageInfo*; Grow 已 resolve; 调用后 AppIdVec 可写.
unsafe fn append_app_ids(pkg: *mut c_void, apps: &[AppId]) -> bool {
    let grow = FN_GROW.load(Ordering::SeqCst);
    if grow.is_null() || pkg.is_null() || apps.is_empty() {
        return false;
    }
    // 跳过已在向量里 / 本次重复的 id, 保序 (重复条目会让 Steam license 处理异常).
    let mut seen: HashSet<AppId> = read_app_id_vec(pkg).into_iter().collect();
    let mut to_add = Vec::with_capacity(apps.len());
    for &id in apps {
        if id == 0 || !seen.insert(id) {
            continue;
        }
        to_add.push(id);
    }
    if to_add.is_empty() {
        return true;
    }
    let vec_ptr = (pkg as *mut u8).add(package_info::APP_ID_VEC_MEMORY) as *mut c_void;
    let old_size = read_u32(pkg as *mut u8, package_info::APP_ID_VEC_SIZE) as usize;
    let f: CUtlMemoryGrowFn = std::mem::transmute(grow);
    let grown = f(vec_ptr, to_add.len() as i32);
    // 写前校验: grow 返回值必须非空, 且重读的 size/capacity 足以覆盖 old + apps.
    // Grow 之后重新读指针与容量 (可能 realloc).
    let new_size = read_u32(pkg as *mut u8, package_info::APP_ID_VEC_SIZE) as usize;
    let capacity = read_u32(
        pkg as *mut u8,
        package_info::APP_ID_VEC_MEMORY + utl_vector::ALLOCATION_COUNT,
    ) as usize;
    let mem = read_usize(pkg as *mut u8, package_info::APP_ID_VEC_MEMORY) as *mut u32;
    let need = old_size + to_add.len();
    if grown.is_null() || mem.is_null() || new_size < need || capacity < need {
        APPEND_FAILURES.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    for (i, &id) in to_add.iter().enumerate() {
        *mem.add(old_size + i) = id;
    }
    // CUtlMemoryGrow 只扩 capacity 并增加 size 字段 (对标上游对 vector 的 grow).
    // 上游 InitFakeLicense 在 grow 后直接写 memory[old+i]; size 由 grow 内部 +=.
    true
}

/// 幂等删除: 找不到也算成功 (Steam 卸载可能已清空 AppIdVec).
///
/// # Safety
/// `pkg` 须为有效 PackageInfo*.
unsafe fn remove_app_id(pkg: *mut c_void, app_id: AppId) -> bool {
    if pkg.is_null() {
        return false;
    }
    let size = read_u32(pkg as *mut u8, package_info::APP_ID_VEC_SIZE) as usize;
    let mem = read_usize(pkg as *mut u8, package_info::APP_ID_VEC_MEMORY) as *mut u32;
    if mem.is_null() || size == 0 {
        // 向量空 = 目标已不在, 对 remove 语义成功.
        return true;
    }
    for i in 0..size {
        if *mem.add(i) == app_id {
            if i + 1 < size {
                *mem.add(i) = *mem.add(size - 1);
            }
            write_u32(
                pkg as *mut u8,
                package_info::APP_ID_VEC_SIZE,
                (size - 1) as u32,
            );
            return true;
        }
    }
    // 未找到: 可能已被 wipe / 先前删过 — 仍成功, 否则 all() 会挡住后续 insert.
    true
}

fn mark_license_and_process() -> bool {
    let user = C_USER.load(Ordering::SeqCst);
    let mark = FN_MARK.load(Ordering::SeqCst);
    let proc = FN_PROCESS.load(Ordering::SeqCst);
    if user.is_null() || mark.is_null() || proc.is_null() {
        return false;
    }
    // # Safety
    // 符号已 resolve; user 来自 CheckAppOwnership 捕获的 CUser*.
    unsafe {
        let m: MarkLicenseFn = std::mem::transmute(mark);
        let p: ProcessPendingFn = std::mem::transmute(proc);
        let _ = m(user, INJECTED_PACKAGE_ID, 1);
        let _ = p(user);
    }
    true
}

/// 配置变更后的 notify: 有 hook 则改 PackageInfo; 否则纯逻辑.
///
/// 每次 client 路径都会先把逻辑 injected **resync 到 AppIdVec 真值**,
/// 再按 configured 做 reconcile: 这样 Steam 原生卸载 wipe 向量后,
/// 仍在配置里的入库 id 会重新入队补回, 而不是整库「消失到刷新清单」。
pub fn notify_license_changed(queue: &LicenseQueue) -> LicenseNotifyPlan {
    if !is_attached() {
        let plan = queue.plan_notify_logic_only();
        apply_ui_actions(&plan);
        return plan;
    }

    // 热路径之外再尝试一次假 license (需要已捕获 CUser / CPackageInfo).
    if !queue.is_fake_license_ready() {
        try_init_fake_license_once();
    }

    let pkg = ensure_injected_package();
    if pkg.is_null() {
        let plan = queue.plan_notify_logic_only();
        apply_ui_actions(&plan);
        return plan;
    }

    // # Safety
    // pkg 非空, 来自 GetPackageInfo.
    let status = unsafe { read_u32(pkg as *mut u8, package_info::STATUS) };
    if status != PACKAGE_STATUS_AVAILABLE {
        // Status 不可用时仍走逻辑队列, 避免入库 UI 协议卡住.
        let plan = queue.plan_notify_logic_only();
        apply_ui_actions(&plan);
        return plan;
    }

    // 1) 内存真值 → 逻辑 injected (发现 Steam 卸载后的 wipe)
    // # Safety
    // Status 已 Available; pkg 有效.
    let present = unsafe { read_app_id_vec(pkg) };
    queue.resync_injected(present.iter().copied());

    // 2) 只补回「配置仍要、但向量里没有」的 id.
    //    不能 full reconcile_owned(present→desired): package0 可能含 Steam 原生条目,
    //    差集 remove 会误删它们. 显式 queue_removal (移除入库) 仍走 pending.
    if let Some(rt) = RUNTIME.get() {
        let desired: Vec<AppId> = rt
            .configured
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .copied()
            .collect();
        for id in desired {
            // queue_addition: 已在 injected(=present) 则跳过; 被 wipe 的会入 pending_add.
            queue.queue_addition(id);
        }
    }

    let mut plan = queue.plan_notify(status, true);
    if plan.should_mark_license_changed {
        // # Safety
        // Status==Available 且 plan 给出要写的 id 列表.
        // remove 幂等 (wipe 后找不到也算成功), 不挡后续 insert 补回.
        let memory_applied = unsafe {
            let removed = plan.remove_ids.iter().all(|id| remove_app_id(pkg, *id));
            let inserted = plan.insert_ids.is_empty() || append_app_ids(pkg, &plan.insert_ids);
            removed && inserted
        };
        if memory_applied {
            plan.client_applied = mark_license_and_process();
        }
    } else if plan.skip_reason == Some("no_changes") {
        plan.client_applied = true;
    }
    apply_ui_actions(&plan);
    plan
}

/// # Safety
/// `base` 非空且 `off` 落在可读对象内.
unsafe fn read_u32(base: *mut u8, off: usize) -> u32 {
    if base.is_null() {
        return 0;
    }
    std::ptr::read_unaligned(base.add(off) as *const u32)
}

/// # Safety
/// `base` 非空且 `off` 落在可写对象内.
unsafe fn write_u32(base: *mut u8, off: usize, v: u32) {
    if base.is_null() {
        return;
    }
    std::ptr::write_unaligned(base.add(off) as *mut u32, v);
}

/// # Safety
/// 同 `write_u32`.
unsafe fn write_u8(base: *mut u8, off: usize, v: u8) {
    if base.is_null() {
        return;
    }
    *base.add(off) = v;
}

/// # Safety
/// 同 `read_u32`.
unsafe fn read_usize(base: *mut u8, off: usize) -> usize {
    if base.is_null() {
        return 0;
    }
    std::ptr::read_unaligned(base.add(off) as *const usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::utl_vector;

    #[test]
    fn env_default_allows_attach() {
        // 不依赖真实环境变量内容做强断言; 仅保证函数可调用.
        let _ = package_hooks_enabled_by_env();
    }

    /// 回归: host seed 直写 HashSet 后必须刷闸门, 否则 checks 涨 forged 一直 0.
    #[test]
    fn direct_configured_write_needs_refresh_gate() {
        let queue = Arc::new(LicenseQueue::new());
        let configured = Arc::new(RwLock::new(HashSet::new()));
        let _ = register_runtime(Arc::clone(&queue), Arc::clone(&configured));
        // 若 RUNTIME 已被其它测试占用, 下面写的是本地 Arc, 闸门仍应能被 refresh 读到 runtime 真值.
        // 能控制的路径: 走 set_configured_apps / add 或本测试独占的第一次 register.
        if let Some(rt) = RUNTIME.get() {
            {
                let mut g = rt
                    .configured
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                g.clear();
            }
            refresh_configured_gate();
            assert!(
                !CONFIGURED_NONEMPTY.load(Ordering::Relaxed),
                "empty set must close forge gate"
            );
            {
                let mut g = rt
                    .configured
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                g.insert(42);
            }
            // 直写后未 refresh: 闸门仍关 (本 bug 的根).
            assert!(
                !CONFIGURED_NONEMPTY.load(Ordering::Relaxed),
                "direct write alone must not open gate"
            );
            refresh_configured_gate();
            assert!(
                CONFIGURED_NONEMPTY.load(Ordering::Relaxed),
                "refresh after seed must open forge gate"
            );
            assert!(is_configured(42));
            assert!(!is_configured(99));
        }
    }

    /// 模拟 CUtlMemoryGrow 失败: 返回空指针, 不扩容量.
    unsafe extern "C" fn mock_grow_null(_vec: *mut c_void, _add: i32) -> *mut c_void {
        std::ptr::null_mut()
    }

    /// 模拟 CUtlMemoryGrow 成功: 扩 ALLOCATION_COUNT 与 SIZE 并返回内存指针.
    unsafe extern "C" fn mock_grow_ok(vec: *mut c_void, add: i32) -> *mut c_void {
        let base = vec as *mut u8;
        let cap = std::ptr::read_unaligned(base.add(utl_vector::ALLOCATION_COUNT) as *const u32);
        let size = std::ptr::read_unaligned(base.add(utl_vector::SIZE) as *const u32);
        std::ptr::write_unaligned(
            base.add(utl_vector::ALLOCATION_COUNT) as *mut u32,
            cap + add as u32,
        );
        std::ptr::write_unaligned(base.add(utl_vector::SIZE) as *mut u32, size + add as u32);
        std::ptr::read(base as *const *mut c_void)
    }

    #[test]
    fn append_app_ids_skips_write_when_grow_fails() {
        let mut buf = [0u8; 0x58];
        let pkg = buf.as_mut_ptr() as *mut c_void;
        FN_GROW.store(
            mock_grow_null as *const c_void as *mut c_void,
            Ordering::SeqCst,
        );
        let before = APPEND_FAILURES.load(Ordering::Relaxed);
        let ok = unsafe { append_app_ids(pkg, &[1001]) };
        assert!(!ok);
        assert!(APPEND_FAILURES.load(Ordering::Relaxed) > before);
        // 失败路径不写任何元素.
        assert_eq!(
            unsafe { read_u32(buf.as_mut_ptr(), package_info::APP_ID_VEC_SIZE) },
            0
        );
        FN_GROW.store(std::ptr::null_mut(), Ordering::SeqCst);
    }

    #[test]
    fn append_app_ids_writes_when_grow_succeeds() {
        let mut store = [0u32; 8];
        let mut buf = [0u8; 0x58];
        unsafe {
            std::ptr::write_unaligned(
                buf.as_mut_ptr().add(package_info::APP_ID_VEC_MEMORY) as *mut *mut u32,
                store.as_mut_ptr(),
            );
            std::ptr::write_unaligned(buf.as_mut_ptr().add(0x48) as *mut u32, store.len() as u32);
        }
        let pkg = buf.as_mut_ptr() as *mut c_void;
        FN_GROW.store(
            mock_grow_ok as *const c_void as *mut c_void,
            Ordering::SeqCst,
        );
        let ok = unsafe { append_app_ids(pkg, &[1001, 1002]) };
        assert!(ok);
        assert_eq!(store[0], 1001);
        assert_eq!(store[1], 1002);
        FN_GROW.store(std::ptr::null_mut(), Ordering::SeqCst);
    }

    #[test]
    fn remove_app_id_is_idempotent_on_empty_or_missing() {
        let mut buf = [0u8; 0x58];
        let pkg = buf.as_mut_ptr() as *mut c_void;
        // 空向量 / 找不到: 都算成功, 不挡后续 insert 补回.
        assert!(unsafe { remove_app_id(pkg, 42) });
        assert!(!unsafe { remove_app_id(std::ptr::null_mut(), 42) });
    }

    #[test]
    fn append_app_ids_skips_already_present() {
        let mut store = [0u32; 8];
        store[0] = 1001;
        let mut buf = [0u8; 0x58];
        unsafe {
            std::ptr::write_unaligned(
                buf.as_mut_ptr().add(package_info::APP_ID_VEC_MEMORY) as *mut *mut u32,
                store.as_mut_ptr(),
            );
            // size=1, capacity=8
            std::ptr::write_unaligned(
                buf.as_mut_ptr().add(package_info::APP_ID_VEC_SIZE) as *mut u32,
                1,
            );
            std::ptr::write_unaligned(buf.as_mut_ptr().add(0x48) as *mut u32, store.len() as u32);
        }
        let pkg = buf.as_mut_ptr() as *mut c_void;
        FN_GROW.store(
            mock_grow_ok as *const c_void as *mut c_void,
            Ordering::SeqCst,
        );
        // 1001 已在, 只应追加 1002.
        assert!(unsafe { append_app_ids(pkg, &[1001, 1002, 1001]) });
        assert_eq!(store[0], 1001);
        assert_eq!(store[1], 1002);
        FN_GROW.store(std::ptr::null_mut(), Ordering::SeqCst);
    }
}
