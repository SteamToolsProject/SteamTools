//! Depot key 的严格路径解析, 只读快照与受限 hook.

use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

use stt_core::DepotId;
use stt_hook::InlineHook;
use stt_metadata::PatternStore;

use crate::verified::resolve_verified_symbol;
use crate::{DownloadCapability, DownloadCapabilityStatus, DownloadKitReport};

const SYMBOL: &str = "ConfigStoreGetBinary";
const CONFIG_STORE_USER_LOCAL: i32 = 3;
const KEY_SIZE: usize = 32;
const MAX_KEY_NAME: usize = 1024;
const DECRYPTION_KEY_SUFFIX: &[u8] = b"\\DecryptionKey";

type ConfigStoreGetBinaryFn =
    unsafe extern "C" fn(*mut c_void, i32, *const c_char, *mut u8, u32) -> i32;

static TARGET: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ATTACHED: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicU64 = AtomicU64::new(0);
static SERVED: AtomicU64 = AtomicU64::new(0);
static HOOK: Mutex<Option<InlineHook>> = Mutex::new(None);
static KEYS: OnceLock<RwLock<HashMap<DepotId, [u8; KEY_SIZE]>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DepotKeySnapshotReport {
    pub accepted: usize,
    pub rejected: usize,
}

fn keys() -> &'static RwLock<HashMap<DepotId, [u8; KEY_SIZE]>> {
    KEYS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 替换 hook 侧快照, 并在离开 host 线程前完成 hex 解码.
pub fn replace_depot_keys(values: HashMap<DepotId, String>) -> DepotKeySnapshotReport {
    let mut decoded = HashMap::with_capacity(values.len());
    let mut report = DepotKeySnapshotReport::default();
    for (depot_id, value) in values {
        match decode_key(&value) {
            Some(key) if depot_id != 0 => {
                decoded.insert(depot_id, key);
                report.accepted += 1;
            }
            _ => report.rejected += 1,
        }
    }
    let mut guard = keys()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = decoded;
    report
}

pub fn is_depot_key_hook_attached() -> bool {
    ATTACHED.load(Ordering::SeqCst)
}

pub fn depot_key_hook_stats() -> (u64, u64) {
    (
        CALLS.load(Ordering::Relaxed),
        SERVED.load(Ordering::Relaxed),
    )
}

/// planner 的 key 项通过全部门禁后, 再验证当前 DLL 并尝试 attach.
pub fn try_install_depot_key_hook(report: &mut DownloadKitReport, patterns: &PatternStore) {
    let Some(capability) = report
        .capabilities
        .iter_mut()
        .find(|item| item.capability == DownloadCapability::DepotKey)
    else {
        return;
    };

    if is_depot_key_hook_attached() {
        capability.status = DownloadCapabilityStatus::HooksAttached;
        capability.detail = Some("depot key hook 已挂上".to_owned());
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
                capability.detail = Some(format!("depot key hook reattach 失败: {error}"));
                return;
            }
        }
        ATTACHED.store(true, Ordering::SeqCst);
        capability.status = DownloadCapabilityStatus::HooksAttached;
        capability.detail = Some("depot key hook 已挂上".to_owned());
        return;
    }

    // # Safety
    // 当前 DLL SHA, RVA 和入口签名已由 resolve_verified_symbol 验证.
    let mut hook =
        match unsafe { InlineHook::new(target, hk_config_store_get_binary as *const c_void) } {
            Ok(hook) => hook,
            Err(error) => {
                capability.detail = Some(format!("depot key hook 初始化失败: {error}"));
                return;
            }
        };
    TARGET.store(target, Ordering::SeqCst);
    if let Err(error) = unsafe { hook.attach() } {
        TARGET.store(std::ptr::null_mut(), Ordering::SeqCst);
        capability.detail = Some(format!("depot key hook attach 失败: {error}"));
        return;
    }
    *slot = Some(hook);
    ATTACHED.store(true, Ordering::SeqCst);
    capability.status = DownloadCapabilityStatus::HooksAttached;
    capability.detail = Some("depot key hook 已挂上".to_owned());
}

/// # Safety
/// 由已验证 ABI 的 ConfigStoreGetBinary 入口调用; 指针沿用原函数契约.
unsafe extern "C" fn hk_config_store_get_binary(
    object: *mut c_void,
    store: i32,
    key_name: *const c_char,
    output: *mut u8,
    output_size: u32,
) -> i32 {
    CALLS.fetch_add(1, Ordering::Relaxed);
    if store == CONFIG_STORE_USER_LOCAL && !output.is_null() && output_size >= KEY_SIZE as u32 {
        if let Some(depot_id) = parse_depot_id_from_ptr(key_name) {
            let key = keys()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&depot_id)
                .copied();
            if let Some(key) = key {
                std::ptr::copy_nonoverlapping(key.as_ptr(), output, KEY_SIZE);
                SERVED.fetch_add(1, Ordering::Relaxed);
                return KEY_SIZE as i32;
            }
        }
    }

    let target = TARGET.load(Ordering::SeqCst);
    if target.is_null() {
        return 0;
    }
    call_original_while_unhooked(|| {
        let original: ConfigStoreGetBinaryFn = std::mem::transmute(target);
        original(object, store, key_name, output, output_size)
    })
    .unwrap_or(0)
}

/// # Safety
/// `call` 只能调用 TARGET 指向的原入口; 调用期间入口补丁已卸下.
unsafe fn call_original_while_unhooked(call: impl FnOnce() -> i32) -> Option<i32> {
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

/// # Safety
/// `key_name` 须满足原 ConfigStoreGetBinary 的可读 C 字符串契约.
unsafe fn parse_depot_id_from_ptr(key_name: *const c_char) -> Option<DepotId> {
    if key_name.is_null() {
        return None;
    }
    for len in 0..=MAX_KEY_NAME {
        if *(key_name.add(len) as *const u8) == 0 {
            let bytes = std::slice::from_raw_parts(key_name.cast::<u8>(), len);
            return parse_depot_key_path(bytes);
        }
    }
    None
}

fn parse_depot_key_path(path: &[u8]) -> Option<DepotId> {
    let prefix = path.strip_suffix(DECRYPTION_KEY_SUFFIX)?;
    let separator = prefix.iter().rposition(|&byte| byte == b'\\')?;
    let depot = &prefix[separator + 1..];
    if depot.is_empty() || !depot.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut value = 0u32;
    for &digit in depot {
        value = value
            .checked_mul(10)?
            .checked_add(u32::from(digit - b'0'))?;
    }
    (value != 0).then_some(value)
}

fn decode_key(value: &str) -> Option<[u8; KEY_SIZE]> {
    let bytes = value.as_bytes();
    if bytes.len() != KEY_SIZE * 2 {
        return None;
    }
    let mut key = [0u8; KEY_SIZE];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        key[index] = hex_nibble(pair[0])?
            .checked_mul(16)?
            .checked_add(hex_nibble(pair[1])?)?;
    }
    Some(key)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_decryption_key_path_returns_depot_id() {
        assert_eq!(
            parse_depot_key_path(b"depots\\228980\\DecryptionKey"),
            Some(228_980)
        );
    }

    #[test]
    fn trailing_content_is_rejected() {
        assert_eq!(
            parse_depot_key_path(b"depots\\228980\\DecryptionKey\\extra"),
            None
        );
    }

    #[test]
    fn non_decimal_depot_id_is_rejected() {
        assert_eq!(parse_depot_key_path(b"depots\\22x980\\DecryptionKey"), None);
    }

    #[test]
    fn zero_depot_id_is_rejected() {
        assert_eq!(parse_depot_key_path(b"depots\\0\\DecryptionKey"), None);
    }

    #[test]
    fn key_decoder_accepts_upper_and_lower_hex() {
        let key = decode_key(&"aB".repeat(KEY_SIZE)).unwrap();

        assert_eq!(key, [0xAB; KEY_SIZE]);
    }

    #[test]
    fn key_decoder_rejects_non_hex() {
        assert!(decode_key(&"zz".repeat(KEY_SIZE)).is_none());
    }

    #[test]
    fn snapshot_rejects_bad_keys_without_retaining_text() {
        let report = replace_depot_keys(HashMap::from([
            (1, "ab".repeat(KEY_SIZE)),
            (2, "bad".to_owned()),
        ]));

        assert_eq!(report.accepted, 1);
        assert_eq!(report.rejected, 1);
    }
}
