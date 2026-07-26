//! 最小 x64 内联 JMP detour (不依赖 C++ Detours).

use std::ptr;

use windows::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, VirtualProtect, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE,
    PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS,
};
use windows::Win32::System::Threading::GetCurrentProcess;

use crate::error::{HookError, Result};

const PATCH_LEN: usize = 12; // mov rax, imm64; jmp rax
const MAX_STEAL: usize = 32;

/// 写在目标 prologue 上的 64 位绝对跳转.
#[derive(Debug)]
pub struct InlineHook {
    target: *mut u8,
    detour: *const u8,
    original: [u8; PATCH_LEN],
    installed: bool,
}

// 地址仅本进程有效; hook 由安装侧线程集使用.
unsafe impl Send for InlineHook {}

impl InlineHook {
    /// # Safety
    /// `target` 须为可执行函数入口且至少 12 字节可安全改写;
    /// `detour` 须匹配调用约定.
    pub unsafe fn new(
        target: *mut core::ffi::c_void,
        detour: *const core::ffi::c_void,
    ) -> Result<Self> {
        if target.is_null() || detour.is_null() {
            return Err(HookError::NullPointer);
        }
        if cfg!(not(target_arch = "x86_64")) {
            return Err(HookError::UnsupportedArch);
        }
        let target = target as *mut u8;
        let mut original = [0u8; PATCH_LEN];
        ptr::copy_nonoverlapping(target, original.as_mut_ptr(), PATCH_LEN);
        Ok(Self {
            target,
            detour: detour as *const u8,
            original,
            installed: false,
        })
    }

    pub fn is_installed(&self) -> bool {
        self.installed
    }

    pub fn target(&self) -> *mut u8 {
        self.target
    }

    /// # Safety
    /// 同 `new`; 不得与其它写者竞态同一页.
    pub unsafe fn attach(&mut self) -> Result<()> {
        if self.installed {
            return Err(HookError::AlreadyInstalled);
        }
        let patch = abs_jmp_patch(self.detour as u64);
        with_rwx(self.target, PATCH_LEN, || {
            ptr::copy_nonoverlapping(patch.as_ptr(), self.target, PATCH_LEN);
        })?;
        self.installed = true;
        Ok(())
    }

    /// # Safety
    /// 恢复已保存的 prologue; 补丁区不得并发执行.
    pub unsafe fn detach(&mut self) -> Result<()> {
        if !self.installed {
            return Err(HookError::NotInstalled);
        }
        with_rwx(self.target, PATCH_LEN, || {
            ptr::copy_nonoverlapping(self.original.as_ptr(), self.target, PATCH_LEN);
        })?;
        self.installed = false;
        Ok(())
    }
}

impl Drop for InlineHook {
    fn drop(&mut self) {
        if self.installed {
            // 尽力恢复; 析构/退出时忽略错误.
            let _ = unsafe { self.detach() };
        }
    }
}

/// 带 trampoline 的 detour: detour 内可调用 `trampoline()` 执行原函数.
///
/// `steal_len` 须覆盖完整指令 (调用方保证边界); 目标处写 12 字节 jmp,
/// 若 steal_len > 12 则多余字节填 nop.
#[derive(Debug)]
pub struct TrampolineHook {
    target: *mut u8,
    detour: *const u8,
    original: [u8; MAX_STEAL],
    steal_len: usize,
    trampoline: *mut u8,
    installed: bool,
}

// SAFETY: 两个裸指针都指向本进程内的固定地址 (目标函数入口与 VirtualAlloc 的
// trampoline 页), 不随线程变化, 也不被本类型跨线程解引用 — attach/detach 都是
// `unsafe fn`, 由调用方保证同一时刻只有一个所有者. 因此把所有权移到别的线程是安全的.
unsafe impl Send for TrampolineHook {}

impl TrampolineHook {
    /// # Safety
    /// 同 `InlineHook::new`; 入口前 `steal_len` 字节须为完整指令 (12..=32).
    pub unsafe fn new(
        target: *mut core::ffi::c_void,
        detour: *const core::ffi::c_void,
    ) -> Result<Self> {
        Self::new_with_steal(target, detour, PATCH_LEN)
    }

    /// # Safety
    /// 同 `new`; `steal_len` 必须落在指令边界上.
    pub unsafe fn new_with_steal(
        target: *mut core::ffi::c_void,
        detour: *const core::ffi::c_void,
        steal_len: usize,
    ) -> Result<Self> {
        if target.is_null() || detour.is_null() {
            return Err(HookError::NullPointer);
        }
        if cfg!(not(target_arch = "x86_64")) {
            return Err(HookError::UnsupportedArch);
        }
        if !(PATCH_LEN..=MAX_STEAL).contains(&steal_len) {
            return Err(HookError::InvalidStealLen {
                got: steal_len,
                min: PATCH_LEN,
                max: MAX_STEAL,
            });
        }
        let target = target as *mut u8;
        let mut original = [0u8; MAX_STEAL];
        ptr::copy_nonoverlapping(target, original.as_mut_ptr(), steal_len);

        let trampoline_size = steal_len + PATCH_LEN;
        let trampoline = VirtualAlloc(
            None,
            trampoline_size,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_EXECUTE_READWRITE,
        );
        if trampoline.is_null() {
            return Err(HookError::TrampolineAlloc);
        }
        let trampoline = trampoline as *mut u8;
        ptr::copy_nonoverlapping(original.as_ptr(), trampoline, steal_len);
        let back = abs_jmp_patch(target as u64 + steal_len as u64);
        ptr::copy_nonoverlapping(back.as_ptr(), trampoline.add(steal_len), PATCH_LEN);
        let _ = FlushInstructionCache(
            GetCurrentProcess(),
            Some(trampoline as *const _),
            trampoline_size,
        );

        Ok(Self {
            target,
            detour: detour as *const u8,
            original,
            steal_len,
            trampoline,
            installed: false,
        })
    }

    pub fn is_installed(&self) -> bool {
        self.installed
    }

    pub fn trampoline(&self) -> *const u8 {
        self.trampoline
    }

    pub fn target(&self) -> *mut u8 {
        self.target
    }

    pub fn steal_len(&self) -> usize {
        self.steal_len
    }

    /// # Safety
    /// 同 `InlineHook::attach`.
    pub unsafe fn attach(&mut self) -> Result<()> {
        if self.installed {
            return Err(HookError::AlreadyInstalled);
        }
        let patch = abs_jmp_patch(self.detour as u64);
        with_rwx(self.target, self.steal_len, || {
            ptr::copy_nonoverlapping(patch.as_ptr(), self.target, PATCH_LEN);
            // 覆盖被偷走的剩余字节, 避免半条指令.
            for i in PATCH_LEN..self.steal_len {
                *self.target.add(i) = 0x90;
            }
        })?;
        self.installed = true;
        Ok(())
    }

    /// # Safety
    /// 同 `InlineHook::detach`.
    pub unsafe fn detach(&mut self) -> Result<()> {
        if !self.installed {
            return Err(HookError::NotInstalled);
        }
        with_rwx(self.target, self.steal_len, || {
            ptr::copy_nonoverlapping(self.original.as_ptr(), self.target, self.steal_len);
        })?;
        self.installed = false;
        Ok(())
    }
}

impl Drop for TrampolineHook {
    fn drop(&mut self) {
        if self.installed {
            let _ = unsafe { self.detach() };
        }
        if !self.trampoline.is_null() {
            unsafe {
                let _ = VirtualFree(self.trampoline as *mut _, 0, MEM_RELEASE);
            }
            self.trampoline = ptr::null_mut();
        }
    }
}

fn abs_jmp_patch(dest: u64) -> [u8; PATCH_LEN] {
    let mut patch = [0u8; PATCH_LEN];
    patch[0] = 0x48;
    patch[1] = 0xB8;
    patch[2..10].copy_from_slice(&dest.to_le_bytes());
    patch[10] = 0xFF;
    patch[11] = 0xE0;
    patch
}

unsafe fn with_rwx(addr: *mut u8, len: usize, f: impl FnOnce()) -> Result<()> {
    let mut old = PAGE_PROTECTION_FLAGS(0);
    VirtualProtect(addr as *const _, len, PAGE_EXECUTE_READWRITE, &mut old)
        .map_err(HookError::Protect)?;
    f();
    let mut tmp = PAGE_PROTECTION_FLAGS(0);
    let _ = VirtualProtect(addr as *const _, len, old, &mut tmp);
    // CPU 可能缓存了旧 prologue; 强制取新指令.
    let _ = FlushInstructionCache(GetCurrentProcess(), Some(addr as *const _), len);
    Ok(())
}

/// 事务式批量 (失败则回滚已 attach 的).
#[derive(Default)]
pub struct HookTransaction {
    hooks: Vec<InlineHook>,
}

impl HookTransaction {
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    pub fn push(&mut self, hook: InlineHook) {
        self.hooks.push(hook);
    }

    /// # Safety
    /// 各 hook 的 Safety 要求均适用.
    pub unsafe fn commit_attach(&mut self) -> Result<()> {
        for i in 0..self.hooks.len() {
            if let Err(e) = self.hooks[i].attach() {
                for j in (0..i).rev() {
                    let _ = self.hooks[j].detach();
                }
                return Err(e);
            }
        }
        Ok(())
    }

    /// # Safety
    /// 各 hook 的 Safety 要求均适用.
    pub unsafe fn commit_detach(&mut self) -> Result<()> {
        let mut first_err = None;
        for h in self.hooks.iter_mut().rev() {
            if h.is_installed() {
                if let Err(e) = h.detach() {
                    first_err.get_or_insert(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    pub fn into_hooks(self) -> Vec<InlineHook> {
        self.hooks
    }
}
