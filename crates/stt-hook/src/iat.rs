//! IAT hook: 改导入表里的函数指针, 不打 prologue.
//!
//! 适合 hook 由导入表调用的 API (如 steam.exe 的 `CreateProcessW`): 无需反汇编
//! 猜指令边界, 版本无关, 卸载时把原指针写回即可.

use std::ffi::c_void;
use std::ptr;

use windows::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows::Win32::System::Memory::{VirtualProtect, PAGE_PROTECTION_FLAGS, PAGE_READWRITE};
use windows::Win32::System::Threading::GetCurrentProcess;

use crate::error::{HookError, Result};

// PE 头里用到的固定偏移 (PE32+ / x64). 格式冻结多年, 直接按偏移读.
const DOS_E_LFANEW: usize = 0x3c;
const OPT_MAGIC_FROM_NT: usize = 0x18; // NT 头内 OptionalHeader.Magic
const PE32PLUS_MAGIC: u16 = 0x20b;
const DATA_DIR_FROM_NT: usize = 0x88; // NT 头内 DataDirectory[0] (PE32+)
const DIR_ENTRY_IMPORT: usize = 1;
const IMPORT_DESC_SIZE: usize = 20; // IMAGE_IMPORT_DESCRIPTOR
const IMPORT_DESC_FIRST_THUNK: usize = 16;

/// 命中同一函数地址的一组 IAT 槽 (可能多于一个导入描述符引用).
pub struct IatHook {
    slots: Vec<*mut *const c_void>,
    original: *const c_void,
    detour: *const c_void,
    installed: bool,
}

// SAFETY: 槽地址与函数指针都在本进程内固定, 不跨线程解引用; attach/detach 为
// unsafe fn, 由调用方保证同一时刻单一所有者.
unsafe impl Send for IatHook {}

impl IatHook {
    /// 在 `module_base` 的导入表里找出所有指向 `target` 的槽.
    ///
    /// # Safety
    /// `module_base` 须为本进程已加载模块的基址; `target`/`detour` 非空且可调用.
    pub unsafe fn new(
        module_base: *const c_void,
        target: *const c_void,
        detour: *const c_void,
    ) -> Result<Self> {
        if module_base.is_null() || target.is_null() || detour.is_null() {
            return Err(HookError::NullPointer);
        }
        let slots = find_iat_slots(module_base.cast(), target);
        if slots.is_empty() {
            return Err(HookError::ImportNotFound);
        }
        Ok(Self {
            slots,
            original: target,
            detour,
            installed: false,
        })
    }

    pub fn original(&self) -> *const c_void {
        self.original
    }

    pub fn is_installed(&self) -> bool {
        self.installed
    }

    /// 把命中的槽全部改成 detour.
    ///
    /// # Safety
    /// 调用后目标 API 的所有导入调用都会走 detour; detour 须与原型 ABI 一致.
    pub unsafe fn attach(&mut self) -> Result<()> {
        if self.installed {
            return Err(HookError::AlreadyInstalled);
        }
        for &slot in &self.slots {
            write_slot(slot, self.detour)?;
        }
        self.installed = true;
        Ok(())
    }

    /// 把槽写回原函数指针.
    ///
    /// # Safety
    /// 同 `attach`.
    pub unsafe fn detach(&mut self) -> Result<()> {
        if !self.installed {
            return Err(HookError::NotInstalled);
        }
        for &slot in &self.slots {
            write_slot(slot, self.original)?;
        }
        self.installed = false;
        Ok(())
    }
}

impl Drop for IatHook {
    fn drop(&mut self) {
        if self.installed {
            let _ = unsafe { self.detach() };
        }
    }
}

impl std::fmt::Debug for IatHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IatHook")
            .field("slots", &self.slots.len())
            .field("installed", &self.installed)
            .finish()
    }
}

/// 改单个 IAT 槽 (先解保护, 写完刷指令缓存).
unsafe fn write_slot(slot: *mut *const c_void, value: *const c_void) -> Result<()> {
    let mut old = PAGE_PROTECTION_FLAGS(0);
    VirtualProtect(
        slot.cast(),
        std::mem::size_of::<*const c_void>(),
        PAGE_READWRITE,
        &mut old,
    )
    .map_err(HookError::Protect)?;
    ptr::write(slot, value);
    let _ = VirtualProtect(
        slot.cast(),
        std::mem::size_of::<*const c_void>(),
        old,
        &mut old,
    );
    let _ = FlushInstructionCache(
        GetCurrentProcess(),
        Some(slot.cast()),
        std::mem::size_of::<*const c_void>(),
    );
    Ok(())
}

/// 走导入描述符, 收集所有当前值等于 `target` 的 IAT 槽.
unsafe fn find_iat_slots(base: *const u8, target: *const c_void) -> Vec<*mut *const c_void> {
    let mut slots = Vec::new();
    let e_lfanew = ptr::read_unaligned(base.add(DOS_E_LFANEW).cast::<u32>()) as usize;
    let nt = base.add(e_lfanew);
    // 只认 PE32+ (x64); 位数不符直接放弃.
    let magic = ptr::read_unaligned(nt.add(OPT_MAGIC_FROM_NT).cast::<u16>());
    if magic != PE32PLUS_MAGIC {
        return slots;
    }
    let import_rva = ptr::read_unaligned(
        nt.add(DATA_DIR_FROM_NT + DIR_ENTRY_IMPORT * 8)
            .cast::<u32>(),
    ) as usize;
    if import_rva == 0 {
        return slots;
    }

    let mut desc = base.add(import_rva);
    loop {
        let first_thunk =
            ptr::read_unaligned(desc.add(IMPORT_DESC_FIRST_THUNK).cast::<u32>()) as usize;
        if first_thunk == 0 {
            break; // 全零描述符 = 数组结尾
        }
        let mut thunk = base.add(first_thunk) as *mut *const c_void;
        loop {
            let val = ptr::read(thunk);
            if val.is_null() {
                break;
            }
            if val == target {
                slots.push(thunk);
            }
            thunk = thunk.add(1);
        }
        desc = desc.add(IMPORT_DESC_SIZE);
    }
    slots
}
