//! Loaded module base / path helpers.

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

// SAFETY: raw base is only meaningful in this process; callers treat it as an address.
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

/// Read a process-local module image into a Vec (for offline-style scans / tests).
///
/// # Safety
/// `info.base` must point at a readable module mapping of `info.size` bytes.
pub unsafe fn read_module_bytes(info: ModuleInfo) -> Vec<u8> {
    let slice = std::slice::from_raw_parts(info.base as *const u8, info.size);
    slice.to_vec()
}
