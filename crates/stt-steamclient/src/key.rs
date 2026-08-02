//! Depot key 的路径解析, 只读快照与 hook.
//!
//! 对齐 OST `Hooks_Decryption.cpp`: ConfigStoreGetBinary 上拦截
//! `...\ <DepotId>\DecryptionKey` (不按 EConfigStore 过滤).

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
const KEY_SIZE: usize = 32;
const MAX_KEY_NAME: usize = 1024;
/// OST 用 substring find(`\\DecryptionKey`); 我们也按此定位, 并额外接受 `/`.
const DECRYPTION_KEY_MARK_BACK: &[u8] = b"\\DecryptionKey";
const DECRYPTION_KEY_MARK_FWD: &[u8] = b"/DecryptionKey";

type ConfigStoreGetBinaryFn =
    unsafe extern "C" fn(*mut c_void, i32, *const c_char, *mut u8, u32) -> i32;

static TARGET: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ATTACHED: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicU64 = AtomicU64::new(0);
static SERVED: AtomicU64 = AtomicU64::new(0);
/// 路径命中 DecryptionKey 但快照无此 depot (或 buffer 太小).
static PATH_HIT_MISS: AtomicU64 = AtomicU64::new(0);
/// 当前 hook 侧快照条目数 (replace 时更新).
static SNAPSHOT_LEN: AtomicU64 = AtomicU64::new(0);
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
    SNAPSHOT_LEN.store(decoded.len() as u64, Ordering::Relaxed);
    let mut guard = keys()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = decoded;
    report
}

pub fn is_depot_key_hook_attached() -> bool {
    ATTACHED.load(Ordering::SeqCst)
}

/// `(calls, served, path_hit_miss, snapshot_len)`.
pub fn depot_key_hook_stats() -> (u64, u64, u64, u64) {
    (
        CALLS.load(Ordering::Relaxed),
        SERVED.load(Ordering::Relaxed),
        PATH_HIT_MISS.load(Ordering::Relaxed),
        SNAPSHOT_LEN.load(Ordering::Relaxed),
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
///
/// 对齐 OST: **不按 EConfigStore 过滤**. 路径命中 `\\DecryptionKey` (或 `/`)
/// 且快照有该 depot、buffer ≥ 32 时直接回 32 字节 key.
unsafe extern "C" fn hk_config_store_get_binary(
    object: *mut c_void,
    store: i32,
    key_name: *const c_char,
    output: *mut u8,
    output_size: u32,
) -> i32 {
    CALLS.fetch_add(1, Ordering::Relaxed);
    if let Some(depot_id) = parse_depot_id_from_ptr(key_name) {
        if !output.is_null() && output_size >= KEY_SIZE as u32 {
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
        // 路径是 DecryptionKey, 但没吐出 (无快照 / buffer 小 / null out).
        PATH_HIT_MISS.fetch_add(1, Ordering::Relaxed);
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

/// OST 语义: `find("\\DecryptionKey")`, 再向前找分隔符取 depot id.
/// 额外接受 `/DecryptionKey`.
fn parse_depot_key_path(path: &[u8]) -> Option<DepotId> {
    let (mark, sep) = if let Some(pos) = find_bytes(path, DECRYPTION_KEY_MARK_BACK) {
        (pos, b'\\')
    } else if let Some(pos) = find_bytes(path, DECRYPTION_KEY_MARK_FWD) {
        (pos, b'/')
    } else {
        return None;
    };
    if mark == 0 {
        return None;
    }
    let before = &path[..mark];
    let separator = before.iter().rposition(|&byte| byte == sep)?;
    let depot = &before[separator + 1..];
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

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=(haystack.len() - needle.len())).find(|&i| &haystack[i..i + needle.len()] == needle)
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
        assert_eq!(
            parse_depot_key_path(b"Software\\Valve\\Steam\\Depots\\2453061\\DecryptionKey"),
            Some(2_453_061)
        );
    }

    #[test]
    fn forward_slash_path_is_accepted() {
        assert_eq!(
            parse_depot_key_path(b"depots/228980/DecryptionKey"),
            Some(228_980)
        );
    }

    #[test]
    fn ost_find_allows_trailing_noise_after_mark() {
        assert_eq!(
            parse_depot_key_path(b"depots\\228980\\DecryptionKey\\extra"),
            Some(228_980)
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
        assert_eq!(SNAPSHOT_LEN.load(Ordering::Relaxed), 1);
    }
}
