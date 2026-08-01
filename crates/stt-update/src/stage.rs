//! 把下载的宿主放进 staging 目录并校验.

use std::path::{Path, PathBuf};

use crate::error::UpdateError;
use crate::{asset_url, checksums::Checksums, staging_dir, Fetcher, HOST_FILE_NAME};

/// 宿主 DLL 的响应体上限 (当前约 5.6 MB, 留余量).
const HOST_MAX_BYTES: usize = 16 * 1024 * 1024;

/// 下载并校验宿主, 落到 `steamtools/update/staging/stbase.dll`.
///
/// 先下载到同目录临时文件, 校验 SHA-256 一致后再原子改名, 避免半截文件.
pub fn stage_host(
    steam_root: &Path,
    fetcher: &dyn Fetcher,
    tag: &str,
    checksums: &Checksums,
) -> Result<PathBuf, UpdateError> {
    let expected = checksums
        .get(HOST_FILE_NAME)
        .ok_or_else(|| UpdateError::MissingEntry(HOST_FILE_NAME.to_owned()))?;
    let url = asset_url(tag, HOST_FILE_NAME);
    let response = fetcher
        .get(&url, HOST_MAX_BYTES)
        .map_err(|source| UpdateError::Http {
            url: url.clone(),
            source,
        })?;
    if response.status != 200 {
        return Err(UpdateError::BadStatus {
            url,
            status: response.status,
        });
    }
    let actual = stt_platform::sha256_bytes(&response.body);
    if actual != expected {
        return Err(UpdateError::ChecksumMismatch {
            name: HOST_FILE_NAME.to_owned(),
            expected: expected.to_owned(),
            actual,
        });
    }
    write_staged(steam_root, &response.body)
}

/// 写 staging 文件: 临时名落盘后改名 (同目录 rename 原子).
fn write_staged(steam_root: &Path, bytes: &[u8]) -> Result<PathBuf, UpdateError> {
    let dir = staging_dir(steam_root);
    std::fs::create_dir_all(&dir).map_err(|source| UpdateError::Io {
        path: dir.clone(),
        source,
    })?;
    let tmp = dir.join(format!("{HOST_FILE_NAME}.tmp"));
    std::fs::write(&tmp, bytes).map_err(|source| UpdateError::Io {
        path: tmp.clone(),
        source,
    })?;
    let target = dir.join(HOST_FILE_NAME);
    std::fs::rename(&tmp, &target).map_err(|source| UpdateError::Io {
        path: target.clone(),
        source,
    })?;
    Ok(target)
}

/// 清掉 staging 目录 (更新已应用或已放弃).
pub fn clear_staging(steam_root: &Path) {
    let dir = staging_dir(steam_root);
    if dir.is_dir() {
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::checksums::Checksums;
    use crate::Fetcher;
    use stt_platform::HttpResponse;

    struct FakeFetcher {
        responses: HashMap<String, HttpResponse>,
    }

    impl Fetcher for FakeFetcher {
        fn get(&self, url: &str, _max: usize) -> Result<HttpResponse, stt_platform::HttpError> {
            self.responses
                .get(url)
                .cloned()
                .ok_or(stt_platform::HttpError::InvalidUrl("missing fake response"))
        }
    }

    fn fake_response(body: &[u8]) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: body.to_vec(),
            location: None,
        }
    }

    #[test]
    fn stages_and_verifies_good_host() {
        let root = tempfile::tempdir().unwrap();
        let body = b"new host bytes";
        let url = asset_url("v0.2.0", HOST_FILE_NAME);
        let fetcher = FakeFetcher {
            responses: HashMap::from([(url, fake_response(body))]),
        };
        let checksums = Checksums::parse(&format!(
            "{}  {HOST_FILE_NAME}",
            stt_platform::sha256_bytes(body)
        ));

        let path = stage_host(root.path(), &fetcher, "v0.2.0", &checksums).unwrap();
        assert!(path.is_file());
        assert_eq!(std::fs::read(&path).unwrap(), body);
    }

    #[test]
    fn rejects_checksum_mismatch() {
        let root = tempfile::tempdir().unwrap();
        let url = asset_url("v0.2.0", HOST_FILE_NAME);
        let fetcher = FakeFetcher {
            responses: HashMap::from([(url, fake_response(b"tampered"))]),
        };
        let checksums = Checksums::parse(&format!(
            "{}  {HOST_FILE_NAME}",
            stt_platform::sha256_bytes(b"original")
        ));

        assert!(matches!(
            stage_host(root.path(), &fetcher, "v0.2.0", &checksums),
            Err(UpdateError::ChecksumMismatch { .. })
        ));
        // staging 目录里不该留半截文件.
        assert!(!staging_dir(root.path()).join(HOST_FILE_NAME).exists());
    }

    #[test]
    fn rejects_missing_checksum_entry() {
        let root = tempfile::tempdir().unwrap();
        let fetcher = FakeFetcher {
            responses: HashMap::new(),
        };
        let checksums = Checksums::parse("");

        assert!(matches!(
            stage_host(root.path(), &fetcher, "v0.2.0", &checksums),
            Err(UpdateError::MissingEntry(_))
        ));
    }

    #[test]
    fn rejects_non_200() {
        let root = tempfile::tempdir().unwrap();
        let url = asset_url("v0.2.0", HOST_FILE_NAME);
        let fetcher = FakeFetcher {
            responses: HashMap::from([(
                url,
                HttpResponse {
                    status: 404,
                    body: vec![],
                    location: None,
                },
            )]),
        };
        let checksums = Checksums::parse(&format!("{}  {HOST_FILE_NAME}", "a".repeat(64)));

        assert!(matches!(
            stage_host(root.path(), &fetcher, "v0.2.0", &checksums),
            Err(UpdateError::BadStatus { status: 404, .. })
        ));
    }
}
