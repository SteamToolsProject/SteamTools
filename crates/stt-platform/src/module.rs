//! 已加载模块的基址 / 路径辅助.

use std::ffi::CStr;
use std::path::PathBuf;

use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::{HMODULE, MAX_PATH};
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW, GetProcAddress};
use windows::Win32::System::ProcessStatus::{EnumProcessModules, GetModuleInformation, MODULEINFO};
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
    module_info_by_handle(module_handle(name)?)
}

pub fn module_info_by_handle(module: HMODULE) -> Option<ModuleInfo> {
    let mut info = MODULEINFO::default();
    unsafe {
        GetModuleInformation(
            GetCurrentProcess(),
            module,
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

/// 本进程当前加载的全部模块 (文件名 + 基址/大小).
///
/// IAT hook 不能只盯几个写死的模块名: 调用可能落在中间层里 (实测 Steam 的
/// `tier0_s64.dll` 同样静态导入 `CreateProcessW`), 漏挂就等于没挂.
pub fn enumerate_modules() -> Vec<(String, ModuleInfo)> {
    const SLOT: usize = std::mem::size_of::<HMODULE>();
    let proc = unsafe { GetCurrentProcess() };
    let mut handles: Vec<HMODULE> = vec![HMODULE::default(); 256];
    loop {
        let cb = u32::try_from(handles.len() * SLOT).unwrap_or(u32::MAX);
        let mut needed = 0u32;
        if unsafe { EnumProcessModules(proc, handles.as_mut_ptr(), cb, &mut needed) }.is_err() {
            return Vec::new();
        }
        let want = needed as usize / SLOT;
        if want <= handles.len() {
            handles.truncate(want);
            break;
        }
        // 两次调用之间还可能再加载模块, 多留一点余量.
        handles.resize(want + 32, HMODULE::default());
    }
    handles
        .into_iter()
        .filter_map(|h| {
            let name = module_path(h)?
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())?;
            Some((name, module_info_by_handle(h)?))
        })
        .collect()
}

/// 主可执行模块 (steam.exe) 的基址; IAT hook 要在这张导入表里找槽.
pub fn main_module_base() -> Option<*const core::ffi::c_void> {
    // GetModuleHandleW(NULL) = 主 exe.
    let h = unsafe { GetModuleHandleW(PCWSTR::null()).ok()? };
    if h.0.is_null() {
        return None;
    }
    Some(h.0.cast_const().cast())
}

/// 已加载模块的导出地址; 找 hook 目标用.
pub fn proc_address(module: &str, symbol: &CStr) -> Option<*const core::ffi::c_void> {
    let h = module_handle(module)?;
    let addr = unsafe { GetProcAddress(h, PCSTR(symbol.as_ptr().cast())) }?;
    Some(addr as *const core::ffi::c_void)
}

/// 把本进程模块映像读进 Vec (离线式扫描 / 测试用).
///
/// # Safety
/// `info.base` 须指向可读, 长度为 `info.size` 的模块映射.
pub unsafe fn read_module_bytes(info: ModuleInfo) -> Vec<u8> {
    let slice = std::slice::from_raw_parts(info.base as *const u8, info.size);
    slice.to_vec()
}
