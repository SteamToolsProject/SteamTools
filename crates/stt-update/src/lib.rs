//! 自更新: 发现最新 tag, 校验后 stage, 重启时 swap 进宿主.
//!
//! 自动更新只换 `stbase.dll` (宿主). 两个 loader 是纯转发壳, 基本不变, 交给安装器,
//! 这里不碰, 以缩小 swap 失败时的风险面 (Steam 找不到 dwmapi/xinput 壳会退回系统 DLL).

pub mod checksums;
pub mod discover;
pub mod error;
pub mod stage;
pub mod swap;
pub mod version;
pub mod worker;

pub use error::UpdateError;

use std::path::{Path, PathBuf};

/// 数据目录下的更新子目录: `steamtools/update/`.
pub const UPDATE_DIR_NAME: &str = "update";
/// staging 子目录名, 存放校验通过的下载文件.
pub const STAGING_DIR_NAME: &str = "staging";
/// 自动更新的目标文件 (宿主 DLL).
pub const HOST_FILE_NAME: &str = "stbase.dll";
/// Release 里带 SHA-256 清单的资产名.
pub const CHECKSUMS_FILE_NAME: &str = "checksums.sha256";
/// 记录"已应用的 tag"的状态文件名.
pub const STATE_FILE_NAME: &str = "applied_tag.txt";

const REPO_OWNER: &str = "SteamToolsProject";
const REPO_NAME: &str = "SteamTools";

pub fn update_dir(steam_root: &Path) -> PathBuf {
    stt_platform::data_dir(steam_root).join(UPDATE_DIR_NAME)
}

pub fn staging_dir(steam_root: &Path) -> PathBuf {
    update_dir(steam_root).join(STAGING_DIR_NAME)
}

pub fn state_file(steam_root: &Path) -> PathBuf {
    update_dir(steam_root).join(STATE_FILE_NAME)
}

pub fn latest_release_url() -> String {
    format!("https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/latest")
}

pub fn asset_url(tag: &str, name: &str) -> String {
    format!("https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/download/{tag}/{name}")
}

/// 更新检查用的受限 GET. 注入以便单测用假数据, 实机用 [`PlatformFetcher`].
pub trait Fetcher: Send + Sync {
    /// GET 一个 URL, 响应体不得超过 `max_body_bytes`.
    fn get(
        &self,
        url: &str,
        max_body_bytes: usize,
    ) -> Result<HttpResponse, stt_platform::HttpError>;
}

/// 走 `stt-platform` 的 WinHTTP 实现.
pub struct PlatformFetcher;

impl Fetcher for PlatformFetcher {
    fn get(
        &self,
        url: &str,
        max_body_bytes: usize,
    ) -> Result<HttpResponse, stt_platform::HttpError> {
        stt_platform::winhttp_request(
            stt_platform::HttpMethod::Get,
            url,
            &[("User-Agent".to_owned(), user_agent())],
            &[],
            stt_platform::WinHttpRequestOptions {
                timeouts: FETCH_TIMEOUTS,
                max_request_body_bytes: 0,
                max_response_body_bytes: max_body_bytes,
            },
        )
    }
}

fn user_agent() -> String {
    format!("SteamTools/{}", env!("CARGO_PKG_VERSION"))
}

/// 下载资产比常规请求慢 (数 MB), 收包时限放宽.
const FETCH_TIMEOUTS: stt_platform::WinHttpTimeouts = stt_platform::WinHttpTimeouts {
    resolve_ms: 10_000,
    connect_ms: 10_000,
    send_ms: 10_000,
    receive_ms: 60_000,
};

type HttpResponse = stt_platform::HttpResponse;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_follow_release_layout() {
        assert_eq!(
            latest_release_url(),
            "https://github.com/SteamToolsProject/SteamTools/releases/latest"
        );
        assert_eq!(
            asset_url("v0.2.0", "stbase.dll"),
            "https://github.com/SteamToolsProject/SteamTools/releases/download/v0.2.0/stbase.dll"
        );
    }

    #[test]
    fn data_dirs_nest_under_steamtools() {
        let root = Path::new(r"C:\Steam");
        assert_eq!(
            staging_dir(root),
            PathBuf::from(r"C:\Steam\steamtools\update\staging")
        );
        assert_eq!(
            state_file(root),
            PathBuf::from(r"C:\Steam\steamtools\update\applied_tag.txt")
        );
    }
}
