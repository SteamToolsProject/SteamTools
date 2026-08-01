//! Windows 小工具: 路径, 数据目录, 线程, 哈希, 模块.

#![cfg(windows)]

mod child_pipe;
mod hash;
mod http;
mod module;

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{HMODULE, MAX_PATH};
use windows::Win32::System::LibraryLoader::{DisableThreadLibraryCalls, GetModuleFileNameW};
use windows::Win32::System::Threading::CreateThread;

pub use child_pipe::{
    crt_fd_block, prepare_devtools_pipe, ChildPipeLaunch, DevToolsPipe, CHILD_READ_FD,
    CHILD_WRITE_FD, EXTENDED_STARTUPINFO_PRESENT, STARTUPINFOW_SIZE,
};
pub use hash::{sha256_bytes, sha256_file};
pub use http::{
    winhttp_get, winhttp_post, winhttp_request, HttpError, HttpMethod, HttpResponse,
    WinHttpGetOptions, WinHttpRequestOptions, WinHttpTimeouts,
};
pub use module::{
    enumerate_modules, main_module_base, module_handle, module_info, module_info_by_handle,
    module_path, module_path_by_name, proc_address, read_module_bytes, ModuleInfo,
};

pub const DATA_DIR_NAME: &str = "steamtools";
pub const LEGACY_DATA_DIR_NAME: &str = "opensteamtool";
pub const HOST_DLL_NAME: &str = "stbase.dll";

pub fn pattern_cache_dir(steam_root: &Path, component: &str) -> PathBuf {
    data_dir(steam_root).join("pattern").join(component)
}

pub fn pattern_cache_file(steam_root: &Path, component: &str, sha256_hex: &str) -> PathBuf {
    pattern_cache_dir(steam_root, component).join(format!("{sha256_hex}.toml"))
}

pub fn legacy_pattern_cache_file(steam_root: &Path, component: &str, sha256_hex: &str) -> PathBuf {
    legacy_data_dir(steam_root)
        .join("pattern")
        .join(component)
        .join(format!("{sha256_hex}.toml"))
}

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

/// 入库意图收件箱: 每行一个 app_id (十进制).
pub fn inbox_dir(steam_root: &Path) -> PathBuf {
    data_dir(steam_root).join("inbox")
}

pub fn ensure_inbox_dir(steam_root: &Path) -> std::io::Result<PathBuf> {
    let dir = inbox_dir(steam_root);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// 商店页注入脚本路径 (CEF/CDP 或后续原生注入共用).
pub fn store_inject_js_path(steam_root: &Path) -> PathBuf {
    data_dir(steam_root).join("store_inject.js")
}

pub fn write_store_inject_js(steam_root: &Path, source: &str) -> std::io::Result<PathBuf> {
    let _ = ensure_data_dir(steam_root)?;
    let path = store_inject_js_path(steam_root);
    std::fs::write(&path, source)?;
    Ok(path)
}

/// CEF 远程调试开关文件 (Steam 根目录, 空文件即可).
///
/// 我们**不再创建**它 (ADR 0010): 调试端点改由 CreateProcessW hook 按会话给,
/// 这里只保留路径, 用来提示用户删掉历史遗留的文件.
pub fn cef_remote_debugging_flag_path(steam_root: &Path) -> PathBuf {
    steam_root.join(".cef-enable-remote-debugging")
}

/// pipe 通道走不通的标记文件.
///
/// 存在即表示上次用 `--remote-debugging-pipe` 没能建立 CDP, 下次直接回退到端口.
/// 删掉它就会重试 pipe (换了 Steam / CEF 版本后值得一试).
pub fn cef_pipe_fallback_marker(steam_root: &Path) -> PathBuf {
    data_dir(steam_root).join("cef_pipe_unsupported")
}

/// # Safety
/// `hinst` 为 DllMain 传入的模块句柄.
pub unsafe fn disable_thread_library_calls_raw(hinst: *mut core::ffi::c_void) {
    let _ = DisableThreadLibraryCalls(HMODULE(hinst));
}

pub type ThreadStart = unsafe extern "system" fn(*mut core::ffi::c_void) -> u32;

/// # Safety
/// 与 Win32 CreateThread 相同约束.
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
        assert_eq!(HOST_DLL_NAME, "stbase.dll");
    }
}
