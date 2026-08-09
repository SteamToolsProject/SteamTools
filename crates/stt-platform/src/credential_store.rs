//! Steam 本机凭证: HKCU 下 per-app 的 AppTicket / ETicket / SteamID.
//!
//! 对齐 OST `OSTPlatform::SteamCredentialStore`:
//! `Software\Valve\Steam\Apps\<appId>` 的 REG_BINARY / REG_SZ.
//! 只读写本机注册表, 不落盘, 不进日志正文.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegGetValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_SET_VALUE, REG_BINARY, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_BINARY,
};

const VALUE_APP_TICKET: &str = "AppTicket";
const VALUE_E_TICKET: &str = "ETicket";
const VALUE_STEAM_ID: &str = "SteamID";

/// 单票大小上限 (防异常 lua hex 撑爆注册表).
const MAX_TICKET_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialStatus {
    Ok,
    NotFound,
    Failed,
}

impl CredentialStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "Ok",
            Self::NotFound => "NotFound",
            Self::Failed => "Failed",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("appId 无效")]
    InvalidAppId,
    #[error("ticket 为空")]
    EmptyTicket,
    #[error("ticket 过大 ({size} B, 上限 {MAX_TICKET_BYTES})")]
    TicketTooLarge { size: usize },
    #[error("hex 无效")]
    InvalidHex,
    #[error("注册表失败: {0}")]
    Registry(String),
}

fn app_key_path(app_id: u32) -> String {
    format!(r"Software\Valve\Steam\Apps\{app_id}")
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn map_win_error(err: WIN32_ERROR) -> CredentialStatus {
    if err == ERROR_FILE_NOT_FOUND || err == ERROR_PATH_NOT_FOUND {
        CredentialStatus::NotFound
    } else if err.0 == 0 {
        CredentialStatus::Ok
    } else {
        CredentialStatus::Failed
    }
}

/// 把偶数位 hex 解成字节; 奇数长度时末尾补 `0` (对齐 OST ParseHexByte 循环).
pub fn decode_ticket_hex(hex: &str) -> Result<Vec<u8>, CredentialError> {
    let hex = hex.trim();
    if hex.is_empty() {
        return Err(CredentialError::EmptyTicket);
    }
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(CredentialError::InvalidHex);
    }
    let mut out = Vec::with_capacity(hex.len().div_ceil(2));
    let bytes = hex.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = bytes[i];
        let lo = if i + 1 < bytes.len() {
            bytes[i + 1]
        } else {
            b'0'
        };
        let nibble = |b: u8| -> u8 {
            match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => 0,
            }
        };
        out.push((nibble(hi) << 4) | nibble(lo));
        i += 2;
    }
    if out.is_empty() {
        return Err(CredentialError::EmptyTicket);
    }
    if out.len() > MAX_TICKET_BYTES {
        return Err(CredentialError::TicketTooLarge { size: out.len() });
    }
    Ok(out)
}

fn write_binary(app_id: u32, value_name: &str, data: &[u8]) -> Result<(), CredentialError> {
    if app_id == 0 {
        return Err(CredentialError::InvalidAppId);
    }
    if data.is_empty() {
        return Err(CredentialError::EmptyTicket);
    }
    if data.len() > MAX_TICKET_BYTES {
        return Err(CredentialError::TicketTooLarge { size: data.len() });
    }

    let path = app_key_path(app_id);
    let path_w = to_wide(&path);
    let name_w = to_wide(value_name);

    // SAFETY: path/name 是 NUL 结尾宽字符; key 用完后 RegCloseKey.
    unsafe {
        let mut key = HKEY::default();
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path_w.as_ptr()),
            0,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        );
        if status != windows::Win32::Foundation::ERROR_SUCCESS {
            return Err(CredentialError::Registry(format!(
                "RegCreateKeyExW {}",
                status.0
            )));
        }
        let set = RegSetValueExW(key, PCWSTR(name_w.as_ptr()), 0, REG_BINARY, Some(data));
        let _ = RegCloseKey(key);
        if set != windows::Win32::Foundation::ERROR_SUCCESS {
            return Err(CredentialError::Registry(format!(
                "RegSetValueExW {}",
                set.0
            )));
        }
    }
    Ok(())
}

fn read_binary(app_id: u32, value_name: &str) -> Result<Vec<u8>, CredentialStatus> {
    if app_id == 0 {
        return Err(CredentialStatus::Failed);
    }
    let path = app_key_path(app_id);
    let path_w = to_wide(&path);
    let name_w = to_wide(value_name);

    // SAFETY: 宽字符 NUL 结尾; 两次 RegGetValueW 取长度再读.
    unsafe {
        let mut bytes: u32 = 0;
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(path_w.as_ptr()),
            PCWSTR(name_w.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            None,
            Some(&mut bytes),
        );
        if status != windows::Win32::Foundation::ERROR_SUCCESS {
            return Err(map_win_error(status));
        }
        if bytes == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; bytes as usize];
        let mut got = bytes;
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(path_w.as_ptr()),
            PCWSTR(name_w.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut got),
        );
        if status != windows::Win32::Foundation::ERROR_SUCCESS {
            return Err(map_win_error(status));
        }
        buf.truncate(got as usize);
        Ok(buf)
    }
}

/// 写 AppOwnershipTicket (`AppTicket` REG_BINARY).
pub fn write_app_ticket(app_id: u32, data: &[u8]) -> Result<(), CredentialError> {
    write_binary(app_id, VALUE_APP_TICKET, data)
}

/// 写 EncryptedAppTicket (`ETicket` REG_BINARY).
pub fn write_eticket(app_id: u32, data: &[u8]) -> Result<(), CredentialError> {
    write_binary(app_id, VALUE_E_TICKET, data)
}

/// 从 hex 写 AppTicket (lua `setAppticket`).
pub fn write_app_ticket_hex(app_id: u32, hex: &str) -> Result<(), CredentialError> {
    let data = decode_ticket_hex(hex)?;
    write_app_ticket(app_id, &data)
}

/// 从 hex 写 ETicket (lua `setEticket` / `setAppEticket`).
pub fn write_eticket_hex(app_id: u32, hex: &str) -> Result<(), CredentialError> {
    let data = decode_ticket_hex(hex)?;
    write_eticket(app_id, &data)
}

pub fn read_app_ticket(app_id: u32) -> Result<Vec<u8>, CredentialStatus> {
    read_binary(app_id, VALUE_APP_TICKET)
}

pub fn read_eticket(app_id: u32) -> Result<Vec<u8>, CredentialStatus> {
    read_binary(app_id, VALUE_E_TICKET)
}

/// 写 per-app SteamID (REG_SZ 十进制).
pub fn write_steam_id(app_id: u32, steam_id: u64) -> Result<(), CredentialError> {
    if app_id == 0 || steam_id == 0 {
        return Err(CredentialError::InvalidAppId);
    }
    let path = app_key_path(app_id);
    let path_w = to_wide(&path);
    let name_w = to_wide(VALUE_STEAM_ID);
    let value = steam_id.to_string();
    let value_w = to_wide(&value);
    let bytes = (value_w.len() * 2) as u32;

    // SAFETY: 宽字符 NUL 结尾; key 关闭.
    unsafe {
        let mut key = HKEY::default();
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path_w.as_ptr()),
            0,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        );
        if status != windows::Win32::Foundation::ERROR_SUCCESS {
            return Err(CredentialError::Registry(format!(
                "RegCreateKeyExW {}",
                status.0
            )));
        }
        // REG_SZ 需要字节视图.
        let raw = std::slice::from_raw_parts(value_w.as_ptr().cast::<u8>(), bytes as usize);
        let set = RegSetValueExW(key, PCWSTR(name_w.as_ptr()), 0, REG_SZ, Some(raw));
        let _ = RegCloseKey(key);
        if set != windows::Win32::Foundation::ERROR_SUCCESS {
            return Err(CredentialError::Registry(format!(
                "RegSetValueExW {}",
                set.0
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_even_and_odd_hex() {
        assert_eq!(decode_ticket_hex("0a0b").unwrap(), vec![0x0a, 0x0b]);
        // 奇数: 末尾补 0 → nibble 'c' + '0'
        assert_eq!(decode_ticket_hex("abc").unwrap(), vec![0xab, 0xc0]);
        assert!(matches!(
            decode_ticket_hex(""),
            Err(CredentialError::EmptyTicket)
        ));
        assert!(matches!(
            decode_ticket_hex("zz"),
            Err(CredentialError::InvalidHex)
        ));
    }

    #[test]
    fn roundtrip_app_ticket_under_test_appid() {
        // 高位测试 id, 避免撞真实游戏.
        const APP: u32 = 4_000_000_001;
        let payload = b"\x01\x02stt-test-ticket".to_vec();
        write_app_ticket(APP, &payload).unwrap();
        let got = read_app_ticket(APP).unwrap();
        assert_eq!(got, payload);

        write_app_ticket_hex(APP, "deadbeef").unwrap();
        assert_eq!(read_app_ticket(APP).unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn roundtrip_eticket_hex() {
        const APP: u32 = 4_000_000_002;
        write_eticket_hex(APP, "cafebabe").unwrap();
        assert_eq!(read_eticket(APP).unwrap(), vec![0xca, 0xfe, 0xba, 0xbe]);
    }
}
