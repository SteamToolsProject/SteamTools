//! Small Windows helpers: paths, data dir, threads from DllMain.

#![cfg(windows)]

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{HMODULE, MAX_PATH};
use windows::Win32::System::LibraryLoader::{DisableThreadLibraryCalls, GetModuleFileNameW};
use windows::Win32::System::Threading::CreateThread;

pub const DATA_DIR_NAME: &str = "steamtools";
pub const LEGACY_DATA_DIR_NAME: &str = "opensteamtool";
pub const HOST_DLL_NAME: &str = "SteamTools.dll";

pub fn module_directory(module: HMODULE) -> Option<PathBuf> {
    let mut buf = vec![0u16; MAX_PATH as usize];
    let len = unsafe { GetModuleFileNameW(module, &mut buf) };
    if len == 0 || (len as usize) >= buf.len() {
        return None;
    }
    buf.truncate(len as usize);
    let path = PathBuf::from(String::from_utf16_lossy(&buf));
    path.parent().map(Path::to_path_buf)
}

pub fn steam_root_from_module(module: HMODULE) -> Option<PathBuf> {
    module_directory(module)
}

pub fn data_dir(steam_root: &Path) -> PathBuf {
    steam_root.join(DATA_DIR_NAME)
}

pub fn legacy_data_dir(steam_root: &Path) -> PathBuf {
    steam_root.join(LEGACY_DATA_DIR_NAME)
}

pub fn legacy_data_dir_exists(steam_root: &Path) -> bool {
    legacy_data_dir(steam_root).is_dir()
}

pub fn ensure_data_dir(steam_root: &Path) -> std::io::Result<PathBuf> {
    let dir = data_dir(steam_root);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn host_log_path(steam_root: &Path) -> PathBuf {
    data_dir(steam_root).join("host.log")
}

/// # Safety
/// `hinst` is the module handle from DllMain.
pub unsafe fn disable_thread_library_calls_raw(hinst: *mut core::ffi::c_void) {
    let _ = DisableThreadLibraryCalls(HMODULE(hinst));
}

pub type ThreadStart = unsafe extern "system" fn(*mut core::ffi::c_void) -> u32;

/// # Safety
/// Same rules as Win32 CreateThread.
pub unsafe fn spawn_thread_raw(
    start: ThreadStart,
    parameter: *mut core::ffi::c_void,
) -> windows::core::Result<()> {
    CreateThread(
        None,
        0,
        Some(start),
        Some(parameter),
        Default::default(),
        None,
    )?;
    Ok(())
}

pub fn steam_root_from_raw(hinst: *mut core::ffi::c_void) -> Option<PathBuf> {
    steam_root_from_module(HMODULE(hinst))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn data_dir_joins_steamtools() {
        let root = Path::new(r"C:\Steam");
        assert_eq!(data_dir(root), PathBuf::from(r"C:\Steam\steamtools"));
        assert_eq!(
            host_log_path(root),
            PathBuf::from(r"C:\Steam\steamtools\host.log")
        );
    }

    #[test]
    fn host_dll_name() {
        assert_eq!(HOST_DLL_NAME, "SteamTools.dll");
    }
}
