//! 商店页入库按钮: 同线程门闩注入 (无后台泵).
//!
//! # 策略 (已踩坑后的正确路径)
//!
//! - **禁止**后台线程扫 `CHTMLWindow*` 调 ExecuteJavaScript (UAF / 进商店崩).
//! - 在 **PostURL** (导航, 同线程) 之后, 若 `browser_id != -1` 且 host 非空, **最多 2 次** EJ.
//! - **dtor** 从表删除 this, 避免悬空指针.
//! - ctor 只登记窗口, **不** EJ (browser_id 仍是 -1).
//! - EJ hook 只统计; 注入走 trampoline 原函数, 不重入.
//!
//! # IDA (sha 2a59e23a)
//!
//! - Ctor `0x1e93c0` steal 17; EJ `0x1ef350` steal 12
//! - PostURL `0x1ee060` steal 12; Dtor `0x1eaa70` steal 12
//! - `this+0x14` browser_id; `this+0x58` host*

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::Mutex;

use stt_config::{ToolId, ToolRegistry};
use stt_hook::TrampolineHook;
use stt_metadata::PatternStore;
use stt_platform::module_info;

use crate::click_bridge::{click_bridge_port, store_inject_js_with_bridge};
use crate::store_inject::STORE_INJECT_JS;

/// pattern: ExecuteJavaScript
pub const STORE_INJECT_SYMBOL: &str = "CHTMLWindow_ExecuteJavaScript";
/// pattern: CHTMLWindow 构造
pub const STORE_CTOR_SYMBOL: &str = "CHTMLWindow_Ctor";
/// pattern: PostURL (导航后注入)
pub const STORE_POSTURL_SYMBOL: &str = "CHTMLWindow_PostURL";
/// pattern: 析构 (摘表)
pub const STORE_DTOR_SYMBOL: &str = "CHTMLWindow_Dtor";

const CTOR_STEAL: usize = 17;
const EXEC_STEAL: usize = 12;
const POSTURL_STEAL: usize = 12;
const DTOR_STEAL: usize = 12;

/// `this+0x14`: browser_id, 诊断用 (门闩只看 host 指针).
const OFF_BROWSER_ID: usize = 0x14;
const OFF_HOST: usize = 0x58;
/// 每个窗口最多注入次数 (同线程, 非泵).
const MAX_INJECT: u32 = 2;

/// 运行模式.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreNativeMode {
    /// 不装 hook.
    Disabled,
    /// 只计数, 不 EJ.
    Observe,
    /// 产品路径: PostURL 门闩注入.
    Inject,
}

impl StoreNativeMode {
    /// 商店页在 steamwebhelper (CEF), 不在 steam.exe 的 CHTMLWindow.
    /// 产品主路径是 CDP 8080; native 默认关, 仅调试时 `STEAMTOOLS_STORE_NATIVE=inject`.
    pub fn from_env() -> Self {
        mode_from_env_value(&std::env::var("STEAMTOOLS_STORE_NATIVE").unwrap_or_default())
    }
}

/// 解析 `STEAMTOOLS_STORE_NATIVE`; 空值与未知值一律 Disabled.
fn mode_from_env_value(raw: &str) -> StoreNativeMode {
    match raw.trim().to_ascii_lowercase().as_str() {
        "inject" | "on" | "1" => StoreNativeMode::Inject,
        "observe" | "obs" => StoreNativeMode::Observe,
        _ => StoreNativeMode::Disabled,
    }
}

static ORIG_EXEC: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ORIG_CTOR: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ORIG_POSTURL: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ORIG_DTOR: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static HOOKS: Mutex<Vec<TrampolineHook>> = Mutex::new(Vec::new());
static INJECT_ENABLED: AtomicBool = AtomicBool::new(false);
static WINDOWS: Mutex<Vec<TrackedWindow>> = Mutex::new(Vec::new());
static CTOR_HITS: AtomicU64 = AtomicU64::new(0);
static EXEC_HITS: AtomicU64 = AtomicU64::new(0);
static POSTURL_HITS: AtomicU64 = AtomicU64::new(0);
static INJECT_CALLS: AtomicU64 = AtomicU64::new(0);
static INJECT_SKIP: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
struct TrackedWindow {
    ptr: usize,
    injects: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreNativeStatus {
    Disabled,
    PatternMissing,
    ModuleMissing,
    ResolveFailed,
    Observing,
    Attached,
    AttachFailed,
}

#[derive(Debug, Clone)]
pub struct StoreNativeReport {
    pub status: StoreNativeStatus,
    pub detail: String,
}

impl StoreNativeReport {
    pub fn summary_line(&self) -> String {
        format!("store_native=status={:?} {}", self.status, self.detail)
    }
}

type ExecJsFn = unsafe extern "C" fn(*mut c_void, *const u8) -> usize;
type CtorFn = unsafe extern "C" fn(*mut c_void, *mut c_void, i32, i8) -> *mut c_void;
type PostUrlFn = unsafe extern "C" fn(*mut c_void, *const u8) -> usize;
type DtorFn = unsafe extern "C" fn(*mut c_void, i8) -> *mut c_void;

/// 安装商店 native. **默认关**: 商店页在 steamwebhelper CEF, steam.exe 里没有,
/// 主路径是 CDP 8080. 仅 `STEAMTOOLS_STORE_NATIVE=inject|observe` 时才装.
pub fn try_install_store_native(
    tools: &ToolRegistry,
    patterns: &PatternStore,
) -> StoreNativeReport {
    try_install_store_native_with_mode(tools, patterns, StoreNativeMode::from_env())
}

pub fn try_install_store_native_with_mode(
    tools: &ToolRegistry,
    patterns: &PatternStore,
    mode: StoreNativeMode,
) -> StoreNativeReport {
    if !tools.is_enabled(ToolId::CatalogAdd) {
        return StoreNativeReport {
            status: StoreNativeStatus::Disabled,
            detail: "catalog_add off".into(),
        };
    }
    if mode == StoreNativeMode::Disabled {
        return StoreNativeReport {
            status: StoreNativeStatus::Disabled,
            detail: "mode=disabled (STEAMTOOLS_STORE_NATIVE=off)".into(),
        };
    }

    if patterns.is_failed("steamui") || patterns.map("steamui").is_none() {
        return StoreNativeReport {
            status: StoreNativeStatus::PatternMissing,
            detail: "steamui pattern missing".into(),
        };
    }
    let Some(info) = module_info("steamui.dll").or_else(|| module_info("SteamUI.dll")) else {
        return StoreNativeReport {
            status: StoreNativeStatus::ModuleMissing,
            detail: "steamui.dll not loaded".into(),
        };
    };
    let image = unsafe { stt_platform::read_module_bytes(info) };
    let base = info.base as usize;

    let exec_addr = patterns.find_in_image("steamui", STORE_INJECT_SYMBOL, &image, base);
    let ctor_addr = patterns.find_in_image("steamui", STORE_CTOR_SYMBOL, &image, base);
    let post_addr = patterns.find_in_image("steamui", STORE_POSTURL_SYMBOL, &image, base);
    let dtor_addr = patterns.find_in_image("steamui", STORE_DTOR_SYMBOL, &image, base);

    // 注入模式必须有 EJ + PostURL; Observe 有 ctor/exec 即可.
    if mode == StoreNativeMode::Inject {
        if exec_addr.is_none() {
            return StoreNativeReport {
                status: StoreNativeStatus::ResolveFailed,
                detail: format!("{STORE_INJECT_SYMBOL} required for inject"),
            };
        }
        if post_addr.is_none() {
            return StoreNativeReport {
                status: StoreNativeStatus::ResolveFailed,
                detail: format!("{STORE_POSTURL_SYMBOL} required for inject (no background pump)"),
            };
        }
    } else if exec_addr.is_none() && ctor_addr.is_none() {
        return StoreNativeReport {
            status: StoreNativeStatus::ResolveFailed,
            detail: "no observe symbols".into(),
        };
    }

    let mut hooks = HOOKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !hooks.is_empty() {
        return StoreNativeReport {
            status: if INJECT_ENABLED.load(Ordering::SeqCst) {
                StoreNativeStatus::Attached
            } else {
                StoreNativeStatus::Observing
            },
            detail: format!("already installed windows={}", remembered_window_count()),
        };
    }

    let mut parts = Vec::new();

    // 先装 EJ (注入与统计都需要 trampoline).
    if let Some(addr) = exec_addr {
        if let Err(e) = attach_one(
            &mut hooks,
            &mut parts,
            "exec",
            addr,
            hk_execute_javascript as *const c_void,
            EXEC_STEAL,
            &ORIG_EXEC,
        ) {
            return e;
        }
    }

    if let Some(addr) = ctor_addr {
        if let Err(e) = attach_one(
            &mut hooks,
            &mut parts,
            "ctor",
            addr,
            hk_chtml_ctor as *const c_void,
            CTOR_STEAL,
            &ORIG_CTOR,
        ) {
            rollback(&mut hooks);
            return e;
        }
    }

    if mode == StoreNativeMode::Inject {
        if let Some(addr) = post_addr {
            if let Err(e) = attach_one(
                &mut hooks,
                &mut parts,
                "posturl",
                addr,
                hk_post_url as *const c_void,
                POSTURL_STEAL,
                &ORIG_POSTURL,
            ) {
                rollback(&mut hooks);
                return e;
            }
        }
        if let Some(addr) = dtor_addr {
            if let Err(e) = attach_one(
                &mut hooks,
                &mut parts,
                "dtor",
                addr,
                hk_dtor as *const c_void,
                DTOR_STEAL,
                &ORIG_DTOR,
            ) {
                rollback(&mut hooks);
                return e;
            }
        }
        INJECT_ENABLED.store(true, Ordering::SeqCst);
        StoreNativeReport {
            status: StoreNativeStatus::Attached,
            detail: format!(
                "hooks={} mode=inject (PostURL gate, max {MAX_INJECT}/window, no pump)",
                parts.join(",")
            ),
        }
    } else {
        INJECT_ENABLED.store(false, Ordering::SeqCst);
        StoreNativeReport {
            status: StoreNativeStatus::Observing,
            detail: format!("hooks={} mode=observe", parts.join(",")),
        }
    }
}

fn attach_one(
    hooks: &mut Vec<TrampolineHook>,
    parts: &mut Vec<String>,
    name: &str,
    addr: usize,
    detour: *const c_void,
    steal: usize,
    orig: &AtomicPtr<c_void>,
) -> Result<(), StoreNativeReport> {
    match unsafe { TrampolineHook::new_with_steal(addr as *mut c_void, detour, steal) } {
        Ok(mut h) => {
            orig.store(h.trampoline() as *mut c_void, Ordering::SeqCst);
            if let Err(e) = unsafe { h.attach() } {
                orig.store(std::ptr::null_mut(), Ordering::SeqCst);
                return Err(StoreNativeReport {
                    status: StoreNativeStatus::AttachFailed,
                    detail: format!("{name} attach: {e}"),
                });
            }
            hooks.push(h);
            parts.push(format!("{name}@{addr:#x}"));
            Ok(())
        }
        Err(e) => Err(StoreNativeReport {
            status: StoreNativeStatus::AttachFailed,
            detail: format!("{name} trampoline: {e}"),
        }),
    }
}

fn rollback(hooks: &mut Vec<TrampolineHook>) {
    for h in hooks.iter_mut().rev() {
        let _ = unsafe { h.detach() };
    }
    hooks.clear();
    ORIG_EXEC.store(std::ptr::null_mut(), Ordering::SeqCst);
    ORIG_CTOR.store(std::ptr::null_mut(), Ordering::SeqCst);
    ORIG_POSTURL.store(std::ptr::null_mut(), Ordering::SeqCst);
    ORIG_DTOR.store(std::ptr::null_mut(), Ordering::SeqCst);
    INJECT_ENABLED.store(false, Ordering::SeqCst);
}

/// # Safety
/// `this` 须为有效 CHTMLWindow*; 调用方保证同线程/生命周期.
pub unsafe fn call_original_execute_js(this: *mut c_void, script: *const u8) -> usize {
    let p = ORIG_EXEC.load(Ordering::SeqCst);
    if p.is_null() {
        return 0;
    }
    let f: ExecJsFn = std::mem::transmute(p);
    f(this, script)
}

unsafe fn call_original_ctor(this: *mut c_void, a2: *mut c_void, a3: i32, a4: i8) -> *mut c_void {
    let p = ORIG_CTOR.load(Ordering::SeqCst);
    if p.is_null() {
        return this;
    }
    let f: CtorFn = std::mem::transmute(p);
    f(this, a2, a3, a4)
}

unsafe fn call_original_post_url(this: *mut c_void, url: *const u8) -> usize {
    let p = ORIG_POSTURL.load(Ordering::SeqCst);
    if p.is_null() {
        return 0;
    }
    let f: PostUrlFn = std::mem::transmute(p);
    f(this, url)
}

unsafe fn call_original_dtor(this: *mut c_void, flags: i8) -> *mut c_void {
    let p = ORIG_DTOR.load(Ordering::SeqCst);
    if p.is_null() {
        return this;
    }
    let f: DtorFn = std::mem::transmute(p);
    f(this, flags)
}

fn remember_window(this: *mut c_void) {
    if this.is_null() {
        return;
    }
    let key = this as usize;
    let mut g = WINDOWS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if g.iter().any(|w| w.ptr == key) {
        return;
    }
    g.push(TrackedWindow {
        ptr: key,
        injects: 0,
    });
    if g.len() > 64 {
        let drain = g.len() - 48;
        g.drain(0..drain);
    }
}

fn forget_window(this: *mut c_void) {
    if this.is_null() {
        return;
    }
    let key = this as usize;
    let mut g = WINDOWS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    g.retain(|w| w.ptr != key);
}

/// # Safety
/// `this` 必须指向可读的 CHTMLWindow.
#[expect(dead_code, reason = "诊断用; 门闩改看 host 指针后暂未调用")]
pub unsafe fn browser_id_of(this: *mut c_void) -> i32 {
    if this.is_null() {
        return -1;
    }
    *(this.byte_add(OFF_BROWSER_ID) as *const i32)
}

/// # Safety
/// `this` 必须指向可读的 CHTMLWindow.
unsafe fn host_of(this: *mut c_void) -> *mut c_void {
    if this.is_null() {
        return std::ptr::null_mut();
    }
    *(this.byte_add(OFF_HOST) as *const *mut c_void)
}

/// 门闩: host 非空 + 配额未满. 同线程调用 (无后台泵).
fn try_inject_gated(this: *mut c_void, reason: &str) {
    if !INJECT_ENABLED.load(Ordering::SeqCst) || this.is_null() {
        return;
    }
    if ORIG_EXEC.load(Ordering::SeqCst).is_null() {
        return;
    }
    if IN_INJECT.with(|c| c.get()) {
        return;
    }

    // browser_id==-1 时 EJ 会入队 (this+0x40), 合法; 崩溃主因是跨线程/UAF 不是 -1.
    // 只要求 host 非空 (ctor 已写 this+0x58).
    let host = unsafe { host_of(this) };
    if host.is_null() {
        INJECT_SKIP.fetch_add(1, Ordering::SeqCst);
        return;
    }

    let key = this as usize;
    let mut allowed = false;
    {
        let mut g = WINDOWS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(w) = g.iter_mut().find(|w| w.ptr == key) {
            if w.injects < MAX_INJECT {
                w.injects = w.injects.saturating_add(1);
                allowed = true;
            }
        } else {
            g.push(TrackedWindow {
                ptr: key,
                injects: 1,
            });
            allowed = true;
        }
    }
    if !allowed {
        return;
    }

    let port = click_bridge_port();
    let js = store_inject_js_with_bridge(port, STORE_INJECT_JS);
    let mut bytes = js.into_bytes();
    bytes.push(0);

    IN_INJECT.with(|c| c.set(true));
    unsafe {
        let _ = call_original_execute_js(this, bytes.as_ptr());
    }
    IN_INJECT.with(|c| c.set(false));
    INJECT_CALLS.fetch_add(1, Ordering::SeqCst);
    let _ = reason;
}

thread_local! {
    static IN_INJECT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

unsafe extern "C" fn hk_chtml_ctor(
    this: *mut c_void,
    a2: *mut c_void,
    a3: i32,
    a4: i8,
) -> *mut c_void {
    let ret = call_original_ctor(this, a2, a3, a4);
    CTOR_HITS.fetch_add(1, Ordering::SeqCst);
    if !this.is_null() {
        remember_window(this);
        // 同线程入队一次 (browser_id 常为 -1, EJ 走队列); 无后台泵.
        try_inject_gated(this, "ctor");
    }
    ret
}

unsafe extern "C" fn hk_execute_javascript(this: *mut c_void, script: *const u8) -> usize {
    // 我们自己的注入走 trampoline, 不会进这里.
    EXEC_HITS.fetch_add(1, Ordering::SeqCst);
    let ret = call_original_execute_js(this, script);
    if !this.is_null() {
        remember_window(this);
        // Steam 自己调 EJ 时浏览器通常已活, 同线程补一次门闩注入.
        try_inject_gated(this, "exec");
    }
    ret
}

unsafe extern "C" fn hk_post_url(this: *mut c_void, url: *const u8) -> usize {
    let ret = call_original_post_url(this, url);
    POSTURL_HITS.fetch_add(1, Ordering::SeqCst);
    if !this.is_null() {
        // 导航 = 新页面: 重置该窗配额再注入.
        reset_inject_quota(this);
        remember_window(this);
        try_inject_gated(this, "posturl");
    }
    ret
}

fn reset_inject_quota(this: *mut c_void) {
    if this.is_null() {
        return;
    }
    let key = this as usize;
    let mut g = WINDOWS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(w) = g.iter_mut().find(|w| w.ptr == key) {
        w.injects = 0;
    }
}

unsafe extern "C" fn hk_dtor(this: *mut c_void, flags: i8) -> *mut c_void {
    // 先摘表再调原析构, 避免析构过程中其它路径再注入.
    forget_window(this);
    call_original_dtor(this, flags)
}

pub fn remembered_window_count() -> usize {
    WINDOWS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len()
}

/// 诊断: ctor / exec / inject / windows. inject 含 PostURL 门闩成功次数.
pub fn store_native_stats() -> (u64, u64, u64, usize) {
    (
        CTOR_HITS.load(Ordering::SeqCst),
        EXEC_HITS.load(Ordering::SeqCst),
        INJECT_CALLS.load(Ordering::SeqCst),
        remembered_window_count(),
    )
}

/// 扩展诊断: posturl 命中与门闩跳过.
pub fn store_native_stats_ext() -> (u64, u64, u64, u64, u64, usize) {
    (
        CTOR_HITS.load(Ordering::SeqCst),
        EXEC_HITS.load(Ordering::SeqCst),
        POSTURL_HITS.load(Ordering::SeqCst),
        INJECT_CALLS.load(Ordering::SeqCst),
        INJECT_SKIP.load(Ordering::SeqCst),
        remembered_window_count(),
    )
}

pub fn is_observing() -> bool {
    !INJECT_ENABLED.load(Ordering::SeqCst)
        && !HOOKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
}

pub fn builtin_steamui_pattern(sha256_hex: &str) -> Option<&'static str> {
    const SHA: &str = "2a59e23adb9b2926515e79affd8a0940522187fa962127d0eddbb823a1c96753";
    if sha256_hex.eq_ignore_ascii_case(SHA) {
        Some(include_str!(
            "../../../docs/plan/scratch/pattern-steamui-2a59e23a.toml"
        ))
    } else {
        None
    }
}

pub fn ensure_builtin_steamui_pattern(
    steam_root: &std::path::Path,
    sha256_hex: &str,
) -> Option<std::path::PathBuf> {
    let body = builtin_steamui_pattern(sha256_hex)?;
    let path = stt_platform::pattern_cache_file(steam_root, "steamui", sha256_hex);
    let parent = path.parent()?;
    std::fs::create_dir_all(parent).ok()?;
    std::fs::write(&path, body).ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stt_config::ToolRegistry;
    use stt_metadata::PatternStore;

    #[test]
    fn disabled_without_tool() {
        let mut tools = ToolRegistry::with_defaults();
        tools.set_enabled(ToolId::CatalogAdd, false);
        let store = PatternStore::new();
        let r = try_install_store_native_with_mode(&tools, &store, StoreNativeMode::Inject);
        assert_eq!(r.status, StoreNativeStatus::Disabled);
    }

    #[test]
    fn disabled_mode() {
        let tools = ToolRegistry::with_defaults();
        let store = PatternStore::new();
        let r = try_install_store_native_with_mode(&tools, &store, StoreNativeMode::Disabled);
        assert_eq!(r.status, StoreNativeStatus::Disabled);
    }

    #[test]
    fn pattern_missing_in_inject() {
        let tools = ToolRegistry::with_defaults();
        let store = PatternStore::new();
        let r = try_install_store_native_with_mode(&tools, &store, StoreNativeMode::Inject);
        assert_eq!(r.status, StoreNativeStatus::PatternMissing);
    }

    #[test]
    fn steal_constants_match_ida() {
        assert_eq!(CTOR_STEAL, 17);
        assert_eq!(EXEC_STEAL, 12);
        assert_eq!(POSTURL_STEAL, 12);
        assert_eq!(DTOR_STEAL, 12);
    }

    #[test]
    fn unknown_env_value_disables_native() {
        // 主路径是 CDP; 环境变量拼错时必须落回 Disabled 而不是偷偷注入.
        assert_eq!(mode_from_env_value("nonsense"), StoreNativeMode::Disabled);
    }

    #[test]
    fn inject_env_value_enables_inject() {
        assert_eq!(mode_from_env_value("inject"), StoreNativeMode::Inject);
    }

    #[test]
    fn inject_quota_per_window_is_two() {
        assert_eq!(MAX_INJECT, 2);
    }
}
