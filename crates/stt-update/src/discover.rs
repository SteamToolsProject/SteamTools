//! 从 `/releases/latest` 的 3xx Location 里解出最新 tag.

use crate::error::UpdateError;
use crate::version::Version;

/// GitHub `/releases/latest` 对禁重定向的客户端返回 302, Location 形如
/// `/SteamToolsProject/SteamTools/releases/tag/v0.2.0`. 这里只取 tag 并校验格式.
pub fn latest_tag_from_redirect(location: Option<&str>) -> Result<String, UpdateError> {
    let location = location.ok_or(UpdateError::MissingTag)?;
    let marker = "/releases/tag/";
    let Some(pos) = location.rfind(marker) else {
        return Err(UpdateError::InvalidTag(location.to_owned()));
    };
    let tag = &location[pos + marker.len()..];
    let tag = tag.split(['?', '#']).next().unwrap_or(tag);
    if tag.is_empty() {
        return Err(UpdateError::InvalidTag(location.to_owned()));
    }
    // 只认 `vX.Y.Z`, 防 Location 被污染成任意路径.
    Version::parse(tag).ok_or_else(|| UpdateError::InvalidTag(location.to_owned()))?;
    Ok(tag.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_tag_from_github_redirect() {
        let location = Some("/SteamToolsProject/SteamTools/releases/tag/v0.2.0");
        assert_eq!(latest_tag_from_redirect(location).unwrap(), "v0.2.0");
    }

    #[test]
    fn rejects_missing_location() {
        assert!(matches!(
            latest_tag_from_redirect(None),
            Err(UpdateError::MissingTag)
        ));
    }

    #[test]
    fn rejects_unrelated_path() {
        assert!(matches!(
            latest_tag_from_redirect(Some("/login?return_to=/releases")),
            Err(UpdateError::InvalidTag(_))
        ));
    }

    #[test]
    fn rejects_tag_that_is_not_semver() {
        assert!(matches!(
            latest_tag_from_redirect(Some("/SteamToolsProject/SteamTools/releases/tag/random")),
            Err(UpdateError::InvalidTag(_))
        ));
    }

    #[test]
    fn strips_query_string() {
        let location = Some("/SteamToolsProject/SteamTools/releases/tag/v0.2.0?foo=1");
        assert_eq!(latest_tag_from_redirect(location).unwrap(), "v0.2.0");
    }
}
