//! 语义化版本 `vX.Y.Z` 的解析与比较.

use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// 解析 `X.Y.Z` 或 `vX.Y.Z`; 其它格式返回 None.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.strip_prefix('v').unwrap_or(text);
        let mut parts = text.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }

    /// 是否严格大于另一个版本.
    pub fn is_newer_than(self, other: Self) -> bool {
        self.cmp(&other) == Ordering::Greater
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_with_and_without_v_prefix() {
        assert_eq!(
            Version::parse("v0.2.0"),
            Some(Version {
                major: 0,
                minor: 2,
                patch: 0
            })
        );
        assert_eq!(
            Version::parse("0.2.0"),
            Some(Version {
                major: 0,
                minor: 2,
                patch: 0
            })
        );
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(Version::parse(""), None);
        assert_eq!(Version::parse("v1"), None);
        assert_eq!(Version::parse("v1.2"), None);
        assert_eq!(Version::parse("v1.2.3.4"), None);
        assert_eq!(Version::parse("v1.2.x"), None);
        assert_eq!(Version::parse("1.2.3-pre"), None);
    }

    #[test]
    fn orders_by_major_then_minor_then_patch() {
        assert!(Version::parse("v0.2.0")
            .unwrap()
            .is_newer_than(Version::parse("v0.1.9").unwrap()));
        assert!(Version::parse("v1.0.0")
            .unwrap()
            .is_newer_than(Version::parse("v0.9.9").unwrap()));
        assert!(!Version::parse("v0.1.0")
            .unwrap()
            .is_newer_than(Version::parse("v0.1.0").unwrap()));
        assert!(!Version::parse("v0.0.9")
            .unwrap()
            .is_newer_than(Version::parse("v0.1.0").unwrap()));
    }
}
