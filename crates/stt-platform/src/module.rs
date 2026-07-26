//! 已加载模块的基址 / 路径辅助.

use std::path::PathBuf;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HMODULE, MAX_PATH};
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::System::ProcessStatus::{GetModuleInformation, MODULEINFO};
use windows::Win32::System::Threading::GetCurrentProcess;

#[derive(Debug, Clone, Copy)]
pub struct ModuleInfo {
    pub base: *mut core::ffi::c_void,
    pub size: usize,
}

// SAFETY: base 只在本进程有意义; 调用方当地址用.
unsafe impl Send for ModuleInfo {}
unsafe impl Sync for ModuleInfo {}

pub fn module_handle(name: &str) -> Option<HMODULE> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { GetModuleHandleW(PCWSTR(wide.as_ptr())).ok() }
}

pub fn module_path_by_name(name: &str) -> Option<PathBuf> {
    let h = module_handle(name)?;
    module_path(h)
}

pub fn module_path(module: HMODULE) -> Option<PathBuf> {
    let mut buf = vec![0u16; MAX_PATH as usize];
    let len = unsafe { GetModuleFileNameW(module, &mut buf) };
    if len == 0 || (len as usize) >= buf.len() {
        return None;
    }
    buf.truncate(len as usize);
    Some(PathBuf::from(String::from_utf16_lossy(&buf)))
}

pub fn module_info(name: &str) -> Option<ModuleInfo> {
    let h = module_handle(name)?;
    let mut info = MODULEINFO::default();
    unsafe {
        GetModuleInformation(
            GetCurrentProcess(),
            h,
            &mut info,
            std::mem::size_of::<MODULEINFO>() as u32,
        )
        .ok()?;
    }
    if info.lpBaseOfDll.is_null() || info.SizeOfImage == 0 {
        return None;
    }
    Some(ModuleInfo {
        base: info.lpBaseOfDll,
        size: info.SizeOfImage as usize,
    })
}

/// 把本进程模块映像读进 Vec (离线式扫描 / 测试用).
///
/// # Safety
/// `info.base` 须指向可读, 长度为 `info.size` 的模块映射.
pub unsafe fn read_module_bytes(info: ModuleInfo) -> Vec<u8> {
    let slice = std::slice::from_raw_parts(info.base as *const u8, info.size);
    slice.to_vec()
}
