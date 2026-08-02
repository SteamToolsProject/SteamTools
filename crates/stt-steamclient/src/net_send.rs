//! 多个 NetPacket consumer 共用的发送入口.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::Mutex;

use stt_hook::InlineHook;
use stt_metadata::PatternStore;

#[cfg(any(feature = "download-request-code", feature = "download-token"))]
use crate::net_recv::MAX_PACKET_SIZE;
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

#[cfg(feature = "download-request-code")]
pub(crate) fn deactivate_consumer(capability: DownloadCapability) {
    consumer_active(capability).store(false, Ordering::SeqCst);
}

/// 共享 send hook 的安装决策 (纯逻辑, 可测).
///
/// token 与 request-code 共用 `BBuildAndAsyncSendFrame`. 先挂的 consumer 会 patch
/// 入口 prologue; 后挂的若再 `resolve_verified_symbol` 必然 SignatureMismatch.
/// 所以: **已有 hook 时只激活 consumer 标志, 不再验入口签名**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SharedSendInstallAction {
    AlreadyActive,
    SkipNotLogicOnly,
    /// HOOK 已存在 (可能已 patch 入口): 只打开本 consumer.
    ActivateExisting,
    /// 首次安装: 需要 resolve + InlineHook.
    InstallFresh,
}

pub(crate) fn decide_shared_send_action(
    already_active: bool,
    status: DownloadCapabilityStatus,
    hook_slot_occupied: bool,
) -> SharedSendInstallAction {
    if already_active {
        return SharedSendInstallAction::AlreadyActive;
    }
    if status != DownloadCapabilityStatus::LogicOnly {
        return SharedSendInstallAction::SkipNotLogicOnly;
    }
    if hook_slot_occupied {
        SharedSendInstallAction::ActivateExisting
    } else {
        SharedSendInstallAction::InstallFresh
    }
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

    let already_active = is_consumer_attached(capability_kind);
    // 在 resolve 之前先看 HOOK 是否已被另一 consumer 装上.
    // token 先 attach 后入口 prologue 已是 detour, 再验签名必然失败.
    let hook_occupied = {
        let slot = HOOK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.is_some()
    };
    match decide_shared_send_action(already_active, capability.status, hook_occupied) {
        SharedSendInstallAction::AlreadyActive => {
            capability.status = DownloadCapabilityStatus::HooksAttached;
            capability.detail = Some(format!("{label} hook 已挂上"));
            return;
        }
        SharedSendInstallAction::SkipNotLogicOnly => return,
        SharedSendInstallAction::ActivateExisting => {
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
            } else {
                // 竞态: 决策时 occupied, 锁后空了 — 本轮不硬装, 等下一轮 rearm.
                capability.detail = Some(format!("{label} shared hook vanished; wait rearm"));
                return;
            }
            ATTACHED.store(true, Ordering::SeqCst);
            consumer_active(capability_kind).store(true, Ordering::SeqCst);
            capability.status = DownloadCapabilityStatus::HooksAttached;
            capability.detail = Some(format!("{label} hook 已挂上 (shared send)"));
            return;
        }
        SharedSendInstallAction::InstallFresh => {}
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
    // 锁内再确认: 另一线程可能刚装完.
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
        capability.detail = Some(format!("{label} hook 已挂上 (shared send)"));
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
    if REQUEST_CODE_ACTIVE.load(Ordering::SeqCst)
        && !data.is_null()
        && size as usize <= MAX_PACKET_SIZE
    {
        let packet = std::slice::from_raw_parts(data.cast_const(), size as usize);
        crate::request_code::submit_manifest_code_frame(u32::from(opcode), packet);
    }

    #[cfg(feature = "download-token")]
    let rewrite = if TOKEN_ACTIVE.load(Ordering::SeqCst) {
        crate::token::record_access_token_call();
        if data.is_null() || size as usize > MAX_PACKET_SIZE {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_consumer_activates_existing_shared_hook_without_resignature() {
        // token 先挂后 HOOK occupied; request-code 再装只应 ActivateExisting,
        // 绝不能再走 InstallFresh (否则会 resolve 已 patch 的 prologue 失败).
        assert_eq!(
            decide_shared_send_action(false, DownloadCapabilityStatus::LogicOnly, true),
            SharedSendInstallAction::ActivateExisting
        );
    }

    #[test]
    fn first_consumer_installs_fresh_when_hook_empty() {
        assert_eq!(
            decide_shared_send_action(false, DownloadCapabilityStatus::LogicOnly, false),
            SharedSendInstallAction::InstallFresh
        );
    }

    #[test]
    fn already_active_is_noop() {
        assert_eq!(
            decide_shared_send_action(true, DownloadCapabilityStatus::LogicOnly, true),
            SharedSendInstallAction::AlreadyActive
        );
    }

    #[test]
    fn non_logic_only_is_skipped_even_if_hook_exists() {
        assert_eq!(
            decide_shared_send_action(false, DownloadCapabilityStatus::DataMissing, true),
            SharedSendInstallAction::SkipNotLogicOnly
        );
        assert_eq!(
            decide_shared_send_action(false, DownloadCapabilityStatus::ToolDisabled, false),
            SharedSendInstallAction::SkipNotLogicOnly
        );
    }
}
