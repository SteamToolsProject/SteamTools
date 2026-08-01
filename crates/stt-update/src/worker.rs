//! 编排一次更新检查 (由 host 在后台线程调用, 不得阻塞启动).

use std::path::Path;

use crate::checksums::Checksums;
use crate::discover::latest_tag_from_redirect;
use crate::stage::stage_host;
use crate::swap::{
    apply_staged, cleanup_after_apply, current_version, read_applied_tag, rollback_if_broken,
};
use crate::version::Version;
use crate::{asset_url, latest_release_url, Fetcher, CHECKSUMS_FILE_NAME};

/// 一次更新检查的结果 (host 转成日志和面板状态).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// 发现新版本并已换进根目录; 重启 Steam 生效.
    Applied { tag: String },
    /// 上一个会话已换好, 只等重启.
    AppliedPendingRestart { tag: String },
    /// 已是最新, 无需动作.
    Latest,
    /// 检查失败 (网络/校验/落盘), 不阻塞, 等下次启动再试.
    Failed(String),
}

impl UpdateOutcome {
    /// host.log 用的一行摘要.
    pub fn summary(&self) -> String {
        match self {
            UpdateOutcome::Applied { tag } => format!("update=applied tag={tag} restart_to_use=1"),
            UpdateOutcome::AppliedPendingRestart { tag } => {
                format!("update=applied_pending_restart tag={tag}")
            }
            UpdateOutcome::Latest => "update=latest".to_owned(),
            UpdateOutcome::Failed(reason) => format!("update=failed reason={reason}"),
        }
    }
}

/// 执行一轮完整更新检查. 出错全部收敛成 [`UpdateOutcome::Failed`], 绝不 panic.
pub fn run(steam_root: &Path, fetcher: &dyn Fetcher) -> UpdateOutcome {
    match rollback_if_broken(steam_root) {
        Ok(true) => {}
        Ok(false) => {}
        Err(error) => return failed(format!("rollback {error}")),
    }

    let current = current_version();
    if let Some(tag) = read_applied_tag(steam_root) {
        match Version::parse(&tag) {
            // 已应用的版本比运行版本新: 上次换好了还没重启, 别重复下载.
            Some(applied) if applied > current => {
                return UpdateOutcome::AppliedPendingRestart { tag }
            }
            // 运行版本已经追上 (或更早): 新 DLL 已经跑起来了, 清残留.
            _ => match cleanup_after_apply(steam_root) {
                Ok(_) => {}
                Err(error) => return failed(format!("cleanup {error}")),
            },
        }
    }

    let latest_url = latest_release_url();
    let response = match fetcher.get(&latest_url, 64 * 1024) {
        Ok(r) => r,
        Err(error) => return failed(format!("discover {error}")),
    };
    if !(301..=308).contains(&response.status) {
        return failed(format!("discover status={}", response.status));
    }
    let tag = match latest_tag_from_redirect(response.location.as_deref()) {
        Ok(tag) => tag,
        Err(error) => return failed(format!("discover {error}")),
    };
    let Some(remote) = Version::parse(&tag) else {
        return failed(format!("discover tag={tag} not semver"));
    };
    if !remote.is_newer_than(current) {
        return UpdateOutcome::Latest;
    }

    let checksums_text = match fetch_checksums(fetcher, &tag) {
        Ok(text) => text,
        Err(outcome) => return outcome,
    };
    let checksums = Checksums::parse(&checksums_text);

    if let Err(error) = stage_host(steam_root, fetcher, &tag, &checksums) {
        return failed(format!("stage {error}"));
    }
    if let Err(error) = apply_staged(steam_root, &tag) {
        return failed(format!("swap {error}"));
    }
    UpdateOutcome::Applied { tag }
}

fn fetch_checksums(fetcher: &dyn Fetcher, tag: &str) -> Result<String, UpdateOutcome> {
    let url = asset_url(tag, CHECKSUMS_FILE_NAME);
    let response = fetcher
        .get(&url, 1024 * 1024)
        .map_err(|error| failed(format!("checksums {error}")))?;
    if response.status != 200 {
        return Err(failed(format!("checksums status={}", response.status)));
    }
    Ok(String::from_utf8_lossy(&response.body).into_owned())
}

fn failed(reason: String) -> UpdateOutcome {
    UpdateOutcome::Failed(reason)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::HOST_FILE_NAME;
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

    fn redirect_response(location: &str) -> HttpResponse {
        HttpResponse {
            status: 302,
            body: vec![],
            location: Some(location.to_owned()),
        }
    }

    fn ok_response(body: &[u8]) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: body.to_vec(),
            location: None,
        }
    }

    /// 构造一个"有 v9.9.9 可用"的假远端.
    fn fake_remote(host_body: &[u8]) -> FakeFetcher {
        let checksums = format!(
            "{}  {HOST_FILE_NAME}\n",
            stt_platform::sha256_bytes(host_body)
        );
        let mut responses = HashMap::new();
        responses.insert(
            latest_release_url(),
            redirect_response("/SteamToolsProject/SteamTools/releases/tag/v9.9.9"),
        );
        responses.insert(
            asset_url("v9.9.9", CHECKSUMS_FILE_NAME),
            ok_response(checksums.as_bytes()),
        );
        responses.insert(asset_url("v9.9.9", HOST_FILE_NAME), ok_response(host_body));
        FakeFetcher { responses }
    }

    #[test]
    fn full_flow_applies_new_host() {
        let root = tempfile::tempdir().unwrap();
        // 模拟旧宿主在根目录.
        std::fs::write(root.path().join(HOST_FILE_NAME), b"old-host").unwrap();
        let fetcher = fake_remote(b"new-host");

        let outcome = run(root.path(), &fetcher);

        assert_eq!(
            outcome,
            UpdateOutcome::Applied {
                tag: "v9.9.9".to_owned()
            }
        );
        assert_eq!(
            std::fs::read(root.path().join(HOST_FILE_NAME)).unwrap(),
            b"new-host"
        );
        // 旧文件留了 .old 备份, 等下次启动清理.
        assert!(root.path().join(format!("{HOST_FILE_NAME}.old")).is_file());
    }

    #[test]
    fn already_latest_is_noop() {
        let root = tempfile::tempdir().unwrap();
        let mut responses = HashMap::new();
        responses.insert(
            latest_release_url(),
            redirect_response("/SteamToolsProject/SteamTools/releases/tag/v0.1.0"),
        );
        let fetcher = FakeFetcher { responses };

        assert_eq!(run(root.path(), &fetcher), UpdateOutcome::Latest);
    }

    #[test]
    fn pending_restart_skips_redownload() {
        let root = tempfile::tempdir().unwrap();
        // 上个会话已应用 v9.9.9, 状态文件在, 但当前运行版本还是旧的.
        std::fs::write(root.path().join(HOST_FILE_NAME), b"new-host").unwrap();
        std::fs::create_dir_all(crate::update_dir(root.path())).unwrap();
        std::fs::write(crate::state_file(root.path()), "v9.9.9").unwrap();
        // 远端不可达也 OK: pending_restart 不该发网络请求.
        let fetcher = FakeFetcher {
            responses: HashMap::new(),
        };

        assert_eq!(
            run(root.path(), &fetcher),
            UpdateOutcome::AppliedPendingRestart {
                tag: "v9.9.9".to_owned()
            }
        );
    }

    #[test]
    fn cleanup_runs_when_version_caught_up() {
        let root = tempfile::tempdir().unwrap();
        // 新版本已经跑起来 (状态文件版本 == 运行版本), 残留 .old/staging 被清掉.
        std::fs::write(root.path().join(HOST_FILE_NAME), b"new-host").unwrap();
        std::fs::write(root.path().join(format!("{HOST_FILE_NAME}.old")), b"old").unwrap();
        std::fs::create_dir_all(crate::update_dir(root.path())).unwrap();
        let current = env!("CARGO_PKG_VERSION");
        std::fs::write(crate::state_file(root.path()), format!("v{current}")).unwrap();
        let mut responses = HashMap::new();
        responses.insert(
            latest_release_url(),
            redirect_response(&format!(
                "/SteamToolsProject/SteamTools/releases/tag/v{current}"
            )),
        );
        let fetcher = FakeFetcher { responses };

        assert_eq!(run(root.path(), &fetcher), UpdateOutcome::Latest);
        assert!(!root.path().join(format!("{HOST_FILE_NAME}.old")).exists());
        assert!(!crate::state_file(root.path()).exists());
    }

    #[test]
    fn bad_checksum_fails_cleanly_and_keeps_old_host() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(HOST_FILE_NAME), b"old-host").unwrap();
        // 远端给的 checksum 对不上.
        let mut responses = HashMap::new();
        responses.insert(
            latest_release_url(),
            redirect_response("/SteamToolsProject/SteamTools/releases/tag/v9.9.9"),
        );
        responses.insert(
            asset_url("v9.9.9", CHECKSUMS_FILE_NAME),
            ok_response(format!("{}  {HOST_FILE_NAME}", "a".repeat(64)).as_bytes()),
        );
        responses.insert(
            asset_url("v9.9.9", HOST_FILE_NAME),
            ok_response(b"tampered-host"),
        );
        let fetcher = FakeFetcher { responses };

        let outcome = run(root.path(), &fetcher);
        assert!(matches!(outcome, UpdateOutcome::Failed(_)));
        // 旧宿主没被动过.
        assert_eq!(
            std::fs::read(root.path().join(HOST_FILE_NAME)).unwrap(),
            b"old-host"
        );
    }

    #[test]
    fn rollback_restores_after_broken_swap() {
        let root = tempfile::tempdir().unwrap();
        // 模拟上次 swap 只完成一半: 新文件没落盘, 只有 .old.
        std::fs::write(
            root.path().join(format!("{HOST_FILE_NAME}.old")),
            b"old-host",
        )
        .unwrap();
        let fetcher = fake_remote(b"new-host");

        let outcome = run(root.path(), &fetcher);

        // 先回滚 .old, 再正常走一轮更新.
        assert_eq!(
            outcome,
            UpdateOutcome::Applied {
                tag: "v9.9.9".to_owned()
            }
        );
        assert_eq!(
            std::fs::read(root.path().join(HOST_FILE_NAME)).unwrap(),
            b"new-host"
        );
    }
}
