//! 代理 `xinput1_4.dll`, 在 steam.exe 下加载宿主.

#![cfg(windows)]
// 面向 OS 的导出; 安全性由调用方 (游戏 / Steam) 负责.
#![allow(clippy::missing_safety_doc)]

use std::ffi::CStr;
use std::sync::OnceLock;

const ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;

type FnGetState = unsafe extern "system" fn(u32, *mut core::ffi::c_void) -> u32;
type FnSetState = unsafe extern "system" fn(u32, *mut core::ffi::c_void) -> u32;
type FnGetCaps = unsafe extern "system" fn(u32, u32, *mut core::ffi::c_void) -> u32;
type FnEnable = unsafe extern "system" fn(i32);
type FnGetAudio = unsafe extern "system" fn(u32, *mut u16, *mut u32, *mut u16, *mut u32) -> u32;
type FnGetBattery = unsafe extern "system" fn(u32, u8, *mut core::ffi::c_void) -> u32;
type FnGetKeystroke = unsafe extern "system" fn(u32, u32, *mut core::ffi::c_void) -> u32;

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleFileNameA(h: *mut core::ffi::c_void, buf: *mut u8, size: u32) -> u32;
    fn LoadLibraryA(name: *const u8) -> *mut core::ffi::c_void;
    fn GetProcAddress(module: *mut core::ffi::c_void, name: *const u8) -> *mut core::ffi::c_void;
    fn GetSystemDirectoryA(buf: *mut u8, size: u32) -> u32;
    fn DisableThreadLibraryCalls(h: *mut core::ffi::c_void) -> i32;
}

struct RealXInput {
    get_state: Option<FnGetState>,
    set_state: Option<FnSetState>,
    get_caps: Option<FnGetCaps>,
    enable: Option<FnEnable>,
    get_audio: Option<FnGetAudio>,
    get_battery: Option<FnGetBattery>,
    get_keystroke: Option<FnGetKeystroke>,
    o100: *mut core::ffi::c_void,
    o101: *mut core::ffi::c_void,
    o102: *mut core::ffi::c_void,
    o103: *mut core::ffi::c_void,
    o104: *mut core::ffi::c_void,
    o108: *mut core::ffi::c_void,
}

// 只加载一次, 初始化后不再改.
unsafe impl Send for RealXInput {}
unsafe impl Sync for RealXInput {}

fn empty_real() -> RealXInput {
    RealXInput {
        get_state: None,
        set_state: None,
        get_caps: None,
        enable: None,
        get_audio: None,
        get_battery: None,
        get_keystroke: None,
        o100: core::ptr::null_mut(),
        o101: core::ptr::null_mut(),
        o102: core::ptr::null_mut(),
        o103: core::ptr::null_mut(),
        o104: core::ptr::null_mut(),
        o108: core::ptr::null_mut(),
    }
}

unsafe fn proc_opt<T>(module: *mut core::ffi::c_void, name: &CStr) -> Option<T> {
    let p = GetProcAddress(module, name.as_ptr().cast());
    if p.is_null() {
        None
    } else {
        Some(core::mem::transmute_copy(&p))
    }
}

fn load_real_xinput() -> RealXInput {
    unsafe {
        let mut sys = [0u8; 260];
        let n = GetSystemDirectoryA(sys.as_mut_ptr(), sys.len() as u32);
        if n == 0 || (n as usize) >= sys.len() {
            return empty_real();
        }
        let dir = CStr::from_ptr(sys.as_ptr().cast())
            .to_string_lossy()
            .into_owned();
        let mut path = dir.into_bytes();
        path.extend_from_slice(b"\\xinput1_4.dll\0");
        let module = LoadLibraryA(path.as_ptr());
        if module.is_null() {
            return empty_real();
        }

        RealXInput {
            get_state: proc_opt(module, c"XInputGetState"),
            set_state: proc_opt(module, c"XInputSetState"),
            get_caps: proc_opt(module, c"XInputGetCapabilities"),
            enable: proc_opt(module, c"XInputEnable"),
            get_audio: proc_opt(module, c"XInputGetAudioDeviceIds"),
            get_battery: proc_opt(module, c"XInputGetBatteryInformation"),
            get_keystroke: proc_opt(module, c"XInputGetKeystroke"),
            o100: GetProcAddress(module, 100 as *const u8),
            o101: GetProcAddress(module, 101 as *const u8),
            o102: GetProcAddress(module, 102 as *const u8),
            o103: GetProcAddress(module, 103 as *const u8),
            o104: GetProcAddress(module, 104 as *const u8),
            o108: GetProcAddress(module, 108 as *const u8),
        }
    }
}

fn real() -> &'static RealXInput {
    static REAL: OnceLock<RealXInput> = OnceLock::new();
    REAL.get_or_init(load_real_xinput)
}

/// 在 CEF 起来前放空文件, 商店注入无需用户手开调试.
///
/// 与 `stt_platform::ensure_cef_remote_debugging_flag` 重复是有意的: loader 要赶在
/// host 加载前落文件, 且按 ADR 0009 保持零依赖, 不为一行 IO 拉进 windows crate.
fn ensure_cef_remote_debugging_flag(steam_exe: &str) {
    use std::path::Path;
    let Some(dir) = Path::new(steam_exe).parent() else {
        return;
    };
    let flag = dir.join(".cef-enable-remote-debugging");
    if flag.is_file() {
        return;
    }
    let _ = std::fs::File::create(flag);
}

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
            ensure_cef_remote_debugging_flag(s);
        }
    }
    !LoadLibraryA(c"SteamTools.dll".as_ptr().cast()).is_null()
}

#[no_mangle]
pub unsafe extern "system" fn XInputGetState(user: u32, state: *mut core::ffi::c_void) -> u32 {
    match real().get_state {
        Some(f) => f(user, state),
        None => ERROR_DEVICE_NOT_CONNECTED,
    }
}

#[no_mangle]
pub unsafe extern "system" fn XInputSetState(user: u32, vib: *mut core::ffi::c_void) -> u32 {
    match real().set_state {
        Some(f) => f(user, vib),
        None => ERROR_DEVICE_NOT_CONNECTED,
    }
}

#[no_mangle]
pub unsafe extern "system" fn XInputGetCapabilities(
    user: u32,
    flags: u32,
    caps: *mut core::ffi::c_void,
) -> u32 {
    match real().get_caps {
        Some(f) => f(user, flags, caps),
        None => ERROR_DEVICE_NOT_CONNECTED,
    }
}

#[no_mangle]
pub unsafe extern "system" fn XInputEnable(enable: i32) {
    if let Some(f) = real().enable {
        f(enable);
    }
}

#[no_mangle]
pub unsafe extern "system" fn XInputGetAudioDeviceIds(
    user: u32,
    render_id: *mut u16,
    render_count: *mut u32,
    capture_id: *mut u16,
    capture_count: *mut u32,
) -> u32 {
    match real().get_audio {
        Some(f) => f(user, render_id, render_count, capture_id, capture_count),
        None => ERROR_DEVICE_NOT_CONNECTED,
    }
}

#[no_mangle]
pub unsafe extern "system" fn XInputGetBatteryInformation(
    user: u32,
    dev_type: u8,
    info: *mut core::ffi::c_void,
) -> u32 {
    match real().get_battery {
        Some(f) => f(user, dev_type, info),
        None => ERROR_DEVICE_NOT_CONNECTED,
    }
}

#[no_mangle]
pub unsafe extern "system" fn XInputGetKeystroke(
    user: u32,
    reserved: u32,
    keystroke: *mut core::ffi::c_void,
) -> u32 {
    match real().get_keystroke {
        Some(f) => f(user, reserved, keystroke),
        None => ERROR_DEVICE_NOT_CONNECTED,
    }
}

#[no_mangle]
pub unsafe extern "system" fn XInputOrdinal100(a1: u32, a2: *mut core::ffi::c_void) -> u32 {
    let p = real().o100;
    if p.is_null() {
        return ERROR_DEVICE_NOT_CONNECTED;
    }
    let f: unsafe extern "system" fn(u32, *mut core::ffi::c_void) -> u32 = core::mem::transmute(p);
    f(a1, a2)
}

#[no_mangle]
pub unsafe extern "system" fn XInputOrdinal101(
    a1: u32,
    a2: u32,
    a3: *mut core::ffi::c_void,
) -> u32 {
    let p = real().o101;
    if p.is_null() {
        return ERROR_DEVICE_NOT_CONNECTED;
    }
    let f: unsafe extern "system" fn(u32, u32, *mut core::ffi::c_void) -> u32 =
        core::mem::transmute(p);
    f(a1, a2, a3)
}

#[no_mangle]
pub unsafe extern "system" fn XInputOrdinal102(a1: u32) -> u32 {
    let p = real().o102;
    if p.is_null() {
        return ERROR_DEVICE_NOT_CONNECTED;
    }
    let f: unsafe extern "system" fn(u32) -> u32 = core::mem::transmute(p);
    f(a1)
}

#[no_mangle]
pub unsafe extern "system" fn XInputOrdinal103(a1: u32) -> u32 {
    let p = real().o103;
    if p.is_null() {
        return ERROR_DEVICE_NOT_CONNECTED;
    }
    let f: unsafe extern "system" fn(u32) -> u32 = core::mem::transmute(p);
    f(a1)
}

#[no_mangle]
pub unsafe extern "system" fn XInputOrdinal104(a1: u32, a2: *mut core::ffi::c_void) -> u32 {
    let p = real().o104;
    if p.is_null() {
        return ERROR_DEVICE_NOT_CONNECTED;
    }
    let f: unsafe extern "system" fn(u32, *mut core::ffi::c_void) -> u32 = core::mem::transmute(p);
    f(a1, a2)
}

#[no_mangle]
pub unsafe extern "system" fn XInputOrdinal108(
    a1: u32,
    a2: *mut core::ffi::c_void,
    a3: *mut core::ffi::c_void,
    a4: *mut core::ffi::c_void,
    a5: *mut core::ffi::c_void,
) -> u32 {
    let p = real().o108;
    if p.is_null() {
        return ERROR_DEVICE_NOT_CONNECTED;
    }
    let f: unsafe extern "system" fn(
        u32,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
    ) -> u32 = core::mem::transmute(p);
    f(a1, a2, a3, a4, a5)
}

const DLL_PROCESS_ATTACH: u32 = 1;

// 由系统加载器调用, 不是 Rust 侧直接调.
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
            let _ = real();
            if !load_steam_tools_if_steam() {
                return 0;
            }
        }
    }
    1
}
