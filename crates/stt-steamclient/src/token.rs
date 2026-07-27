//! PICS access token 的受限 frame 与 protobuf wire 改写.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

use stt_core::AppId;
use stt_hook::InlineHook;
use stt_metadata::PatternStore;

use crate::verified::resolve_verified_symbol;
use crate::wire::{encode_varint, parse_field, WireValue};
use crate::{DownloadCapability, DownloadCapabilityStatus, DownloadKitReport};

const SYMBOL: &str = "BBuildAndAsyncSendFrame";
const BINARY_OPCODE: u32 = 2;
const PROTO_FLAG: u32 = 0x8000_0000;
const PICS_PRODUCT_INFO_REQUEST: u32 = 8903;
const FRAME_HEADER_SIZE: usize = 8;
const MAX_PROTO_HEADER_SIZE: usize = 1024;
const MAX_BODY_SIZE: usize = 65_536;

type BuildAndSendFrameFn = unsafe extern "C" fn(*mut c_void, u8, *mut u8, u32) -> u8;

static TARGET: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ATTACHED: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicU64 = AtomicU64::new(0);
static PATCHED_FRAMES: AtomicU64 = AtomicU64::new(0);
static PATCHED_APPS: AtomicU64 = AtomicU64::new(0);
static HOOK: Mutex<Option<InlineHook>> = Mutex::new(None);
static TOKENS: OnceLock<RwLock<HashMap<AppId, u64>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AccessTokenSnapshotReport {
    pub accepted: usize,
    pub rejected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessTokenRewrite {
    Passthrough,
    Rewritten {
        packet: Vec<u8>,
        patched_apps: usize,
    },
}

fn tokens() -> &'static RwLock<HashMap<AppId, u64>> {
    TOKENS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 替换 hook 侧 token 快照; 零 ID 与零 token 不进入热路径.
pub fn replace_access_tokens(values: HashMap<AppId, u64>) -> AccessTokenSnapshotReport {
    let mut accepted = HashMap::with_capacity(values.len());
    let mut report = AccessTokenSnapshotReport::default();
    for (app_id, token) in values {
        if app_id == 0 || token == 0 {
            report.rejected += 1;
        } else {
            accepted.insert(app_id, token);
            report.accepted += 1;
        }
    }
    let mut guard = tokens()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = accepted;
    report
}

pub fn is_access_token_hook_attached() -> bool {
    ATTACHED.load(Ordering::SeqCst)
}

pub fn access_token_hook_stats() -> (u64, u64, u64) {
    (
        CALLS.load(Ordering::Relaxed),
        PATCHED_FRAMES.load(Ordering::Relaxed),
        PATCHED_APPS.load(Ordering::Relaxed),
    )
}

/// planner 的 token 项通过全部门禁后, 再验证当前 DLL 并尝试 attach.
pub fn try_install_access_token_hook(report: &mut DownloadKitReport, patterns: &PatternStore) {
    let Some(capability) = report
        .capabilities
        .iter_mut()
        .find(|item| item.capability == DownloadCapability::AccessToken)
    else {
        return;
    };

    if is_access_token_hook_attached() {
        capability.status = DownloadCapabilityStatus::HooksAttached;
        capability.detail = Some("access token hook 已挂上".to_owned());
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
                capability.detail = Some(format!("access token hook reattach 失败: {error}"));
                return;
            }
        }
        ATTACHED.store(true, Ordering::SeqCst);
        capability.status = DownloadCapabilityStatus::HooksAttached;
        capability.detail = Some("access token hook 已挂上".to_owned());
        return;
    }

    // # Safety
    // 当前 DLL SHA, RVA 和入口签名已由 resolve_verified_symbol 验证.
    let mut hook =
        match unsafe { InlineHook::new(target, hk_build_and_async_send_frame as *const c_void) } {
            Ok(hook) => hook,
            Err(error) => {
                capability.detail = Some(format!("access token hook 初始化失败: {error}"));
                return;
            }
        };
    TARGET.store(target, Ordering::SeqCst);
    if let Err(error) = unsafe { hook.attach() } {
        TARGET.store(std::ptr::null_mut(), Ordering::SeqCst);
        capability.detail = Some(format!("access token hook attach 失败: {error}"));
        return;
    }
    *slot = Some(hook);
    ATTACHED.store(true, Ordering::SeqCst);
    capability.status = DownloadCapabilityStatus::HooksAttached;
    capability.detail = Some("access token hook 已挂上".to_owned());
}

/// # Safety
/// 由已验证 ABI 的 BBuildAndAsyncSendFrame 入口调用; 指针沿用原函数契约.
unsafe extern "C" fn hk_build_and_async_send_frame(
    object: *mut c_void,
    opcode: u8,
    data: *mut u8,
    size: u32,
) -> u8 {
    CALLS.fetch_add(1, Ordering::Relaxed);
    let target = TARGET.load(Ordering::SeqCst);
    if target.is_null() {
        return 0;
    }

    let rewrite = if data.is_null() {
        AccessTokenRewrite::Passthrough
    } else {
        let packet = std::slice::from_raw_parts(data.cast_const(), size as usize);
        let guard = tokens()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rewrite_access_token_frame(u32::from(opcode), packet, &guard)
    };

    match rewrite {
        AccessTokenRewrite::Passthrough => call_original_while_unhooked(|| {
            let original: BuildAndSendFrameFn = std::mem::transmute(target);
            original(object, opcode, data, size)
        })
        .unwrap_or(0),
        AccessTokenRewrite::Rewritten {
            mut packet,
            patched_apps,
        } => {
            let Ok(rewritten_size) = u32::try_from(packet.len()) else {
                return call_original_while_unhooked(|| {
                    let original: BuildAndSendFrameFn = std::mem::transmute(target);
                    original(object, opcode, data, size)
                })
                .unwrap_or(0);
            };
            PATCHED_FRAMES.fetch_add(1, Ordering::Relaxed);
            PATCHED_APPS.fetch_add(patched_apps as u64, Ordering::Relaxed);
            call_original_while_unhooked(|| {
                let original: BuildAndSendFrameFn = std::mem::transmute(target);
                original(object, opcode, packet.as_mut_ptr(), rewritten_size)
            })
            .unwrap_or(0)
        }
    }
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

/// 只改写二进制 EMsg 8903 frame 中已配置 app 的 access token.
pub fn rewrite_access_token_frame(
    opcode: u32,
    packet: &[u8],
    tokens: &HashMap<AppId, u64>,
) -> AccessTokenRewrite {
    if opcode != BINARY_OPCODE || tokens.is_empty() || packet.len() < FRAME_HEADER_SIZE {
        return AccessTokenRewrite::Passthrough;
    }

    let Some(message) = read_u32_le(packet, 0) else {
        return AccessTokenRewrite::Passthrough;
    };
    if message & PROTO_FLAG == 0 || message & !PROTO_FLAG != PICS_PRODUCT_INFO_REQUEST {
        return AccessTokenRewrite::Passthrough;
    }
    let Some(header_size) = read_u32_le(packet, 4).and_then(|value| usize::try_from(value).ok())
    else {
        return AccessTokenRewrite::Passthrough;
    };
    if header_size > MAX_PROTO_HEADER_SIZE {
        return AccessTokenRewrite::Passthrough;
    }
    let Some(body_offset) = FRAME_HEADER_SIZE.checked_add(header_size) else {
        return AccessTokenRewrite::Passthrough;
    };
    if body_offset > packet.len() || packet.len() - body_offset > MAX_BODY_SIZE {
        return AccessTokenRewrite::Passthrough;
    }

    let Some((body, patched_apps)) = rewrite_request_body(&packet[body_offset..], tokens) else {
        return AccessTokenRewrite::Passthrough;
    };
    if patched_apps == 0 || body.len() > MAX_BODY_SIZE {
        return AccessTokenRewrite::Passthrough;
    }

    let Some(new_size) = body_offset.checked_add(body.len()) else {
        return AccessTokenRewrite::Passthrough;
    };
    let mut rewritten = Vec::with_capacity(new_size);
    rewritten.extend_from_slice(&packet[..body_offset]);
    rewritten.extend_from_slice(&body);
    AccessTokenRewrite::Rewritten {
        packet: rewritten,
        patched_apps,
    }
}

fn rewrite_request_body(body: &[u8], tokens: &HashMap<AppId, u64>) -> Option<(Vec<u8>, usize)> {
    let mut cursor = 0;
    let mut output = Vec::with_capacity(body.len());
    let mut patched_apps = 0;
    while cursor < body.len() {
        let field = parse_field(body, &mut cursor)?;
        let WireValue::Bytes(app) = field.value else {
            output.extend_from_slice(&body[field.start..field.end]);
            continue;
        };
        if field.number != 2 || field.wire_type != 2 {
            output.extend_from_slice(&body[field.start..field.end]);
            continue;
        }

        let Some(rewritten_app) = rewrite_app_info(app, tokens)? else {
            output.extend_from_slice(&body[field.start..field.end]);
            continue;
        };
        output.extend_from_slice(&body[field.start..field.tag_end]);
        encode_varint(rewritten_app.len() as u64, &mut output);
        output.extend_from_slice(&rewritten_app);
        patched_apps += 1;
    }
    Some((output, patched_apps))
}

fn rewrite_app_info(app: &[u8], tokens: &HashMap<AppId, u64>) -> Option<Option<Vec<u8>>> {
    let mut cursor = 0;
    let mut app_id = None;
    let mut current_token = None;
    while cursor < app.len() {
        let field = parse_field(app, &mut cursor)?;
        match (field.number, field.wire_type, field.value) {
            (1, 0, WireValue::Varint(value)) => app_id = u32::try_from(value).ok(),
            (2, 0, WireValue::Varint(value)) => current_token = Some(value),
            _ => {}
        }
    }

    let token = app_id
        .and_then(|app_id| tokens.get(&app_id))
        .copied()
        .filter(|token| *token != 0);
    let Some(token) = token else {
        return Some(None);
    };
    if current_token == Some(token) {
        return Some(None);
    }

    let mut cursor = 0;
    let mut output = Vec::with_capacity(app.len() + 11);
    while cursor < app.len() {
        let field = parse_field(app, &mut cursor)?;
        if field.number != 2 || field.wire_type != 0 {
            output.extend_from_slice(&app[field.start..field.end]);
        }
    }
    encode_varint((2 << 3) as u64, &mut output);
    encode_varint(token, &mut output);
    Some(Some(output))
}

fn read_u32_le(input: &[u8], offset: usize) -> Option<u32> {
    let bytes = input.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field_varint(number: u32, value: u64) -> Vec<u8> {
        let mut output = Vec::new();
        encode_varint(u64::from(number) << 3, &mut output);
        encode_varint(value, &mut output);
        output
    }

    fn field_bytes(number: u32, value: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        encode_varint((u64::from(number) << 3) | 2, &mut output);
        encode_varint(value.len() as u64, &mut output);
        output.extend_from_slice(value);
        output
    }

    fn frame(body: &[u8]) -> Vec<u8> {
        let header = [0xAA, 0xBB, 0xCC];
        let mut packet = Vec::new();
        packet.extend_from_slice(&(PROTO_FLAG | PICS_PRODUCT_INFO_REQUEST).to_le_bytes());
        packet.extend_from_slice(&(header.len() as u32).to_le_bytes());
        packet.extend_from_slice(&header);
        packet.extend_from_slice(body);
        packet
    }

    #[test]
    fn configured_app_token_is_replaced_without_touching_other_fields() {
        let mut app = field_varint(1, 42);
        app.extend(field_bytes(9, b"unknown"));
        app.extend(field_varint(2, 7));
        let package = field_varint(1, 100);
        let mut body = field_bytes(1, &package);
        body.extend(field_varint(8, 99));
        body.extend(field_bytes(2, &app));
        body.extend(field_varint(3, 1));

        let result = rewrite_access_token_frame(2, &frame(&body), &HashMap::from([(42, 123)]));

        let mut expected_app = field_varint(1, 42);
        expected_app.extend(field_bytes(9, b"unknown"));
        expected_app.extend(field_varint(2, 123));
        let mut expected_body = field_bytes(1, &package);
        expected_body.extend(field_varint(8, 99));
        expected_body.extend(field_bytes(2, &expected_app));
        expected_body.extend(field_varint(3, 1));
        assert_eq!(
            result,
            AccessTokenRewrite::Rewritten {
                packet: frame(&expected_body),
                patched_apps: 1,
            }
        );
    }

    #[test]
    fn unrelated_app_is_passthrough() {
        let app = field_varint(1, 42);
        let body = field_bytes(2, &app);

        let result = rewrite_access_token_frame(2, &frame(&body), &HashMap::from([(43, 123)]));

        assert_eq!(result, AccessTokenRewrite::Passthrough);
    }

    #[test]
    fn existing_matching_token_is_passthrough() {
        let mut app = field_varint(1, 42);
        app.extend(field_varint(2, 123));
        let body = field_bytes(2, &app);

        let result = rewrite_access_token_frame(2, &frame(&body), &HashMap::from([(42, 123)]));

        assert_eq!(result, AccessTokenRewrite::Passthrough);
    }

    #[test]
    fn non_binary_frame_is_passthrough() {
        let app = field_varint(1, 42);
        let body = field_bytes(2, &app);

        let result = rewrite_access_token_frame(1, &frame(&body), &HashMap::from([(42, 123)]));

        assert_eq!(result, AccessTokenRewrite::Passthrough);
    }

    #[test]
    fn malformed_nested_length_is_passthrough() {
        let body = [0x12, 0x04, 0x08, 0x2A];

        let result = rewrite_access_token_frame(2, &frame(&body), &HashMap::from([(42, 123)]));

        assert_eq!(result, AccessTokenRewrite::Passthrough);
    }

    #[test]
    fn oversized_body_is_passthrough() {
        let body = vec![0; MAX_BODY_SIZE + 1];

        let result = rewrite_access_token_frame(2, &frame(&body), &HashMap::from([(42, 123)]));

        assert_eq!(result, AccessTokenRewrite::Passthrough);
    }

    #[test]
    fn token_growth_past_body_limit_is_passthrough() {
        let mut app = field_varint(1, 42);
        let filler_size = MAX_BODY_SIZE - app.len() - 8;
        app.extend(field_bytes(9, &vec![0; filler_size]));
        let body = field_bytes(2, &app);
        assert_eq!(body.len(), MAX_BODY_SIZE);

        let result = rewrite_access_token_frame(2, &frame(&body), &HashMap::from([(42, u64::MAX)]));

        assert_eq!(result, AccessTokenRewrite::Passthrough);
    }

    #[test]
    fn overflowing_varint_is_passthrough() {
        let app = [
            0x08, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02,
        ];
        let body = field_bytes(2, &app);

        let result = rewrite_access_token_frame(2, &frame(&body), &HashMap::from([(42, 123)]));

        assert_eq!(result, AccessTokenRewrite::Passthrough);
    }

    #[test]
    fn zero_snapshot_token_is_passthrough() {
        let app = field_varint(1, 42);
        let body = field_bytes(2, &app);

        let result = rewrite_access_token_frame(2, &frame(&body), &HashMap::from([(42, 0)]));

        assert_eq!(result, AccessTokenRewrite::Passthrough);
    }

    #[test]
    fn snapshot_rejects_zero_ids_and_tokens() {
        let report = replace_access_tokens(HashMap::from([(0, 1), (1, 0), (2, 3)]));

        assert_eq!(
            report,
            AccessTokenSnapshotReport {
                accepted: 1,
                rejected: 2,
            }
        );
    }
}
