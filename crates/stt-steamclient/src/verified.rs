//! 当前 steamclient 样本的符号验证.

use std::ffi::c_void;

use stt_metadata::{ByteSig, PatternStore};
use stt_platform::module_info;

#[derive(Debug, thiserror::Error)]
pub(crate) enum VerifiedSymbolError {
    #[error("steamclient64.dll 尚未加载")]
    ModuleNotLoaded,
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
    // pattern 已按当前 DLL SHA 加载 (pattern/steamclient/{sha}.toml); 入口签名
    // 比对是最终防线. 不再硬编码单一 SHA: Steam 更新后 pattern 文件会换,
    // 硬编码会让 download_kit 永久 LogicOnly.

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
    // module 已加载, RVA 在映像内, 入口签名已比对通过.
    Ok(unsafe { (info.base as *mut u8).add(rva).cast::<c_void>() })
}
