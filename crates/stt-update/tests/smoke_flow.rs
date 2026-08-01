//! 冒烟: 真实 WinHTTP + 本地 HTTP 服务器, 走完整更新链路.
//!
//! 覆盖: latest 302 Location -> checksums 下载 -> 宿主下载 -> SHA-256 校验 ->
//! swap 进根目录 -> 二次运行视为"待重启"不再重下 -> 版本追平后清理 .old.
//! 不依赖外网与真实 Steam, 只用一个本地 TCP 服务器模拟 GitHub releases 布局.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::thread;

use stt_update::worker::{run, UpdateOutcome};
use stt_update::Fetcher;

/// 把 GitHub release URL 重写到本地 mock 服务器 (其余字段原样走 WinHTTP).
struct RewritingFetcher {
    base: String,
}

impl RewritingFetcher {
    fn new(base: &str) -> Self {
        Self {
            base: base.to_owned(),
        }
    }
}

impl Fetcher for RewritingFetcher {
    fn get(
        &self,
        url: &str,
        max_body_bytes: usize,
    ) -> Result<stt_platform::HttpResponse, stt_platform::HttpError> {
        let rewritten = url.replace(
            "https://github.com/SteamToolsProject/SteamTools",
            &self.base,
        );
        stt_update::PlatformFetcher.get(&rewritten, max_body_bytes)
    }
}

/// 迷你 mock, 三个路径:
///   /releases/latest                  -> 302 Location 指向 /releases/tag/<tag>
///   /releases/download/<tag>/checksums.sha256 -> 200 + checksums 文本
///   /releases/download/<tag>/stbase.dll       -> 200 + host 二进制
fn spawn_mock(host_bytes: &[u8], checksums: String, tag: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let host_bytes = host_bytes.to_vec();
    let tag = tag.to_owned();

    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
            let mut buf = [0u8; 4096];
            let Ok(n) = stream.read(&mut buf) else {
                continue;
            };
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/");

            let response: String = if path == "/releases/latest" {
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: /SteamToolsProject/SteamTools/releases/tag/{tag}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
            } else if path == format!("/releases/download/{tag}/checksums.sha256") {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{checksums}",
                    checksums.len()
                )
            } else if path == format!("/releases/download/{tag}/stbase.dll") {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    host_bytes.len()
                )
            } else {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_owned()
            };

            let _ = stream.write_all(response.as_bytes());
            // stbase.dll 是二进制, 追加在 header 之后.
            if path == format!("/releases/download/{tag}/stbase.dll") {
                let _ = stream.write_all(&host_bytes);
            }
        }
    });

    format!("http://127.0.0.1:{port}")
}

fn checksums_for(body: &[u8]) -> String {
    format!("{}  stbase.dll\n", stt_platform::sha256_bytes(body))
}

fn fake_root() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// 模拟旧宿主: 一个不是目标版本的 stbase.dll.
fn install_old_host(root: &Path) {
    std::fs::write(root.join("stbase.dll"), b"old-host-bytes").unwrap();
}

#[test]
fn full_flow_applies_then_pending_then_cleanup() {
    let base = spawn_mock(
        b"new-host-bytes",
        checksums_for(b"new-host-bytes"),
        "v9.9.9",
    );
    let fetcher = RewritingFetcher::new(&base);

    // 第一轮: 有可用更新, 下载校验并 swap.
    let root = fake_root();
    install_old_host(root.path());
    assert_eq!(
        run(root.path(), &fetcher),
        UpdateOutcome::Applied {
            tag: "v9.9.9".to_owned()
        }
    );
    assert_eq!(
        std::fs::read(root.path().join("stbase.dll")).unwrap(),
        b"new-host-bytes"
    );
    assert!(root.path().join("stbase.dll.old").is_file());
    assert!(root.path().join("steamtools").join("update").is_dir());

    // 第二轮 (模拟还没重启): 状态文件 tag 比运行版本新, 应直接返回待重启,
    // 不重复下载, 根目录文件不变.
    assert_eq!(
        run(root.path(), &fetcher),
        UpdateOutcome::AppliedPendingRestart {
            tag: "v9.9.9".to_owned()
        }
    );
}

#[test]
fn cleanup_runs_when_running_version_catches_up() {
    // 状态文件已写当前运行版本 -> 清理 .old/staging/状态, 远端也是当前版本 -> Latest.
    let current_tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    let base = spawn_mock(
        b"new-host-bytes",
        checksums_for(b"new-host-bytes"),
        &current_tag,
    );
    let fetcher = RewritingFetcher::new(&base);

    let root = fake_root();
    std::fs::write(root.path().join("stbase.dll"), b"new-host-bytes").unwrap();
    std::fs::write(root.path().join("stbase.dll.old"), b"old").unwrap();
    let update_dir = root.path().join("steamtools").join("update");
    std::fs::create_dir_all(&update_dir).unwrap();
    std::fs::write(update_dir.join("applied_tag.txt"), &current_tag).unwrap();
    let staging = update_dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();

    assert_eq!(run(root.path(), &fetcher), UpdateOutcome::Latest);
    assert!(!root.path().join("stbase.dll.old").exists());
    assert!(!update_dir.exists());
    assert_eq!(
        std::fs::read(root.path().join("stbase.dll")).unwrap(),
        b"new-host-bytes"
    );
}

#[test]
fn bad_checksum_keeps_old_host_and_aborts() {
    // mock 里 checksums 与 stbase 内容故意不一致.
    let base = spawn_mock(b"different-bytes", checksums_for(b"other-bytes"), "v9.9.9");
    let fetcher = RewritingFetcher::new(&base);

    let root = fake_root();
    install_old_host(root.path());
    let outcome = run(root.path(), &fetcher);
    assert!(matches!(outcome, UpdateOutcome::Failed(_)), "{outcome:?}");
    // 旧宿主原封不动, 没有 .old 也没有 staging.
    assert_eq!(
        std::fs::read(root.path().join("stbase.dll")).unwrap(),
        b"old-host-bytes"
    );
    assert!(!root.path().join("stbase.dll.old").exists());
    assert!(!root.path().join("steamtools").join("update").is_dir());
}

#[test]
fn broken_swap_recovers_from_old() {
    // 模拟上次 swap 只完成一半: 根目录只有 .old, 新文件没落盘.
    let base = spawn_mock(
        b"new-host-bytes",
        checksums_for(b"new-host-bytes"),
        "v9.9.9",
    );
    let fetcher = RewritingFetcher::new(&base);

    let root = fake_root();
    std::fs::write(root.path().join("stbase.dll.old"), b"old-host-bytes").unwrap();

    assert_eq!(
        run(root.path(), &fetcher),
        UpdateOutcome::Applied {
            tag: "v9.9.9".to_owned()
        }
    );
    assert_eq!(
        std::fs::read(root.path().join("stbase.dll")).unwrap(),
        b"new-host-bytes"
    );
    // 回滚后 .old 重新被占用为新备份.
    assert!(root.path().join("stbase.dll.old").is_file());
}
