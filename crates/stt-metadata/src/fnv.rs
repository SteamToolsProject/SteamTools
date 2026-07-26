//! FNV-1a 32 位 (与上游 OpenSteamTool 一致).

pub fn fnv1a32(data: &[u8]) -> u32 {
    let mut h = 0x811c9dc5_u32;
    for &b in data {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x01000193);
    }
    h
}

pub fn fnv1a32_str(s: &str) -> u32 {
    fnv1a32(s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_upstream_sample() {
        assert_eq!(fnv1a32_str("BBuildAndAsyncSendFrame"), 0x8242_8E37);
    }
}
