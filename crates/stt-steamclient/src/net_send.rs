//! 多个 NetPacket consumer 共用的发送入口.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::Mutex;

use stt_hook::InlineHook;
use stt_metadata::PatternStore;

use crate::verified::resolve_verified_symbol;
use crate::{DownloadCapability, DownloadCapabilityStatus, DownloadKitReport};

const SYMBOL: &str = "BBuildAndAsyncSendFrame";

type BuildAndSendFrameFn = unsafe extern "C" fn(*mut c_void, u8, *mut u8, u32) -> u8;

static TARGET: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ATTACHED: AtomicBool = AtomicBool::new(false);
static TOKEN_ACTIVE: AtomicBool = AtomicBool::new(false);
static REQUEST_CODE_ACTIVE: AtomicBool = AtomicBool::new(false);
static HOOK: Mutex<Option<InlineHook>> = Mutex::new(None);

pub(crate) fn is_consumer_attached(capability: DownloadCapability) -> bool {
    ATTACHED.load(Ordering::SeqCst) && consumer_active(capability).load(Ordering::SeqCst)
}

pub(crate) fn try_install_consumer(
    report: &mut DownloadKitReport,
    patterns: &PatternStore,
    capability_kind: DownloadCapability,
    label: &str,
) {
    let Some(capability) = report
        .capabilities
        .iter_mut()
        .find(|item| item.capability == capability_kind)
    else {
        return;
    };

    if is_consumer_attached(capability_kind) {
        capability.status = DownloadCapabilityStatus::HooksAttached;
        capability.detail = Some(format!("{label} hook 已挂上"));
        return;
    }
    if capability.status != DownloadCapabilityStatus::LogicOnly {
        return;
    }

    let target = match resolve_verified_symbol(patterns, SYMBOL) {
        Ok(target) => target,
        Err(error) => {
            capability.detail = Some(error.to_string());
            return;
        }
    };
    let mut slot = HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(hook) = slot.as_mut() {
        if !hook.is_installed() {
            if let Err(error) = unsafe { hook.attach() } {
                capability.detail = Some(format!("{label} hook reattach 失败: {error}"));
                return;
            }
        }
        ATTACHED.store(true, Ordering::SeqCst);
        consumer_active(capability_kind).store(true, Ordering::SeqCst);
        capability.status = DownloadCapabilityStatus::HooksAttached;
        capability.detail = Some(format!("{label} hook 已挂上"));
        return;
    }

    // # Safety
    // 当前 DLL SHA, RVA 和入口签名已由 resolve_verified_symbol 验证.
    let mut hook = match unsafe { InlineHook::new(target, hk_send_frame as *const c_void) } {
        Ok(hook) => hook,
        Err(error) => {
            capability.detail = Some(format!("{label} hook 初始化失败: {error}"));
            return;
        }
    };
    TARGET.store(target, Ordering::SeqCst);
    if let Err(error) = unsafe { hook.attach() } {
        TARGET.store(std::ptr::null_mut(), Ordering::SeqCst);
        capability.detail = Some(format!("{label} hook attach 失败: {error}"));
        return;
    }
    *slot = Some(hook);
    ATTACHED.store(true, Ordering::SeqCst);
    consumer_active(capability_kind).store(true, Ordering::SeqCst);
    capability.status = DownloadCapabilityStatus::HooksAttached;
    capability.detail = Some(format!("{label} hook 已挂上"));
}

fn consumer_active(capability: DownloadCapability) -> &'static AtomicBool {
    match capability {
        DownloadCapability::AccessToken => &TOKEN_ACTIVE,
        DownloadCapability::RequestCode => &REQUEST_CODE_ACTIVE,
        DownloadCapability::ManifestOverride | DownloadCapability::DepotKey => {
            unreachable!("non-NetPacket capability")
        }
    }
}

/// # Safety
/// 由已验证 ABI 的 BBuildAndAsyncSendFrame 入口调用; 指针沿用原函数契约.
unsafe extern "C" fn hk_send_frame(
    object: *mut c_void,
    opcode: u8,
    data: *mut u8,
    size: u32,
) -> u8 {
    let target = TARGET.load(Ordering::SeqCst);
    if target.is_null() {
        return 0;
    }

    #[cfg(feature = "download-request-code")]
    if REQUEST_CODE_ACTIVE.load(Ordering::SeqCst) && !data.is_null() {
        let packet = std::slice::from_raw_parts(data.cast_const(), size as usize);
        crate::request_code::submit_manifest_code_frame(u32::from(opcode), packet);
    }

    #[cfg(feature = "download-token")]
    let rewrite = if TOKEN_ACTIVE.load(Ordering::SeqCst) {
        crate::token::record_access_token_call();
        if data.is_null() {
            crate::token::AccessTokenRewrite::Passthrough
        } else {
            let packet = std::slice::from_raw_parts(data.cast_const(), size as usize);
            crate::token::rewrite_access_token_snapshot_frame(u32::from(opcode), packet)
        }
    } else {
        crate::token::AccessTokenRewrite::Passthrough
    };

    #[cfg(not(feature = "download-token"))]
    let rewrite = ();

    call_rewritten_or_original(target, object, opcode, data, size, rewrite)
}

#[cfg(feature = "download-token")]
unsafe fn call_rewritten_or_original(
    target: *mut c_void,
    object: *mut c_void,
    opcode: u8,
    data: *mut u8,
    size: u32,
    rewrite: crate::token::AccessTokenRewrite,
) -> u8 {
    match rewrite {
        crate::token::AccessTokenRewrite::Passthrough => {
            call_original(target, object, opcode, data, size)
        }
        crate::token::AccessTokenRewrite::Rewritten {
            mut packet,
            patched_apps,
        } => {
            let Ok(rewritten_size) = u32::try_from(packet.len()) else {
                return call_original(target, object, opcode, data, size);
            };
            crate::token::record_access_token_patch(patched_apps);
            call_original(target, object, opcode, packet.as_mut_ptr(), rewritten_size)
        }
    }
}

#[cfg(not(feature = "download-token"))]
unsafe fn call_rewritten_or_original(
    target: *mut c_void,
    object: *mut c_void,
    opcode: u8,
    data: *mut u8,
    size: u32,
    _rewrite: (),
) -> u8 {
    call_original(target, object, opcode, data, size)
}

unsafe fn call_original(
    target: *mut c_void,
    object: *mut c_void,
    opcode: u8,
    data: *mut u8,
    size: u32,
) -> u8 {
    call_original_while_unhooked(|| {
        let original: BuildAndSendFrameFn = std::mem::transmute(target);
        original(object, opcode, data, size)
    })
    .unwrap_or(0)
}

/// # Safety
/// `call` 只能调用 TARGET 指向的原入口; 调用期间入口补丁已卸下.
unsafe fn call_original_while_unhooked(call: impl FnOnce() -> u8) -> Option<u8> {
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
    }
    Some(result)
}
