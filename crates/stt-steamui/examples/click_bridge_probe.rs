//! 端到端验证 click_bridge 的鉴权: 起一座真桥, 把攻击者能发的请求都打一遍.
//!
//! ```text
//! cargo run -p stt-steamui --example click_bridge_probe
//! ```

#![cfg(windows)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 发一条裸 HTTP 请求, 返回状态行.
fn send(port: u16, raw: &str) -> String {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
        return "connect failed".into();
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = s.write_all(raw.as_bytes());
    let mut buf = [0u8; 256];
    let n = s.read(&mut buf).unwrap_or(0);
    String::from_utf8_lossy(&buf[..n])
        .lines()
        .next()
        .unwrap_or("(no response)")
        .to_owned()
}

fn main() {
    let hits = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&hits);
    stt_steamui::ensure_click_bridge(move |app_id| {
        println!("    !! 桥接受了 app_id={app_id}");
        counter.fetch_add(1, Ordering::SeqCst);
    });

    let port = stt_steamui::click_bridge_port();
    let token = stt_steamui::click_bridge_token();
    if port == 0 || token.is_empty() {
        println!("FAIL 桥没起来 (port={port} token_len={})", token.len());
        return;
    }
    println!("bridge port={port} token={}...", &token[..8]);

    // 攻击者能做的: 不知道 token, 只能猜路径/方法/参数位置.
    let attacks = [
        (
            "img 标签盲喷 (GET)",
            "GET /stt/add?appid=730 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".to_owned(),
        ),
        (
            "fetch no-cors POST 无 token",
            "POST /stt/add?appid=730 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".to_owned(),
        ),
        (
            "猜错 token",
            format!(
                "POST /stt/add?token={}&appid=730 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
                "0".repeat(32)
            ),
        ),
        (
            "把 appid 藏在 Referer 头里",
            format!(
                "POST /stt/add?token={token} HTTP/1.1\r\nReferer: http://evil.test/?appid=999\r\n\r\n"
            ),
        ),
        (
            "换条路径",
            format!("POST /?token={token}&appid=730 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"),
        ),
    ];

    println!("\n-- 攻击者视角 (都该被拒) --");
    for (name, raw) in &attacks {
        println!("  {name}: {}", send(port, raw));
    }
    let after_attacks = hits.load(Ordering::SeqCst);

    println!("\n-- 我方注入脚本 (该通过) --");
    let legit =
        format!("POST /stt/add?token={token}&appid=730 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
    println!("  正常请求: {}", send(port, &legit));

    // 回调在工作线程里跑, 等它一下.
    std::thread::sleep(Duration::from_millis(300));
    let total = hits.load(Ordering::SeqCst);

    println!();
    if after_attacks == 0 && total == 1 {
        println!("PROBE_OK 攻击全被拒, 合法请求通过");
    } else {
        println!("PROBE_FAIL 攻击命中={after_attacks} 总命中={total} (期望 0 / 1)");
    }
}
