//! hook steam.exe / steamclient64.dll 的进程创建 API, 只改 steamwebhelper 的调试参数.
//!
//! 走 IAT 而非 prologue: steam.exe 从导入表调 `CreateProcessW` (已实证), 改指针
//! 不用猜指令边界. 详见 ADR 0010.
//!
//! 2026-08-01 起同时接管 `CreateProcessAsUserW`: steamclient64.dll 经它拉起
//! webhelper, 只挂 W 时整个会话一次都截不到 (host.log `missed_webhelper
//! calls=1 webhelper=0`), 8080 无人监听, 入口/面板全部消失.
//!
//! # 风险
//!
//! 这个 detour 挂在**所有子进程创建**的必经之路上 (包括启动游戏). 因此:
//! 非目标一律原样透传; 不 panic (跨 FFI 展开是 UB); 不等锁; 改写失败就放行原参数.

use std::collections::BTreeSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use stt_hook::IatHook;

use stt_platform::{
    prepare_devtools_pipe, ChildPipeLaunch, DevToolsPipe, EXTENDED_STARTUPINFO_PRESENT,
    STARTUPINFOW_SIZE,
};

use crate::store_debug::{
    alloc_cef_debug_port, is_webhelper_launch, mark_session_port_live, rewrite_webhelper_cmdline,
    DebugChannel,
};

/// `CreateProcessW` 原型 (只写我们要用的部分, 其余按指针透传).
type CreateProcessWFn = unsafe extern "system" fn(
    *const u16, // lpApplicationName
    *mut u16,   // lpCommandLine (可写!)
    *const c_void,
    *const c_void,
    i32,
    u32,
    *const c_void,
    *const u16,
    *const c_void,
    *mut c_void,
) -> i32;

/// `CreateProcessAsUserW` 原型: 比 W 多一个 `hToken` 在最前面.
type CreateProcessAsUserWFn = unsafe extern "system" fn(
    *const c_void, // hToken
    *const u16,
    *mut u16,
    *const c_void,
    *const c_void,
    i32,
    u32,
    *const c_void,
    *const u16,
    *const c_void,
    *mut c_void,
) -> i32;

/// hook 状态; 模块陆续加载, 所以要能反复补挂.
struct HookState {
    /// 已挂上的 (模块名, hook).
    hooks: Vec<(String, IatHook)>,
    /// 已查过导入表的模块 (基址, 名字) — 不导入 CreateProcessW 的也记着,
    /// 否则每轮补挂都要把上百个模块的导入表重扫一遍.
    ///
    /// 代价: 模块卸载后又在同一基址重新加载会被当成已查过而漏挂. Steam 的
    /// 这几个 DLL 进程内不卸载, 用这点风险换掉每 2s 一次的全量重扫.
    examined: BTreeSet<(usize, String)>,
}

static STATE: Mutex<HookState> = Mutex::new(HookState {
    hooks: Vec::new(),
    examined: BTreeSet::new(),
});
static ORIGINAL: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
/// `CreateProcessAsUserW` 的原函数指针 (steamclient64 拉起 webhelper 走这条路).
static ORIGINAL_AS_USER_W: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
/// 关掉时只剥参数不注入, webhelper 就完全不开调试端点.
static INJECT_ENABLED: AtomicBool = AtomicBool::new(false);
/// detour 被调用的总次数 (任何子进程) — 用来分辨"没挂上"与"挂上了但没等到 webhelper".
static CALLS: AtomicU64 = AtomicU64::new(0);
/// 认出是 webhelper 主进程的次数.
static WEBHELPER_SEEN: AtomicU64 = AtomicU64::new(0);
/// 真的改写了命令行的次数.
static REWRITES: AtomicU64 = AtomicU64::new(0);
/// 首选 pipe 通道 (不开端口); 由 host 按回退状态决定.
static USE_PIPE: AtomicBool = AtomicBool::new(false);
/// 已成功把管道交给某次 webhelper.
static PIPE_ARMED: AtomicBool = AtomicBool::new(false);
/// detour 里建好、等 CDP 桥来取的管道.
static PENDING_PIPE: Mutex<Option<DevToolsPipe>> = Mutex::new(None);

/// 取走 detour 建好的 CDP 管道 (只有第一个调用者拿得到).
pub fn take_devtools_pipe() -> Option<DevToolsPipe> {
    PENDING_PIPE.lock().ok().and_then(|mut g| g.take())
}

/// 管道是否已经交给了某个 webhelper.
pub fn pipe_armed() -> bool {
    PIPE_ARMED.load(Ordering::SeqCst)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CefDebugStatus {
    /// 工具关闭, 没装 hook.
    Disabled,
    /// 没有任何已加载模块从导入表调 CreateProcessW.
    ImportMissing,
    /// 装载失败.
    AttachFailed,
    /// 已接管, 后续 webhelper 会拿到本会话端口.
    Attached,
}

#[derive(Debug, Clone)]
pub struct CefDebugReport {
    pub status: CefDebugStatus,
    pub port: u16,
    /// 已挂上 CreateProcessW IAT 的模块名.
    pub modules: Vec<String>,
    pub detail: String,
}

impl CefDebugReport {
    pub fn summary_line(&self) -> String {
        let (calls, seen, rewrites) = cef_debug_stats();
        format!(
            "cef_debug=status={:?} port={} calls={calls} webhelper={seen} rewrites={rewrites} {}",
            self.status, self.port, self.detail
        )
    }

    /// 唯一可信的成功判据: 真的截到过一次 webhelper 启动.
    ///
    /// 曾经用"挂上了 steamclient64"当判据, 结果是假阳性 — 拼参数的确实是
    /// steamclient, 但发起 `CreateProcessW` 的调用落在 `tier0_s64.dll`,
    /// 于是 hook 报 Attached、端口却没人监听.
    pub fn caught_webhelper(&self) -> bool {
        WEBHELPER_SEEN.load(Ordering::SeqCst) > 0
    }
}

/// 装上 hook 并分配本会话端口.
///
/// `enable`: 通常是 `catalog_add` 开关. 关掉时**仍装 hook**, 但只剥不注入 —
/// 这样能顺手把 Steam 自己的 8080 与 `--remote-allow-origins=*` 一起摘掉.
///
/// 可重复调用: steamclient64 可能比 host 晚加载, 后续再调会补挂新出现的模块.
pub fn install_cef_debug_hook(enable: bool, use_pipe: bool) -> CefDebugReport {
    let mut guard = STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    INJECT_ENABLED.store(enable, Ordering::SeqCst);
    USE_PIPE.store(use_pipe, Ordering::SeqCst);

    // pipe 模式不占端口, 自然也不用预留.
    let port = if use_pipe {
        0
    } else {
        match crate::store_debug::cef_debug_port() {
            0 if enable => alloc_cef_debug_port(),
            p => p,
        }
    };
    if enable && !use_pipe && port == 0 {
        return CefDebugReport {
            status: CefDebugStatus::AttachFailed,
            port: 0,
            modules: Vec::new(),
            detail: "could not reserve a debug port".into(),
        };
    }

    let Some(target) = resolve_create_process_w() else {
        return CefDebugReport {
            status: CefDebugStatus::ImportMissing,
            port,
            modules: Vec::new(),
            detail: "CreateProcessW not resolvable".into(),
        };
    };
    let Some(target_as_user) = resolve_create_process_as_user_w() else {
        return CefDebugReport {
            status: CefDebugStatus::ImportMissing,
            port,
            modules: Vec::new(),
            detail: "CreateProcessAsUserW not resolvable".into(),
        };
    };
    // 先存原函数指针: detour 一旦生效就会立刻用到它.
    ORIGINAL.store(target.cast_mut(), Ordering::SeqCst);
    ORIGINAL_AS_USER_W.store(target_as_user.cast_mut(), Ordering::SeqCst);

    for (name, base) in hookable_modules() {
        // 同一模块只查一次导入表: 补挂每 2s 跑一轮, 重扫上百个模块太浪费.
        if !guard.examined.insert((base as usize, name.clone())) {
            continue;
        }
        // 模块不导入这两个函数是正常的, 跳过即可.
        if let Ok(mut hook) =
            unsafe { IatHook::new(base, target, hk_create_process_w as *const c_void) }
        {
            if unsafe { hook.attach() }.is_ok() {
                guard.hooks.push((name.clone(), hook));
            }
        }
        // steamclient64 走 AsUserW 拉起 webhelper, 两者都要接管 (见模块注释).
        if let Ok(mut hook) = unsafe {
            IatHook::new(
                base,
                target_as_user,
                hk_create_process_as_user_w as *const c_void,
            )
        } {
            if unsafe { hook.attach() }.is_ok() {
                guard.hooks.push((name, hook));
            }
        }
    }

    if guard.hooks.is_empty() {
        ORIGINAL.store(std::ptr::null_mut(), Ordering::SeqCst);
        ORIGINAL_AS_USER_W.store(std::ptr::null_mut(), Ordering::SeqCst);
        return CefDebugReport {
            status: CefDebugStatus::ImportMissing,
            port,
            modules: Vec::new(),
            detail: "no loaded module imports CreateProcessW/AsUserW yet".into(),
        };
    }

    let modules: Vec<String> = guard.hooks.iter().map(|(n, _)| n.clone()).collect();
    CefDebugReport {
        status: CefDebugStatus::Attached,
        port,
        modules: modules.clone(),
        detail: format!(
            "modules={} channel={}",
            modules.join(","),
            match (enable, use_pipe) {
                (false, _) => "off (strip only)",
                (true, true) => "pipe (no port)",
                (true, false) => "port (fallback)",
            }
        ),
    }
}

/// 忙等模块加载并补挂, 直到真的截到 webhelper 启动 (或超时).
///
/// webhelper 通常在 steam 启动后几百毫秒内被拉起; 把补挂丢到 watch 循环会错过
/// 首个主进程. 判据必须是"截到了", 不能是"挂上了某个模块" — 后者会因为发起调用
/// 的模块没被挂到而假阳性.
pub fn wait_cef_debug_hook(enable: bool, use_pipe: bool, timeout: Duration) -> CefDebugReport {
    let start = Instant::now();
    let mut last = install_cef_debug_hook(enable, use_pipe);
    // 20ms 一跳: 够抢在 ~300ms 的 webhelper 启动前, 又不至于反复枚举上百个模块.
    const STEP: Duration = Duration::from_millis(20);
    while !last.caught_webhelper() && start.elapsed() < timeout {
        std::thread::sleep(STEP);
        last = install_cef_debug_hook(enable, use_pipe);
    }
    last
}

/// 当前已加载的全部模块.
///
/// 不写死名单: 拼调试参数的是 steamclient64.dll, 但实测发起 `CreateProcessW`
/// 的调用可以落在 `tier0_s64.dll` 这样的中间层里. 逐个试, 不导入的自然挂不上.
fn hookable_modules() -> Vec<(String, *const c_void)> {
    let mut out: Vec<(String, *const c_void)> = stt_platform::enumerate_modules()
        .into_iter()
        .map(|(name, info)| (name, info.base.cast_const()))
        .collect();
    // 枚举失败时至少还有主 exe 兜底.
    if out.is_empty() {
        if let Some(base) = stt_platform::main_module_base() {
            out.push(("(main)".into(), base));
        }
    }
    out
}

/// 首次截到 webhelper 时的进程创建参数快照.
///
/// 为 `--remote-debugging-pipe` 探路: Chromium 在 Windows 上从继承的 **fd 3/4**
/// 收发 CDP (已用 Edge 150 实测), 而 fd≥3 靠 MSVCRT 的 `lpReserved2` 块传递.
/// 能不能干净地塞进去, 取决于 Steam 这边给的 `bInheritHandles` / `STARTUPINFO`.
static LAUNCH_SNAPSHOT: Mutex<Option<String>> = Mutex::new(None);

/// 取走快照 (只有第一次拿得到内容).
pub fn take_launch_snapshot() -> Option<String> {
    LAUNCH_SNAPSHOT.try_lock().ok().and_then(|mut g| g.take())
}

// x64 `STARTUPINFOW` 字段偏移. 这是公开且冻结的 Win32 ABI, 不是猜出来的偏移;
// 与 iat.rs 里按偏移读 PE 头同理.
const SI_CB: usize = 0;
const SI_DW_FLAGS: usize = 60;
const SI_CB_RESERVED2: usize = 66;
const SI_LP_RESERVED2: usize = 72;
const SI_H_STD_INPUT: usize = 80;
const SI_H_STD_OUTPUT: usize = 88;
const SI_H_STD_ERROR: usize = 96;

/// 记下 Steam 传进来的创建参数; 只记第一次, 失败就静默放弃.
///
/// # Safety
/// `si` 须为 `CreateProcessW` 传入的合法 `STARTUPINFOW` 指针.
unsafe fn snapshot_launch(si: *const c_void, inherit: i32, flags: u32) {
    if si.is_null() {
        return;
    }
    let Ok(mut guard) = LAUNCH_SNAPSHOT.try_lock() else {
        return; // 不等锁: 这是子进程创建的必经之路.
    };
    if guard.is_some() {
        return;
    }
    let b = si.cast::<u8>();
    // SAFETY: 下面几处 read_unaligned 都在 cb 校验通过后, 落在同一结构体内;
    // 用 unaligned 是因为来源指针的对齐不受我们控制.
    let cb = std::ptr::read_unaligned(b.add(SI_CB).cast::<u32>());
    // cb 不像 STARTUPINFOW 就别往下读了.
    if cb as usize != STARTUPINFOW_SIZE {
        *guard = Some(format!("startupinfo cb={cb} (unexpected, not read)"));
        return;
    }
    let dw_flags = std::ptr::read_unaligned(b.add(SI_DW_FLAGS).cast::<u32>());
    let cb_reserved2 = std::ptr::read_unaligned(b.add(SI_CB_RESERVED2).cast::<u16>());
    let lp_reserved2 = std::ptr::read_unaligned(b.add(SI_LP_RESERVED2).cast::<*const u8>());
    let h_in = std::ptr::read_unaligned(b.add(SI_H_STD_INPUT).cast::<usize>());
    let h_out = std::ptr::read_unaligned(b.add(SI_H_STD_OUTPUT).cast::<usize>());
    let h_err = std::ptr::read_unaligned(b.add(SI_H_STD_ERROR).cast::<usize>());
    *guard = Some(format!(
        "inherit={inherit} extended={} dwFlags=0x{dw_flags:08x} \
         cbReserved2={cb_reserved2} lpReserved2={} \
         hStdIn=0x{h_in:x} hStdOut=0x{h_out:x} hStdErr=0x{h_err:x}",
        flags & EXTENDED_STARTUPINFO_PRESENT != 0,
        if lp_reserved2.is_null() {
            "null"
        } else {
            "set"
        },
    ));
}

/// 诊断计数: (detour 总调用, 认出的 webhelper 启动, 实际改写).
///
/// `calls=0` 说明 hook 没挂到发起调用的模块; `calls>0 && webhelper=0` 说明
/// webhelper 走的是别的 API (steamclient64 经 `CreateProcessAsUserW` 拉起,
/// 2026-08-01 起两路都挂, 若再现则优先怀疑模块 IAT 重定位).
pub fn cef_debug_stats() -> (u64, u64, u64) {
    (
        CALLS.load(Ordering::SeqCst),
        WEBHELPER_SEEN.load(Ordering::SeqCst),
        REWRITES.load(Ordering::SeqCst),
    )
}

/// 已改写过的 webhelper 启动次数 (诊断).
pub fn cef_debug_rewrites() -> u64 {
    REWRITES.load(Ordering::SeqCst)
}

fn resolve_create_process_w() -> Option<*const c_void> {
    stt_platform::proc_address("kernel32.dll", c"CreateProcessW")
}

fn resolve_create_process_as_user_w() -> Option<*const c_void> {
    stt_platform::proc_address("kernel32.dll", c"CreateProcessAsUserW")
}

/// 直接调原函数; 拿不到就返回失败 (不可能发生, 但绝不 panic).
#[expect(clippy::too_many_arguments, reason = "Win32 CreateProcessW 原型")]
unsafe fn call_original(
    app: *const u16,
    cmd: *mut u16,
    pa: *const c_void,
    ta: *const c_void,
    inherit: i32,
    flags: u32,
    env: *const c_void,
    dir: *const u16,
    si: *const c_void,
    pi: *mut c_void,
) -> i32 {
    let raw = ORIGINAL.load(Ordering::SeqCst);
    if raw.is_null() {
        return 0;
    }
    // SAFETY: raw 来自 kernel32 导出表, 原型与 Win32 CreateProcessW 一致.
    let f: CreateProcessWFn = std::mem::transmute(raw);
    f(app, cmd, pa, ta, inherit, flags, env, dir, si, pi)
}

/// 直接调原 `CreateProcessAsUserW`; 拿不到就返回失败 (绝不 panic).
#[expect(clippy::too_many_arguments, reason = "Win32 CreateProcessAsUserW 原型")]
unsafe fn call_original_as_user_w(
    h_token: *const c_void,
    app: *const u16,
    cmd: *mut u16,
    pa: *const c_void,
    ta: *const c_void,
    inherit: i32,
    flags: u32,
    env: *const c_void,
    dir: *const u16,
    si: *const c_void,
    pi: *mut c_void,
) -> i32 {
    let raw = ORIGINAL_AS_USER_W.load(Ordering::SeqCst);
    if raw.is_null() {
        return 0;
    }
    // SAFETY: raw 来自 kernel32 导出表, 原型与 Win32 CreateProcessAsUserW 一致.
    let f: CreateProcessAsUserWFn = std::mem::transmute(raw);
    f(h_token, app, cmd, pa, ta, inherit, flags, env, dir, si, pi)
}

/// 两个 detour 共用的准备阶段: 识别 webhelper、改写命令行、备管道.
///
/// 改写缓冲由 `_buffer` 持有, 必须活到原函数调用返回; 调用方负责调原函数
/// 并走 `finish_launch` 收尾.
struct PreparedLaunch {
    _buffer: Option<Vec<u16>>,
    cmd: *mut u16,
    si: *const c_void,
    flags: u32,
    inherit: i32,
    launch: Option<ChildPipeLaunch>,
}

unsafe fn prepare_launch(
    app: *const u16,
    cmd: *mut u16,
    inherit: i32,
    flags: u32,
    si: *const c_void,
) -> PreparedLaunch {
    // 改写用的缓冲要活到调用结束; None 表示照原样透传.
    let seen_before = WEBHELPER_SEEN.load(Ordering::SeqCst);
    let mut patched = rewritten_cmdline(app, cmd);
    let is_webhelper = WEBHELPER_SEEN.load(Ordering::SeqCst) > seen_before;
    // 认出 webhelper 才留快照 — 免得为每个游戏进程都解析一遍 STARTUPINFO.
    if is_webhelper {
        snapshot_launch(si, inherit, flags);
    }
    let cmd_ptr = match patched {
        Some(ref mut buf) => buf.as_mut_ptr(),
        None => cmd,
    };
    if patched.is_some() {
        REWRITES.fetch_add(1, Ordering::SeqCst);
    }

    // pipe 模式: 把 CDP 管道作为 fd 3/4 交下去, 并换掉 STARTUPINFO.
    // launch 里的缓冲被 STARTUPINFOEX 按指针引用, 必须活到原函数返回.
    let mut launch: Option<ChildPipeLaunch> = None;
    let (si_arg, flags_arg, inherit_arg) = if is_webhelper && patched.is_some() && use_pipe() {
        match arm_devtools_pipe(si) {
            Some((ptr, l)) => {
                launch = Some(l);
                // 继承范围已由句柄白名单锁死在那两个管道端.
                (ptr, flags | EXTENDED_STARTUPINFO_PRESENT, 1)
            }
            // 备不出管道就按原样放行: webhelper 照常起, 只是没有调试通道.
            None => (si, flags, inherit),
        }
    } else {
        (si, flags, inherit)
    };
    PreparedLaunch {
        _buffer: patched,
        cmd: cmd_ptr,
        si: si_arg,
        flags: flags_arg,
        inherit: inherit_arg,
        launch,
    }
}

/// 原函数返回后的收尾: 进程没起来就扔掉管道, 否则标记管道已交付.
unsafe fn finish_launch(rc: i32, launch: Option<ChildPipeLaunch>) {
    if launch.is_some() {
        if rc == 0 {
            // 进程没起来: 扔掉管道, 免得 CDP 桥去等一个永远不来的对端.
            drop(take_devtools_pipe());
        } else {
            PIPE_ARMED.store(true, Ordering::SeqCst);
        }
    }
}

// 参数个数由 Win32 CreateProcessW 决定, 不能精简.
unsafe extern "system" fn hk_create_process_w(
    app: *const u16,
    cmd: *mut u16,
    pa: *const c_void,
    ta: *const c_void,
    inherit: i32,
    flags: u32,
    env: *const c_void,
    dir: *const u16,
    si: *const c_void,
    pi: *mut c_void,
) -> i32 {
    CALLS.fetch_add(1, Ordering::SeqCst);
    let prepared = prepare_launch(app, cmd, inherit, flags, si);
    let rc = call_original(
        app,
        prepared.cmd,
        pa,
        ta,
        prepared.inherit,
        prepared.flags,
        env,
        dir,
        prepared.si,
        pi,
    );
    finish_launch(rc, prepared.launch);
    rc
}

// steamclient64 经 CreateProcessAsUserW 拉起 webhelper (2026-08-01 实机证据).
unsafe extern "system" fn hk_create_process_as_user_w(
    h_token: *const c_void,
    app: *const u16,
    cmd: *mut u16,
    pa: *const c_void,
    ta: *const c_void,
    inherit: i32,
    flags: u32,
    env: *const c_void,
    dir: *const u16,
    si: *const c_void,
    pi: *mut c_void,
) -> i32 {
    CALLS.fetch_add(1, Ordering::SeqCst);
    let prepared = prepare_launch(app, cmd, inherit, flags, si);
    let rc = call_original_as_user_w(
        h_token,
        app,
        prepared.cmd,
        pa,
        ta,
        prepared.inherit,
        prepared.flags,
        env,
        dir,
        prepared.si,
        pi,
    );
    finish_launch(rc, prepared.launch);
    rc
}

fn use_pipe() -> bool {
    USE_PIPE.load(Ordering::SeqCst)
}

/// 建好 CDP 管道并存下给桥取, 返回要用的 `lpStartupInfo`.
///
/// 先占锁再建管道: 反过来的话, 管道建好却存不进去时我们这侧的两端会被 drop
/// 关掉, 子进程只会拿到一个立刻 EOF 的 fd — 那比不注入更糟.
///
/// # Safety
/// `si` 须为 `CreateProcessW` 传入的 `lpStartupInfo`.
unsafe fn arm_devtools_pipe(si: *const c_void) -> Option<(*const c_void, ChildPipeLaunch)> {
    // 不等锁 — 这是所有子进程创建的必经之路.
    let mut slot = PENDING_PIPE.try_lock().ok()?;
    let (pipe, launch) = prepare_devtools_pipe(si)?;
    let ptr = launch.startup_info();
    *slot = Some(pipe);
    Some((ptr, launch))
}

/// 判断并生成改写后的命令行 (以 NUL 结尾的 UTF-16 缓冲).
///
/// 任何一步不确定都返回 `None` = 原样放行. 这个函数不 panic.
unsafe fn rewritten_cmdline(app: *const u16, cmd: *mut u16) -> Option<Vec<u16>> {
    let app_str = wide_to_string(app);
    let cmd_str = wide_to_string(cmd);
    if !is_webhelper_launch(app_str.as_deref(), cmd_str.as_deref()) {
        return None;
    }
    WEBHELPER_SEEN.fetch_add(1, Ordering::SeqCst);
    // lpCommandLine 为空时无从改写 (Steam 实机总是给的).
    let cmd_str = cmd_str?;
    let channel = if !INJECT_ENABLED.load(Ordering::SeqCst) {
        DebugChannel::None
    } else if use_pipe() {
        DebugChannel::Pipe
    } else {
        DebugChannel::Port(crate::store_debug::cef_debug_port())
    };
    let rewritten = rewrite_webhelper_cmdline(&cmd_str, channel);
    if rewritten == cmd_str {
        return None;
    }
    if let DebugChannel::Port(p) = channel {
        if p != 0 {
            // 端口确实进了命令行, CDP 这才该去连它而不是 8080.
            mark_session_port_live();
        }
    }
    Some(rewritten.encode_utf16().chain(std::iter::once(0)).collect())
}

/// 读一个以 NUL 结尾的宽字符串; 空指针或超长都返回 None.
unsafe fn wide_to_string(p: *const u16) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // 命令行上限 32767; 留一倍余量后仍不见 NUL 就当它不可信.
    const MAX: usize = 64 * 1024;
    let mut len = 0usize;
    while len < MAX && *p.add(len) != 0 {
        len += 1;
    }
    if len >= MAX {
        return None;
    }
    Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_line_mentions_port() {
        let r = CefDebugReport {
            status: CefDebugStatus::Attached,
            port: 51234,
            modules: vec!["steam.exe".into()],
            detail: "x".into(),
        };
        assert!(r.summary_line().contains("port=51234"));
    }

    /// 挂上模块不等于截到启动: 判据必须来自 detour 实际看到的那次调用.
    #[test]
    fn attached_modules_alone_do_not_mean_caught() {
        let report = CefDebugReport {
            status: CefDebugStatus::Attached,
            port: 1,
            modules: vec!["steam.exe".into(), "steamclient64.dll".into()],
            detail: String::new(),
        };
        // 计数器是进程级的, 只断言它跟着 detour 走, 不跟着 modules 走.
        let before = WEBHELPER_SEEN.load(Ordering::SeqCst);
        assert_eq!(report.caught_webhelper(), before > 0);
    }

    #[test]
    fn seeing_a_webhelper_launch_marks_caught() {
        let mut cmd: Vec<u16> =
            r#""steamwebhelper.exe" -lang=zh"#.encode_utf16().chain(std::iter::once(0)).collect();
        let _ = unsafe { rewritten_cmdline(std::ptr::null(), cmd.as_mut_ptr()) };
        let report = CefDebugReport {
            status: CefDebugStatus::Attached,
            port: 1,
            modules: Vec::new(),
            detail: String::new(),
        };
        assert!(report.caught_webhelper());
    }

    #[test]
    fn null_wide_string_is_none() {
        assert!(unsafe { wide_to_string(std::ptr::null()) }.is_none());
    }

    /// steamclient64 靠 AsUserW 拉起 webhelper; 拿不到导出就谈不上接管.
    #[test]
    fn as_user_w_resolves_on_kernel32() {
        assert!(resolve_create_process_as_user_w().is_some());
    }

    #[test]
    fn reads_nul_terminated_wide_string() {
        let buf: Vec<u16> = "steam".encode_utf16().chain(std::iter::once(0)).collect();
        assert_eq!(
            unsafe { wide_to_string(buf.as_ptr()) }.as_deref(),
            Some("steam")
        );
    }

    #[test]
    fn non_webhelper_launch_is_passed_through() {
        let mut cmd: Vec<u16> =
            r#""steam.exe" -silent"#.encode_utf16().chain(std::iter::once(0)).collect();
        assert!(unsafe { rewritten_cmdline(std::ptr::null(), cmd.as_mut_ptr()) }.is_none());
    }

    #[test]
    fn webhelper_launch_gets_rewritten() {
        let mut cmd: Vec<u16> = r#""steamwebhelper.exe" --remote-debugging-port=8080"#
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let out = unsafe { rewritten_cmdline(std::ptr::null(), cmd.as_mut_ptr()) };
        assert!(out.is_some(), "webhelper 应被改写");
    }

    #[test]
    fn rewritten_buffer_is_nul_terminated() {
        let mut cmd: Vec<u16> = r#""steamwebhelper.exe" --remote-allow-origins=*"#
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let out = unsafe { rewritten_cmdline(std::ptr::null(), cmd.as_mut_ptr()) }.unwrap();
        assert_eq!(out.last(), Some(&0));
    }
}
