//! 无害进程内 hook: 改本地函数 / 手写机器码, 计数, 再卸掉.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use windows::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE,
};

use crate::detour::{InlineHook, TrampolineHook};
use crate::error::{HookError, Result};
use crate::iat::IatHook;

static CALLS: AtomicU32 = AtomicU32::new(0);
static SELF_TEST_LOCK: Mutex<()> = Mutex::new(());

// 填充保证函数体 >= 12 字节, 避免 release 下补丁写穿邻接代码.
#[inline(never)]
#[no_mangle]
pub extern "C" fn stt_hook_probe_target(x: u32) -> u32 {
    let mut y = x;
    y = y.wrapping_add(1);
    std::hint::black_box(y);
    y = y.wrapping_add(0);
    std::hint::black_box(y);
    y = y.wrapping_add(0);
    std::hint::black_box(y);
    y
}

#[inline(never)]
#[no_mangle]
pub extern "C" fn stt_hook_probe_pad(x: u32) -> u32 {
    x.wrapping_add(7)
}

unsafe extern "C" fn stt_hook_probe_detour(x: u32) -> u32 {
    CALLS.fetch_add(1, Ordering::SeqCst);
    // 无跳回原函数的 trampoline; 自测只证明能补丁与恢复.
    x.wrapping_add(100)
}

/// 安装 -> 调用 -> 卸载本地 probe; 返回 detour 见到的调用次数.
pub fn run_harmless_self_test() -> Result<u32> {
    let _guard = SELF_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    CALLS.store(0, Ordering::SeqCst);
    let _ = stt_hook_probe_pad(0);
    let target = stt_hook_probe_target as *mut core::ffi::c_void;
    let detour = stt_hook_probe_detour as *const core::ffi::c_void;

    unsafe {
        let mut hook = InlineHook::new(target, detour)?;
        hook.attach()?;

        let hooked = stt_hook_probe_target(1);
        if hooked != 101 {
            let _ = hook.detach();
            return Err(HookError::SelfTestFailed(format!(
                "expected detour result 101, got {hooked}"
            )));
        }

        hook.detach()?;

        let after = stt_hook_probe_target(1);
        if after != 2 {
            return Err(HookError::SelfTestFailed(format!(
                "expected restored result 2, got {after}"
            )));
        }
    }

    let n = CALLS.load(Ordering::SeqCst);
    if n == 0 {
        return Err(HookError::SelfTestFailed("detour did not run".into()));
    }
    Ok(n)
}

/// 手写 x64 序言 (与 CHTMLWindow ctor 同布局 17 字节) + 返回 x+1.
/// 验证 TrampolineHook steal=17 能正确回跳.
pub fn run_trampoline_self_test() -> Result<u32> {
    let _guard = SELF_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // 序言 17 字节 (与 steamui ctor 对齐):
    // mov [rsp+8], rbx; mov [rsp+10h], rsi; push rdi; sub rsp, 20h; xor edi, edi
    // 然后: mov eax, ecx; inc eax; add rsp,20h; pop rdi; restore rbx/rsi; ret
    // 中间 xor edi,edi 仅为占位对齐 steal 边界, 不影响返回值.
    let code: &[u8] = &[
        0x48, 0x89, 0x5C, 0x24, 0x08, // mov [rsp+8], rbx
        0x48, 0x89, 0x74, 0x24, 0x10, // mov [rsp+10h], rsi
        0x57, // push rdi
        0x48, 0x83, 0xEC, 0x20, // sub rsp, 20h
        0x33, 0xFF, // xor edi, edi   <-- steal 17 落在此指令后
        0x8B, 0xC1, // mov eax, ecx
        0xFF, 0xC0, // inc eax
        0x48, 0x83, 0xC4, 0x20, // add rsp, 20h
        0x5F, // pop rdi
        0x48, 0x8B, 0x74, 0x24, 0x10, // mov rsi, [rsp+10h]
        0x48, 0x8B, 0x5C, 0x24, 0x08, // mov rbx, [rsp+8]
        0xC3, // ret
    ];

    type FnTy = unsafe extern "C" fn(u32) -> u32;
    static ORIG: std::sync::atomic::AtomicPtr<core::ffi::c_void> =
        std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
    static TRAMP_CALLS: AtomicU32 = AtomicU32::new(0);

    TRAMP_CALLS.store(0, Ordering::SeqCst);

    unsafe extern "C" fn tramp_detour(x: u32) -> u32 {
        TRAMP_CALLS.fetch_add(1, Ordering::SeqCst);
        let p = ORIG.load(Ordering::SeqCst);
        if p.is_null() {
            return 0;
        }
        let f: FnTy = std::mem::transmute(p);
        f(x).wrapping_add(10)
    }

    unsafe {
        let mem = VirtualAlloc(
            None,
            code.len(),
            MEM_COMMIT | MEM_RESERVE,
            PAGE_EXECUTE_READWRITE,
        );
        if mem.is_null() {
            return Err(HookError::SelfTestFailed("VirtualAlloc code failed".into()));
        }
        std::ptr::copy_nonoverlapping(code.as_ptr(), mem as *mut u8, code.len());

        let target = mem;
        let detour = tramp_detour as *const core::ffi::c_void;
        let mut hook = match TrampolineHook::new_with_steal(target, detour, 17) {
            Ok(h) => h,
            Err(e) => {
                let _ = VirtualFree(mem, 0, MEM_RELEASE);
                return Err(e);
            }
        };
        ORIG.store(hook.trampoline() as *mut _, Ordering::SeqCst);
        if let Err(e) = hook.attach() {
            ORIG.store(std::ptr::null_mut(), Ordering::SeqCst);
            drop(hook);
            let _ = VirtualFree(mem, 0, MEM_RELEASE);
            return Err(e);
        }

        let f: FnTy = std::mem::transmute(target);
        // 原 +1, detour 再 +10 => 1+1+10 = 12
        let hooked = f(1);
        if hooked != 12 {
            let _ = hook.detach();
            ORIG.store(std::ptr::null_mut(), Ordering::SeqCst);
            drop(hook);
            let _ = VirtualFree(mem, 0, MEM_RELEASE);
            return Err(HookError::SelfTestFailed(format!(
                "trampoline expected 12, got {hooked}"
            )));
        }

        hook.detach()?;
        ORIG.store(std::ptr::null_mut(), Ordering::SeqCst);
        drop(hook);

        let after = f(1);
        let _ = VirtualFree(mem, 0, MEM_RELEASE);
        if after != 2 {
            return Err(HookError::SelfTestFailed(format!(
                "trampoline restore expected 2, got {after}"
            )));
        }
    }

    let n = TRAMP_CALLS.load(Ordering::SeqCst);
    if n == 0 {
        return Err(HookError::SelfTestFailed(
            "trampoline detour did not run".into(),
        ));
    }
    Ok(n)
}

/// IAT 往返自测: 把本模块导入的 `GetTickCount` 换成 detour, 调用后卸掉.
///
/// detour 返回固定哨兵值以证明"走的是 detour 而非原函数"; 卸载后再调必须落回
/// kernel32 原实现. 与生产要 hook 的 `CreateProcessW` 走完全一样的 IAT 机制.
pub fn run_iat_self_test() -> Result<u32> {
    use windows::core::s;
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::Win32::System::SystemInformation::GetTickCount;

    let _guard = SELF_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    const SENTINEL: u32 = 0x5474_1234;
    unsafe extern "system" fn tick_detour() -> u32 {
        CALLS.fetch_add(1, Ordering::SeqCst);
        SENTINEL
    }

    // 逼链接器把 GetTickCount 放进本模块 IAT.
    let _warmup = unsafe { GetTickCount() };
    CALLS.store(0, Ordering::SeqCst);

    unsafe {
        let module = GetModuleHandleW(None).map_err(HookError::Protect)?;
        let target = GetProcAddress(
            GetModuleHandleW(windows::core::w!("kernel32.dll")).map_err(HookError::Protect)?,
            s!("GetTickCount"),
        )
        .ok_or(HookError::ImportNotFound)?;

        let mut hook = IatHook::new(
            module.0.cast(),
            target as *const core::ffi::c_void,
            tick_detour as *const core::ffi::c_void,
        )?;
        hook.attach()?;

        let hooked = GetTickCount();
        if hooked != SENTINEL {
            let _ = hook.detach();
            return Err(HookError::SelfTestFailed(format!(
                "expected sentinel {SENTINEL:#x}, got {hooked:#x}"
            )));
        }

        hook.detach()?;
        // 卸载后应落回真实 tick (非哨兵).
        let after = GetTickCount();
        if after == SENTINEL {
            return Err(HookError::SelfTestFailed(
                "iat detour still active after detach".into(),
            ));
        }
    }

    let n = CALLS.load(Ordering::SeqCst);
    if n == 0 {
        return Err(HookError::SelfTestFailed("iat detour did not run".into()));
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iat_hook_swaps_and_restores() {
        let n = run_iat_self_test().expect("iat self test");
        assert!(n >= 1, "detour should have run at least once, got {n}");
    }

    #[test]
    fn harmless_hook_counts_and_restores() {
        let n = run_harmless_self_test().expect("self test");
        assert!(n >= 1, "detour should have run at least once, got {n}");
        assert_eq!(stt_hook_probe_target(5), 6);
    }

    #[test]
    fn trampoline_steal17_calls_original_and_restores() {
        let n = run_trampoline_self_test().expect("trampoline self test");
        assert!(n >= 1);
    }
}
