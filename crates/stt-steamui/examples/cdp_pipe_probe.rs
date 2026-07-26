//! 端到端验证 CDP over pipe 的**会话层**: `Target.getTargets` →
//! `Target.attachToTarget{flatten}` → `Runtime.evaluate`.
//!
//! 用普通 Chromium 跑, 不碰 Steam — 会话层与浏览器无关, 能在这里验穿就说明
//! 逻辑对, 剩下的只是 webhelper 能不能起来.
//!
//! ```text
//! cargo run -p stt-steamui --example cdp_pipe_probe -- <chromium.exe> <profile-dir>
//! ```

#![cfg(windows)]

use windows::core::PWSTR;
use windows::Win32::System::Threading::{
    CreateProcessW, TerminateProcess, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW,
};

fn main() {
    let mut args = std::env::args().skip(1);
    let exe = args
        .next()
        .expect("usage: cdp_pipe_probe <chromium.exe> <profile-dir>");
    let profile = args
        .next()
        .expect("usage: cdp_pipe_probe <chromium.exe> <profile-dir>");

    let (pipe, launch) = unsafe { stt_platform::prepare_devtools_pipe(std::ptr::null()) }
        .expect("prepare_devtools_pipe");

    let cmdline = format!(
        "\"{exe}\" --remote-debugging-pipe --headless=new --no-first-run \
         --no-default-browser-check --disable-gpu \"--user-data-dir={profile}\" \
         \"data:text/html,<title>probe</title><h1>hi</h1>\""
    );
    let mut cmd: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();

    let mut pi = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            None,
            PWSTR(cmd.as_mut_ptr()),
            None,
            None,
            true,
            PROCESS_CREATION_FLAGS(stt_platform::EXTENDED_STARTUPINFO_PRESENT),
            None,
            None,
            launch.startup_info().cast::<STARTUPINFOW>(),
            &mut pi,
        )
    }
    .expect("CreateProcessW");
    drop(launch);

    let mut session = stt_steamui::CdpPipeSession::new(pipe);

    // 冷启动要点时间, 与 run_store_pipe_loop 的验活逻辑一致.
    let mut targets = Vec::new();
    for _ in 0..20 {
        match session.targets() {
            Ok(t) if !t.is_empty() => {
                targets = t;
                break;
            }
            _ => std::thread::sleep(std::time::Duration::from_millis(500)),
        }
    }
    if targets.is_empty() {
        println!("SESSION_FAIL no targets");
        unsafe { TerminateProcess(pi.hProcess, 0) }.ok();
        return;
    }
    println!("targets={}", targets.len());
    for t in &targets {
        println!("  [{}] {} :: {}", t.kind, t.title, t.url);
    }

    let page = targets.iter().find(|t| t.kind == "page");
    let Some(page) = page else {
        println!("SESSION_FAIL no page target");
        unsafe { TerminateProcess(pi.hProcess, 0) }.ok();
        return;
    };

    match session.attach(&page.target_id) {
        Ok(sid) => {
            println!("attached sessionId={sid}");
            match session.eval_value(&sid, "document.title + '/' + (1+1)") {
                Ok(v) => println!("SESSION_OK eval={v}"),
                Err(e) => println!("SESSION_FAIL eval: {e}"),
            }
            session.detach(&sid);
        }
        Err(e) => println!("SESSION_FAIL attach: {e}"),
    }

    unsafe { TerminateProcess(pi.hProcess, 0) }.ok();
}
