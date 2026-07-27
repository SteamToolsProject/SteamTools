//! Manifest override 的受限 hook 与只读运行时快照.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

use stt_core::{DepotId, ManifestOverride};
use stt_hook::InlineHook;
use stt_metadata::{ByteSig, PatternStore};
use stt_platform::{module_info, module_path_by_name, sha256_file};

use crate::{DownloadCapability, DownloadCapabilityStatus, DownloadKitReport};

const VERIFIED_STEAMCLIENT_SHA256: &str =
    "61dd80e84a1c5ddab034f5253436835dead86b194e214bd99a6bd644a07dde7d";
const SYMBOL: &str = "BuildDepotDependency";
const MAX_DEPOT_ENTRIES: usize = 4096;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DepotEntry {
    depot_id: u32,
    app_id: u32,
    manifest_gid: u64,
    manifest_size: u64,
    dlc_app_id: u32,
    lcs_required: u8,
    not_new_target: u8,
    shared_install: u8,
    padding: u8,
}

#[repr(C)]
struct CUtlVector<T> {
    memory: *mut T,
    allocation_count: i32,
    grow_size: i32,
    size: i32,
}

const _: () = {
    assert!(std::mem::size_of::<DepotEntry>() == 0x20);
    assert!(std::mem::offset_of!(DepotEntry, depot_id) == 0x00);
    assert!(std::mem::offset_of!(DepotEntry, app_id) == 0x04);
    assert!(std::mem::offset_of!(DepotEntry, manifest_gid) == 0x08);
    assert!(std::mem::offset_of!(DepotEntry, manifest_size) == 0x10);
    assert!(std::mem::offset_of!(DepotEntry, dlc_app_id) == 0x18);
    assert!(std::mem::size_of::<CUtlVector<DepotEntry>>() == 0x18);
    assert!(std::mem::offset_of!(CUtlVector<DepotEntry>, memory) == 0x00);
    assert!(std::mem::offset_of!(CUtlVector<DepotEntry>, size) == 0x10);
};

type BuildDepotDependencyFn = unsafe extern "C" fn(
    *mut c_void,
    u32,
    *mut c_void,
    *mut CUtlVector<DepotEntry>,
    *mut CUtlVector<DepotEntry>,
    *mut c_void,
    *mut u32,
    *mut u8,
) -> u8;

static TARGET: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ATTACHED: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicU64 = AtomicU64::new(0);
static PATCHED: AtomicU64 = AtomicU64::new(0);
static HOOK: Mutex<Option<InlineHook>> = Mutex::new(None);
static OVERRIDES: OnceLock<RwLock<HashMap<DepotId, ManifestOverride>>> = OnceLock::new();

fn overrides() -> &'static RwLock<HashMap<DepotId, ManifestOverride>> {
    OVERRIDES.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 用 host 生成的新快照替换 hook 侧数据.
pub fn replace_manifest_overrides(values: HashMap<DepotId, ManifestOverride>) {
    let mut guard = overrides()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = values;
}

pub fn is_manifest_hook_attached() -> bool {
    ATTACHED.load(Ordering::SeqCst)
}

pub fn manifest_hook_stats() -> (u64, u64) {
    (
        CALLS.load(Ordering::Relaxed),
        PATCHED.load(Ordering::Relaxed),
    )
}

/// planner 的 manifest 项通过全部门禁后, 再验证当前 DLL 并尝试 attach.
pub fn try_install_manifest_hook(report: &mut DownloadKitReport, patterns: &PatternStore) {
    let Some(capability) = report
        .capabilities
        .iter_mut()
        .find(|item| item.capability == DownloadCapability::ManifestOverride)
    else {
        return;
    };

    if is_manifest_hook_attached() {
        capability.status = DownloadCapabilityStatus::HooksAttached;
        capability.detail = Some("manifest hook 已挂上".to_owned());
        return;
    }
    if capability.status != DownloadCapabilityStatus::LogicOnly {
        return;
    }

    let Some(info) = module_info("steamclient64.dll") else {
        capability.detail = Some("steamclient64.dll 尚未加载".to_owned());
        return;
    };
    let Some(module_path) = module_path_by_name("steamclient64.dll") else {
        capability.detail = Some("无法读取 steamclient64.dll 路径".to_owned());
        return;
    };
    let Ok(module_sha) = sha256_file(&module_path) else {
        capability.detail = Some("无法校验 steamclient64.dll SHA-256".to_owned());
        return;
    };
    if module_sha != VERIFIED_STEAMCLIENT_SHA256 {
        capability.detail = Some("当前 steamclient64.dll 布局尚未验证".to_owned());
        return;
    }

    let Some(entry) = patterns
        .map("steamclient")
        .and_then(|map| map.get_by_name(SYMBOL))
    else {
        capability.status = DownloadCapabilityStatus::SymbolsMissing;
        capability.missing = vec![SYMBOL.to_owned()];
        return;
    };
    let Some(rva) = entry.rva.and_then(|value| usize::try_from(value).ok()) else {
        capability.detail = Some("BuildDepotDependency 缺少有效 RVA".to_owned());
        return;
    };
    let Some(signature) = entry.sig.as_deref() else {
        capability.detail = Some("BuildDepotDependency 缺少入口签名".to_owned());
        return;
    };
    let Ok(signature) = ByteSig::parse(signature) else {
        capability.detail = Some("BuildDepotDependency 入口签名无效".to_owned());
        return;
    };
    let Some(end) = rva.checked_add(signature.bytes.len()) else {
        capability.detail = Some("BuildDepotDependency RVA 越界".to_owned());
        return;
    };
    if end > info.size {
        capability.detail = Some("BuildDepotDependency RVA 越界".to_owned());
        return;
    }

    // # Safety
    // module_info 已确认映像范围, rva/end 也已在该范围内.
    let prologue = unsafe {
        std::slice::from_raw_parts((info.base as *const u8).add(rva), signature.bytes.len())
    };
    if signature.find_in(prologue) != Some(0) {
        capability.detail = Some("BuildDepotDependency 入口签名不匹配".to_owned());
        return;
    }

    // # Safety
    // 当前 DLL SHA, RVA 和入口签名均已验证, detour ABI 与勘察布局一致.
    let target = unsafe { (info.base as *mut u8).add(rva).cast::<c_void>() };
    let mut slot = HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(hook) = slot.as_mut() {
        if !hook.is_installed() {
            if let Err(error) = unsafe { hook.attach() } {
                capability.detail = Some(format!("manifest hook reattach 失败: {error}"));
                return;
            }
        }
        ATTACHED.store(true, Ordering::SeqCst);
        capability.status = DownloadCapabilityStatus::HooksAttached;
        capability.detail = Some("manifest hook 已挂上".to_owned());
        return;
    }
    let mut hook =
        match unsafe { InlineHook::new(target, hk_build_depot_dependency as *const c_void) } {
            Ok(hook) => hook,
            Err(error) => {
                capability.detail = Some(format!("manifest hook 初始化失败: {error}"));
                return;
            }
        };
    TARGET.store(target, Ordering::SeqCst);
    if let Err(error) = unsafe { hook.attach() } {
        TARGET.store(std::ptr::null_mut(), Ordering::SeqCst);
        capability.detail = Some(format!("manifest hook attach 失败: {error}"));
        return;
    }
    *slot = Some(hook);
    ATTACHED.store(true, Ordering::SeqCst);
    capability.status = DownloadCapabilityStatus::HooksAttached;
    capability.detail = Some("manifest hook 已挂上".to_owned());
}

/// # Safety
/// 由已验证 ABI 的 `BuildDepotDependency` 入口调用; 所有指针均沿用原函数契约.
unsafe extern "C" fn hk_build_depot_dependency(
    user_app_mgr: *mut c_void,
    app_id: u32,
    user_config: *mut c_void,
    depot_info: *mut CUtlVector<DepotEntry>,
    shared_depot_info: *mut CUtlVector<DepotEntry>,
    steam_app: *mut c_void,
    build_id: *mut u32,
    beta_fallback: *mut u8,
) -> u8 {
    CALLS.fetch_add(1, Ordering::Relaxed);
    let target = TARGET.load(Ordering::SeqCst);
    if target.is_null() {
        return 0;
    }
    let result = call_original_while_unhooked(|| {
        let original: BuildDepotDependencyFn = std::mem::transmute(target);
        original(
            user_app_mgr,
            app_id,
            user_config,
            depot_info,
            shared_depot_info,
            steam_app,
            build_id,
            beta_fallback,
        )
    });
    let Some(result) = result else {
        return 0;
    };
    if result == 0 {
        return result;
    }

    let patched = patch_primary_vector(depot_info);
    PATCHED.fetch_add(patched as u64, Ordering::Relaxed);
    result
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

/// # Safety
/// `vector` 来自成功返回的原函数, 指向当前 SHA 对应的 CUtlVector 布局.
unsafe fn patch_primary_vector(vector: *mut CUtlVector<DepotEntry>) -> usize {
    let Some(vector) = vector.as_mut() else {
        return 0;
    };
    let Ok(size) = usize::try_from(vector.size) else {
        return 0;
    };
    if size == 0 || size > MAX_DEPOT_ENTRIES || vector.memory.is_null() {
        return 0;
    }
    let entries = std::slice::from_raw_parts_mut(vector.memory, size);
    let guard = overrides()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    apply_overrides(entries, &guard)
}

fn apply_overrides(
    entries: &mut [DepotEntry],
    values: &HashMap<DepotId, ManifestOverride>,
) -> usize {
    let mut patched = 0;
    for entry in entries {
        let Some(over) = values.get(&entry.depot_id) else {
            continue;
        };
        entry.manifest_gid = over.manifest_gid;
        if over.size != 0 {
            entry.manifest_size = over.size;
        }
        patched += 1;
    }
    patched
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(depot_id: u32, gid: u64, size: u64) -> DepotEntry {
        DepotEntry {
            depot_id,
            manifest_gid: gid,
            manifest_size: size,
            ..DepotEntry::default()
        }
    }

    #[test]
    fn matching_depot_replaces_gid_and_nonzero_size() {
        let mut entries = [entry(10, 20, 30)];
        let values = HashMap::from([(
            10,
            ManifestOverride {
                manifest_gid: 40,
                size: 50,
            },
        )]);

        let patched = apply_overrides(&mut entries, &values);

        assert_eq!(patched, 1);
        assert_eq!(entries[0].manifest_gid, 40);
        assert_eq!(entries[0].manifest_size, 50);
    }

    #[test]
    fn zero_override_size_keeps_original_size() {
        let mut entries = [entry(10, 20, 30)];
        let values = HashMap::from([(
            10,
            ManifestOverride {
                manifest_gid: 40,
                size: 0,
            },
        )]);

        apply_overrides(&mut entries, &values);

        assert_eq!(entries[0].manifest_gid, 40);
        assert_eq!(entries[0].manifest_size, 30);
    }

    #[test]
    fn unmatched_depot_is_unchanged() {
        let original = entry(10, 20, 30);
        let mut entries = [original];
        let values = HashMap::from([(
            11,
            ManifestOverride {
                manifest_gid: 40,
                size: 50,
            },
        )]);

        let patched = apply_overrides(&mut entries, &values);

        assert_eq!(patched, 0);
        assert_eq!(entries[0], original);
    }
}
