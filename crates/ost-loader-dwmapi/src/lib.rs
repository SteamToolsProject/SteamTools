//! Proxy `dwmapi.dll`: forward exports, load host under steam.exe.

#![cfg(windows)]

use std::ffi::CStr;

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleFileNameA(h: *mut core::ffi::c_void, buf: *mut u8, size: u32) -> u32;
    fn LoadLibraryA(name: *const u8) -> *mut core::ffi::c_void;
    fn DisableThreadLibraryCalls(h: *mut core::ffi::c_void) -> i32;
}

/// False only when we are steam.exe and SteamTools.dll failed to load.
unsafe fn load_steam_tools_if_steam() -> bool {
    let mut buf = [0u8; 260];
    let len = GetModuleFileNameA(core::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32);
    if len > 0 && (len as usize) < buf.len() {
        let path = CStr::from_ptr(buf.as_ptr().cast());
        if let Ok(s) = path.to_str() {
            let name = s.rsplit('\\').next().unwrap_or(s);
            if !name.eq_ignore_ascii_case("steam.exe") {
                return true;
            }
        }
    }
    !LoadLibraryA(c"SteamTools.dll".as_ptr().cast()).is_null()
}

const DLL_PROCESS_ATTACH: u32 = 1;

// Called by the OS loader, not by Rust callers.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "system" fn DllMain(
    hinst: *mut core::ffi::c_void,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        unsafe {
            let _ = DisableThreadLibraryCalls(hinst);
            if !load_steam_tools_if_steam() {
                return 0;
            }
        }
    }
    1
}
