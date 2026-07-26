//! 端到端验证 DevTools pipe: 用**我们自己**构造的 `CreateProcessW` 参数起一个
//! Chromium, 走继承的 fd 3/4 发一条 CDP 命令并等回复.
//!
//! 目的是在碰 Steam 之前就把 CRT fd 块 + 句柄白名单 + STARTUPINFOEX 这条链路验穿.
//!
//! ```text
//! cargo run -p stt-platform --example pipe_probe -- <chromium.exe> <user-data-dir>
//! ```

#![cfg(windows)]

use std::sync::mpsc;
use std::time::Duration;

use windows::core::PWSTR;
use windows::Win32::System::Threading::{
    CreateProcessW, TerminateProcess, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW,
};

fn main() {
    let mut args = std::env::args().skip(1);
    let exe = args
        .next()
        .expect("usage: pipe_probe <chromium.exe> <profile-dir> [extra args]");
    let profile = args
        .next()
        .expect("usage: pipe_probe <chromium.exe> <profile-dir> [extra args]");
    // 子进程没有 stdio (fd 0/1/2 故意不打开), 想看它的日志就用 --log-file 之类.
    let extra: Vec<String> = args.collect();

    let (pipe, launch) = unsafe { stt_platform::prepare_devtools_pipe(std::ptr::null()) }
        .expect("prepare_devtools_pipe");

    let cmdline = format!(
        "\"{exe}\" --remote-debugging-pipe --headless=new --no-first-run \
         --no-default-browser-check --disable-gpu \"--user-data-dir={profile}\" {} about:blank",
        extra.join(" ")
    );
    let mut cmd: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();

    let mut pi = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            None,
            PWSTR(cmd.as_mut_ptr()),
            None,
            None,
            // 句柄白名单已限定只继承那两个管道端.
            true,
            PROCESS_CREATION_FLAGS(stt_platform::EXTENDED_STARTUPINFO_PRESENT),
            None,
            None,
            launch.startup_info().cast::<STARTUPINFOW>(),
            &mut pi,
        )
    }
    .expect("CreateProcessW");
    // 子进程已经拿到句柄, 我方这侧不再需要 launch 里那两端.
    drop(launch);

    pipe.send(r#"{"id":1,"method":"Browser.getVersion"}"#)
        .expect("send");

    // 匿名管道的读是阻塞的, 丢给线程好加超时.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut acc = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    acc.extend_from_slice(&buf[..n]);
                    if let Some(i) = acc.iter().position(|&b| b == 0) {
                        let _ = tx.send(String::from_utf8_lossy(&acc[..i]).into_owned());
                        return;
                    }
                }
            }
        }
        let _ = tx.send(String::new());
    });

    let verdict = match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(s) if !s.is_empty() => format!("PIPE_OK {}", s.chars().take(300).collect::<String>()),
        Ok(_) => "PIPE_CLOSED (child exited without replying)".into(),
        Err(_) => "PIPE_TIMEOUT (no reply on fd 4)".into(),
    };
    println!("{verdict}");

    unsafe {
        let _ = TerminateProcess(pi.hProcess, 0);
    }
}
