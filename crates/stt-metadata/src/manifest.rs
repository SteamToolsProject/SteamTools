//! SteamTools-Patterns 发布 manifest.

use std::collections::HashMap;

use serde::Deserialize;

use crate::{MetadataError, Result};

const SUPPORTED_SCHEMA: u32 = 1;
const SHA256_LEN: usize = 64;

/// `SteamTools-Patterns` 的一个 channel manifest.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatternManifest {
    schema: u32,
    channel: String,
    steam_build: String,
    components: HashMap<String, PatternManifestComponent>,
}

/// manifest 中描述一个组件 pattern 的条目.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatternManifestComponent {
    dll: String,
    sha256: String,
    pattern: String,
}

impl PatternManifest {
    /// 解析并校验 `SteamTools-Patterns` 的发布 manifest.
    ///
    /// # Errors
    ///
    /// JSON 格式、schema 或组件路径不符合发布契约时返回错误.
    pub fn parse(body: &[u8]) -> Result<Self> {
        let manifest: Self = serde_json::from_slice(body)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// 返回发布 channel.
    pub fn channel(&self) -> &str {
        &self.channel
    }

    /// 返回生成该 manifest 的 Steam build.
    pub fn steam_build(&self) -> &str {
        &self.steam_build
    }

    /// 按组件名称查找 manifest 条目.
    pub fn component(&self, name: &str) -> Option<&PatternManifestComponent> {
        self.components.get(name)
    }

    /// 返回匹配本机 DLL 的远端 pattern 路径.
    ///
    /// # Errors
    ///
    /// `sha256` 不是 64 位十六进制字符串时返回错误.
    pub fn matching_pattern(
        &self,
        component: &str,
        dll_name: &str,
        sha256: &str,
    ) -> Result<Option<&str>> {
        validate_sha256(sha256, "requested DLL")?;
        let Some(entry) = self.component(component) else {
            return Ok(None);
        };
        if entry.dll != dll_name || !entry.sha256.eq_ignore_ascii_case(sha256) {
            return Ok(None);
        }
        Ok(Some(entry.pattern()))
    }

    fn validate(&self) -> Result<()> {
        if self.schema != SUPPORTED_SCHEMA {
            return Err(MetadataError::Invalid(format!(
                "unsupported pattern manifest schema {}",
                self.schema
            )));
        }
        if self.channel.trim().is_empty() {
            return Err(MetadataError::Invalid(
                "pattern manifest channel is empty".into(),
            ));
        }
        if self.steam_build.trim().is_empty() {
            return Err(MetadataError::Invalid(
                "pattern manifest Steam build is empty".into(),
            ));
        }
        if self.components.is_empty() {
            return Err(MetadataError::Invalid(
                "pattern manifest has no components".into(),
            ));
        }

        for (name, entry) in &self.components {
            if name.trim().is_empty() || name.contains(['/', '\\']) {
                return Err(MetadataError::Invalid(
                    "pattern manifest has an invalid component name".into(),
                ));
            }
            entry.validate(name)?;
        }
        Ok(())
    }
}

impl PatternManifestComponent {
    /// 返回组件对应的 DLL 文件名.
    pub fn dll(&self) -> &str {
        &self.dll
    }

    /// 返回组件对应的 DLL SHA-256.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// 返回仓库内的 pattern 相对路径.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    fn validate(&self, component: &str) -> Result<()> {
        if self.dll.trim().is_empty() {
            return Err(MetadataError::Invalid(format!(
                "pattern manifest component {component} has no DLL"
            )));
        }
        validate_sha256(&self.sha256, component)?;
        let expected = format!("{component}/{}.toml", self.sha256.to_ascii_lowercase());
        if self.pattern != expected {
            return Err(MetadataError::Invalid(format!(
                "pattern manifest component {component} has invalid pattern path"
            )));
        }
        Ok(())
    }
}

fn validate_sha256(value: &str, label: &str) -> Result<()> {
    let valid = value.len() == SHA256_LEN && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    if valid {
        Ok(())
    } else {
        Err(MetadataError::Invalid(format!(
            "{label} has invalid SHA-256"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
{
  "schema": 1,
  "channel": "stable",
  "steam_build": "1785187029",
  "components": {
    "steamui": {
      "dll": "steamui.dll",
      "sha256": "55a42fc091aee1aa68e6901f3647d732c30f1ad7aeb037ec7b0c425e8029defd",
      "pattern": "steamui/55a42fc091aee1aa68e6901f3647d732c30f1ad7aeb037ec7b0c425e8029defd.toml"
    }
  }
}
"#;

    #[test]
    fn matches_component_by_dll_and_sha() {
        let manifest = PatternManifest::parse(MANIFEST.as_bytes()).unwrap();

        assert_eq!(manifest.channel(), "stable");
        assert_eq!(manifest.steam_build(), "1785187029");
        assert_eq!(
            manifest
                .matching_pattern(
                    "steamui",
                    "steamui.dll",
                    "55A42FC091AEE1AA68E6901F3647D732C30F1AD7AEB037EC7B0C425E8029DEFD"
                )
                .unwrap(),
            Some("steamui/55a42fc091aee1aa68e6901f3647d732c30f1ad7aeb037ec7b0c425e8029defd.toml")
        );
    }

    #[test]
    fn rejects_path_that_does_not_match_component_sha() {
        let body = MANIFEST.replace(
            "steamui/55a42fc091aee1aa68e6901f3647d732c30f1ad7aeb037ec7b0c425e8029defd.toml",
            "steamui/other.toml",
        );

        assert!(matches!(
            PatternManifest::parse(body.as_bytes()),
            Err(MetadataError::Invalid(message)) if message.contains("invalid pattern path")
        ));
    }

    #[test]
    fn returns_none_for_another_dll() {
        let manifest = PatternManifest::parse(MANIFEST.as_bytes()).unwrap();

        assert_eq!(
            manifest
                .matching_pattern(
                    "steamui",
                    "steamclient64.dll",
                    "55a42fc091aee1aa68e6901f3647d732c30f1ad7aeb037ec7b0c425e8029defd"
                )
                .unwrap(),
            None
        );
    }
}
