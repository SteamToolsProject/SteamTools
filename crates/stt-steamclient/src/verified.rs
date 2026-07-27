//! 当前 steamclient 样本的符号验证.

use std::ffi::c_void;

use stt_metadata::{ByteSig, PatternStore};
use stt_platform::{module_info, module_path_by_name, sha256_file};

const VERIFIED_STEAMCLIENT_SHA256: &str =
    "61dd80e84a1c5ddab034f5253436835dead86b194e214bd99a6bd644a07dde7d";

#[derive(Debug, thiserror::Error)]
pub(crate) enum VerifiedSymbolError {
    #[error("steamclient64.dll 尚未加载")]
    ModuleNotLoaded,
    #[error("无法读取 steamclient64.dll 路径")]
    ModulePathMissing,
    #[error("无法校验 steamclient64.dll SHA-256")]
    ModuleHashUnavailable,
    #[error("当前 steamclient64.dll 布局尚未验证")]
    LayoutUnverified,
    #[error("{0} pattern 不存在")]
    PatternMissing(&'static str),
    #[error("{0} 缺少有效 RVA")]
    RvaMissing(&'static str),
    #[error("{0} 缺少入口签名")]
    SignatureMissing(&'static str),
    #[error("{0} 入口签名无效")]
    SignatureInvalid(&'static str),
    #[error("{0} RVA 越界")]
    RvaOutOfBounds(&'static str),
    #[error("{0} 入口签名不匹配")]
    SignatureMismatch(&'static str),
}

pub(crate) fn resolve_verified_symbol(
    patterns: &PatternStore,
    symbol: &'static str,
) -> Result<*mut c_void, VerifiedSymbolError> {
    let info = module_info("steamclient64.dll").ok_or(VerifiedSymbolError::ModuleNotLoaded)?;
    let module_path =
        module_path_by_name("steamclient64.dll").ok_or(VerifiedSymbolError::ModulePathMissing)?;
    let module_sha =
        sha256_file(&module_path).map_err(|_| VerifiedSymbolError::ModuleHashUnavailable)?;
    if module_sha != VERIFIED_STEAMCLIENT_SHA256 {
        return Err(VerifiedSymbolError::LayoutUnverified);
    }

    let entry = patterns
        .map("steamclient")
        .and_then(|map| map.get_by_name(symbol))
        .ok_or(VerifiedSymbolError::PatternMissing(symbol))?;
    let rva = entry
        .rva
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(VerifiedSymbolError::RvaMissing(symbol))?;
    let signature = entry
        .sig
        .as_deref()
        .ok_or(VerifiedSymbolError::SignatureMissing(symbol))?;
    let signature =
        ByteSig::parse(signature).map_err(|_| VerifiedSymbolError::SignatureInvalid(symbol))?;
    let end = rva
        .checked_add(signature.bytes.len())
        .ok_or(VerifiedSymbolError::RvaOutOfBounds(symbol))?;
    if end > info.size {
        return Err(VerifiedSymbolError::RvaOutOfBounds(symbol));
    }

    // # Safety
    // module_info 已确认映像范围, rva/end 也已在该范围内.
    let prologue = unsafe {
        std::slice::from_raw_parts((info.base as *const u8).add(rva), signature.bytes.len())
    };
    if signature.find_in(prologue) != Some(0) {
        return Err(VerifiedSymbolError::SignatureMismatch(symbol));
    }

    // # Safety
    // 当前 DLL SHA, RVA 和入口签名均已验证.
    Ok(unsafe { (info.base as *mut u8).add(rva).cast::<c_void>() })
}
