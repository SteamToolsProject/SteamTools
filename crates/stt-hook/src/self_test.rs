//! 无害进程内 hook: 改本地函数, 计数, 再卸掉.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use crate::detour::InlineHook;
use crate::error::{HookError, Result};

static CALLS: AtomicU32 = AtomicU32::new(0);
static SELF_TEST_LOCK: Mutex<()> = Mutex::new(());

#[inline(never)]
#[no_mangle]
pub extern "C" fn stt_hook_probe_target(x: u32) -> u32 {
    x.wrapping_add(1)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harmless_hook_counts_and_restores() {
        let n = run_harmless_self_test().expect("self test");
        assert!(n >= 1, "detour should have run at least once, got {n}");
        assert_eq!(stt_hook_probe_target(5), 6);
    }
}
