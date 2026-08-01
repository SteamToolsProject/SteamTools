//! Manifest request code 共用的接收入口.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::Mutex;

use stt_hook::InlineHook;
use stt_metadata::PatternStore;

use crate::request_code::ManifestCodeResponseRewrite;
use crate::verified::resolve_verified_symbol;

const SYMBOL: &str = "RecvPkt";

/// 单帧上限: 8 字节头 + 1024 命令前缀 + 64 KiB 载荷 (与 net_send 共用同一常量).
pub(crate) const MAX_PACKET_SIZE: usize = 8 + 1024 + 65_536;

#[repr(C)]
struct NetPacket {
    connection: usize,
    data: *mut u8,
    size: u32,
}

const _: () = {
    assert!(std::mem::offset_of!(NetPacket, connection) == 0x00);
    assert!(std::mem::offset_of!(NetPacket, data) == 0x08);
    assert!(std::mem::offset_of!(NetPacket, size) == 0x10);
};

type RecvPacketFn = unsafe extern "C" fn(*mut c_void, *mut NetPacket) -> *mut c_void;

static TARGET: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ATTACHED: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static HOOK: Mutex<Option<InlineHook>> = Mutex::new(None);

pub(crate) fn is_attached() -> bool {
    ATTACHED.load(Ordering::SeqCst)
}

pub(crate) fn is_active() -> bool {
    is_attached() && ACTIVE.load(Ordering::SeqCst)
}

pub(crate) fn set_active(active: bool) {
    ACTIVE.store(active && is_attached(), Ordering::SeqCst);
}

pub(crate) fn try_install(patterns: &PatternStore) -> Result<(), String> {
    if is_attached() {
        return Ok(());
    }

    let target = resolve_verified_symbol(patterns, SYMBOL).map_err(|error| error.to_string())?;
    let mut slot = HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(hook) = slot.as_mut() {
        if !hook.is_installed() {
            // # Safety
            // hook 只保存由 exact-SHA 与入口签名共同验证的 RecvPkt 地址.
            unsafe { hook.attach() }
                .map_err(|error| format!("RecvPkt hook reattach 失败: {error}"))?;
        }
        ATTACHED.store(true, Ordering::SeqCst);
        return Ok(());
    }

    // # Safety
    // 当前 DLL SHA, RVA 和入口签名已由 resolve_verified_symbol 验证.
    let mut hook = unsafe { InlineHook::new(target, hk_recv_packet as *const c_void) }
        .map_err(|error| format!("RecvPkt hook 初始化失败: {error}"))?;
    TARGET.store(target, Ordering::SeqCst);
    // # Safety
    // target 与 detour ABI 均已按 exact-SHA 样本验证.
    if let Err(error) = unsafe { hook.attach() } {
        TARGET.store(std::ptr::null_mut(), Ordering::SeqCst);
        return Err(format!("RecvPkt hook attach 失败: {error}"));
    }
    *slot = Some(hook);
    ATTACHED.store(true, Ordering::SeqCst);
    Ok(())
}

/// # Safety
/// 由已验证 ABI 的 RecvPkt 入口调用; packet 布局来自 exact-SHA 规范化函数.
unsafe extern "C" fn hk_recv_packet(object: *mut c_void, packet: *mut NetPacket) -> *mut c_void {
    let target = TARGET.load(Ordering::SeqCst);
    if target.is_null() {
        return std::ptr::null_mut();
    }
    let Some(packet) = packet.as_mut() else {
        return call_original(target, object, packet);
    };
    if !is_active() || packet.data.is_null() || packet.size as usize > MAX_PACKET_SIZE {
        return call_original(target, object, packet);
    }

    let input = std::slice::from_raw_parts(packet.data.cast_const(), packet.size as usize);
    match crate::request_code::rewrite_manifest_code_runtime_response(2, input) {
        ManifestCodeResponseRewrite::Passthrough => call_original(target, object, packet),
        ManifestCodeResponseRewrite::Rewritten {
            packet: mut data, ..
        } => {
            let Ok(size) = u32::try_from(data.len()) else {
                return call_original(target, object, packet);
            };
            call_with_rewritten_packet(packet, &mut data, size, |packet| {
                call_original(target, object, packet)
            })
        }
    }
}

unsafe fn call_with_rewritten_packet(
    packet: &mut NetPacket,
    data: &mut [u8],
    size: u32,
    call: impl FnOnce(*mut NetPacket) -> *mut c_void,
) -> *mut c_void {
    let original_data = packet.data;
    let original_size = packet.size;
    packet.data = data.as_mut_ptr();
    packet.size = size;
    let result = call(packet);
    packet.data = original_data;
    packet.size = original_size;
    result
}

unsafe fn call_original(
    target: *mut c_void,
    object: *mut c_void,
    packet: *mut NetPacket,
) -> *mut c_void {
    call_original_while_unhooked(|| {
        let original: RecvPacketFn = std::mem::transmute(target);
        original(object, packet)
    })
    .unwrap_or(std::ptr::null_mut())
}

/// # Safety
/// `call` 只能调用 TARGET 指向的原入口; 调用期间入口补丁已卸下.
unsafe fn call_original_while_unhooked(call: impl FnOnce() -> *mut c_void) -> Option<*mut c_void> {
    let mut slot = HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let hook = slot.as_mut()?;
    if hook.detach().is_err() {
        return None;
    }
    let result = call();
    if hook.attach().is_err() {
        ATTACHED.store(false, Ordering::SeqCst);
        ACTIVE.store(false, Ordering::SeqCst);
        crate::net_send::deactivate_consumer(crate::DownloadCapability::RequestCode);
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewritten_packet_is_visible_only_during_original_call() {
        let mut original = vec![1, 2, 3];
        let original_ptr = original.as_mut_ptr();
        let mut packet = NetPacket {
            connection: 7,
            data: original_ptr,
            size: original.len() as u32,
        };
        let mut rewritten = vec![4, 5, 6, 7];
        let rewritten_ptr = rewritten.as_mut_ptr();
        let marker = std::ptr::dangling_mut::<c_void>();

        let result = unsafe {
            call_with_rewritten_packet(&mut packet, &mut rewritten, 4, |current| {
                let current = &*current;
                assert_eq!(current.data, rewritten_ptr);
                assert_eq!(current.size, 4);
                marker
            })
        };

        assert_eq!(result, marker);
        assert_eq!(packet.data, original_ptr);
        assert_eq!(packet.size, 3);
    }
}
