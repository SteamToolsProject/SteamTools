//! 系统 CSPRNG.
//!
//! 用于会话内的不可猜凭据 (如 click_bridge token). 不要用时间戳/进程号/地址
//! 之类凑随机: 那些对攻击者是可预测或可枚举的.

use windows::Win32::Security::Cryptography::{
    BCryptGenRandom, BCRYPT_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};

/// 用系统 CSPRNG 填满 `buf`; 失败返回 `false`.
///
/// 失败时 `buf` 内容不可信, 调用方必须**放弃**而不是降级用弱随机.
pub fn random_bytes(buf: &mut [u8]) -> bool {
    // SAFETY: buf 是合法可写切片; 传 null 算法句柄配合
    // BCRYPT_USE_SYSTEM_PREFERRED_RNG 是这个 API 的既定用法.
    let status = unsafe {
        BCryptGenRandom(
            BCRYPT_ALG_HANDLE::default(),
            buf,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    status.is_ok()
}

/// 生成 `bytes` 字节的随机值并转成小写十六进制; 取不到随机数就返回 `None`.
pub fn random_hex_token(bytes: usize) -> Option<String> {
    let mut raw = vec![0u8; bytes];
    if !random_bytes(&mut raw) {
        return None;
    }
    Some(to_hex(&raw))
}

/// 小写十六进制, 每字节定长两位.
///
/// 定长很关键: 前导零被吃掉会让 token 变短, 熵也就跟着少了.
fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // write! 直接写进 out, 不像 format! 每字节再分配一个 String.
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// 定长比较, 不因首个不同字节的位置提前返回.
///
/// 本机 HTTP 上的计时侧信道很难利用, 但常数时间比较几乎不要钱, 没理由不做.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_has_two_hex_chars_per_byte() {
        let t = random_hex_token(16).expect("system rng");
        assert_eq!(t.len(), 32);
    }

    /// 前导零若被吃掉, token 会变短、熵会变少 — 这里用定值把它钉死.
    #[test]
    fn hex_keeps_leading_zeros() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xff]), "000fff");
    }

    #[test]
    fn hex_of_nothing_is_empty() {
        assert_eq!(to_hex(&[]), "");
    }

    #[test]
    fn token_is_all_hex() {
        let t = random_hex_token(16).expect("system rng");
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn two_tokens_differ() {
        let a = random_hex_token(16).expect("system rng");
        let b = random_hex_token(16).expect("system rng");
        assert_ne!(a, b);
    }

    #[test]
    fn equal_strings_compare_equal() {
        assert!(constant_time_eq("abc123", "abc123"));
    }

    #[test]
    fn different_strings_compare_unequal() {
        assert!(!constant_time_eq("abc123", "abc124"));
    }

    #[test]
    fn different_lengths_compare_unequal() {
        assert!(!constant_time_eq("abc", "abcd"));
    }

    #[test]
    fn empty_token_never_matches_a_real_one() {
        assert!(!constant_time_eq("", "abc"));
    }
}
