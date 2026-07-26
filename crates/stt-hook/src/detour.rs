//! 最小 x64 内联 JMP detour (不依赖 C++ Detours).

use std::ptr;

use windows::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows::Win32::System::Memory::{
    VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS,
};
use windows::Win32::System::Threading::GetCurrentProcess;

use crate::error::{HookError, Result};

const PATCH_LEN: usize = 12; // mov rax, imm64; jmp rax

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
        let mut patch = [0u8; PATCH_LEN];
        // mov rax, imm64
        patch[0] = 0x48;
        patch[1] = 0xB8;
        let addr = self.detour as u64;
        patch[2..10].copy_from_slice(&addr.to_le_bytes());
        // jmp rax
        patch[10] = 0xFF;
        patch[11] = 0xE0;

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
