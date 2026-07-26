//! steamwebhelper 的 CEF 调试通道选择与命令行改写 (纯字符串).
//!
//! 商店注入靠 CDP, 而 CDP 端点由 steamwebhelper 的命令行决定. 与其在 Steam 根
//! 目录长期留一个 `.cef-enable-remote-debugging` 开着固定 8080, 不如 hook
//! `CreateProcessW` 自己给参数 (ADR 0010).
//!
//! 首选 [`DebugChannel::Pipe`]: 走继承的 fd 3/4, **没有端口可扫、没有 URL 可
//! 连**, 只有持有管道的父进程能用. [`DebugChannel::Port`] 是它走不通时的回退 —
//! 端口随机化只是纵深防御, 本机进程照样能读命令行拿到端口.

use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};

/// CEF 未被我们接管时的历史端口 (`.cef-enable-remote-debugging` 时代).
pub const LEGACY_CDP_PORT: u16 = 8080;

const WEBHELPER_EXE: &str = "steamwebhelper.exe";

/// 要从命令行里剥掉的调试相关参数.
///
/// `--remote-allow-origins`: Steam 实机上把它设成 `*`, 等于拆掉 Chromium 111+
/// "默认拒绝一切带 Origin 头的 ws 升级" 这道防线 — 恶意网页就能连上 CDP 驱动
/// Steam. 我们自己的 ws 客户端不发 Origin, 剥掉它只挡浏览器, 不影响注入.
const STRIP_PREFIXES: &[&str] = &["--remote-debugging-", "--remote-allow-origins"];

static SESSION_PORT: AtomicU16 = AtomicU16::new(0);
static SESSION_PORT_LIVE: AtomicBool = AtomicBool::new(false);

/// 本会话给 webhelper 分配的调试端口; 0 表示尚未分配 (hook 没装上).
pub fn cef_debug_port() -> u16 {
    SESSION_PORT.load(Ordering::SeqCst)
}

/// 会话端口真的写进了某次 webhelper 的命令行.
///
/// 分配端口 ≠ 端口会被监听: hook 若没赶上 (或压根没挂到发起调用的模块),
/// webhelper 用的还是它自己的参数. 在这个标记置位前, CDP 必须去连 8080,
/// 否则就是对着一个没人监听的随机端口空转 — 实测过的失败形态.
pub fn mark_session_port_live() {
    SESSION_PORT_LIVE.store(true, Ordering::SeqCst);
}

/// 会话端口是否已确认可用.
pub fn session_port_live() -> bool {
    SESSION_PORT_LIVE.load(Ordering::SeqCst)
}

/// 让系统挑一个空闲高位端口并记下来. 立刻释放, 由 CEF 去真正绑定.
///
/// 释放到 CEF 绑定之间有竞态窗口, 但这只影响可用性 (端口被抢就连不上),
/// 不影响正确性; 换来的是不必自己造随机数, 也基本不会撞到在用端口.
pub fn alloc_cef_debug_port() -> u16 {
    let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
        return 0;
    };
    let Ok(addr) = listener.local_addr() else {
        return 0;
    };
    let port = addr.port();
    drop(listener);
    SESSION_PORT.store(port, Ordering::SeqCst);
    port
}

/// CDP 连接目标: 确认改写成功才用本会话端口, 否则退回 8080
/// (兼容已在跑的 webhelper / 用户自留的 `.cef-enable-remote-debugging`).
pub fn cdp_host_port() -> String {
    if session_port_live() {
        host_port_for(cef_debug_port())
    } else {
        host_port_for(0)
    }
}

fn host_port_for(port: u16) -> String {
    match port {
        0 => format!("127.0.0.1:{LEGACY_CDP_PORT}"),
        p => format!("127.0.0.1:{p}"),
    }
}

/// 这次 `CreateProcessW` 是不是在拉起 webhelper 的**主**进程.
///
/// `--type=` 说明是 CEF 自己 fork 的 renderer/gpu/utility: 那些不该加调试参数
/// (而且由 webhelper 自己拉起, 本不该经过 steam.exe 的 hook).
pub fn is_webhelper_launch(app_name: Option<&str>, cmdline: Option<&str>) -> bool {
    if cmdline.is_some_and(|c| c.contains("--type=")) {
        return false;
    }
    if app_name.is_some_and(exe_is_webhelper) {
        return true;
    }
    // 没给 lpApplicationName 时 argv[0] 才是 exe; 只看它, 免得参数里提一嘴就命中.
    cmdline
        .and_then(|c| split_args(c).into_iter().next())
        .is_some_and(exe_is_webhelper)
}

fn exe_is_webhelper(path: &str) -> bool {
    path.trim_matches('"')
        .rsplit(['\\', '/'])
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case(WEBHELPER_EXE))
}

/// webhelper 该开哪种调试通道.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugChannel {
    /// 完全不开 — `catalog_add` 关掉时如此.
    None,
    /// 继承的 fd 3/4, **不开任何端口** (ADR 0010 B 档).
    Pipe,
    /// 本会话随机高位端口; pipe 走不通时的回退.
    Port(u16),
}

/// 剥掉调试相关参数再按指定通道重挂.
///
/// 先剥后加所以幂等: 重复调用 (或 Steam 自己已经加过) 都不会叠参数.
/// `STRIP_PREFIXES` 里的 `--remote-debugging-` 同时覆盖 `-port`/`-pipe`,
/// 所以切换通道时不会两种参数并存.
pub fn rewrite_webhelper_cmdline(cmdline: &str, channel: DebugChannel) -> String {
    let mut parts: Vec<&str> = split_args(cmdline)
        .into_iter()
        .filter(|tok| !is_stripped_flag(tok))
        .collect();
    let injected;
    match channel {
        DebugChannel::None => {}
        DebugChannel::Pipe => parts.push("--remote-debugging-pipe"),
        DebugChannel::Port(port) => {
            injected = format!("--remote-debugging-port={port}");
            parts.push(&injected);
            parts.push("--remote-debugging-address=127.0.0.1");
        }
    }
    parts.join(" ")
}

fn is_stripped_flag(token: &str) -> bool {
    let bare = token.trim_matches('"');
    STRIP_PREFIXES.iter().any(|p| bare.starts_with(p))
}

/// 按空白切 token, 引号内的空白不算分隔; 返回原文切片 (保留引号).
fn split_args(cmd: &str) -> Vec<&str> {
    let bytes = cmd.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        let mut in_quote = false;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => in_quote = !in_quote,
                b' ' | b'\t' if !in_quote => break,
                _ => {}
            }
            i += 1;
        }
        // 只在 ASCII 空白/引号处切, 多字节字符不会被切断.
        out.push(&cmd[start..i]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEBHELPER: &str =
        r#""E:\Steam\bin\cef\cef.win64\steamwebhelper.exe" -lang=zh --disable-quick-menu"#;

    #[test]
    fn detects_webhelper_from_application_name() {
        assert!(is_webhelper_launch(
            Some(r"E:\Steam\bin\cef\cef.win64\steamwebhelper.exe"),
            None
        ));
    }

    #[test]
    fn detects_webhelper_from_quoted_argv0() {
        assert!(is_webhelper_launch(None, Some(WEBHELPER)));
    }

    #[test]
    fn ignores_cef_child_processes() {
        // renderer/gpu/utility 也叫 steamwebhelper.exe, 但不该加调试参数.
        let child = r#""steamwebhelper.exe" --type=renderer --lang=zh"#;
        assert!(!is_webhelper_launch(None, Some(child)));
    }

    #[test]
    fn ignores_other_executables() {
        assert!(!is_webhelper_launch(
            None,
            Some(r#""E:\Steam\steam.exe" -silent"#)
        ));
    }

    #[test]
    fn does_not_match_webhelper_mentioned_only_in_args() {
        let cmd = r#""E:\Steam\steam.exe" --log=steamwebhelper.exe"#;
        assert!(!is_webhelper_launch(None, Some(cmd)));
    }

    #[test]
    fn pipe_channel_opens_no_port() {
        let cmd = r#""wh.exe" --remote-debugging-port=8080 --lang=zh"#;
        let out = rewrite_webhelper_cmdline(cmd, DebugChannel::Pipe);
        assert!(out.contains("--remote-debugging-pipe"), "{out}");
        assert!(!out.contains("--remote-debugging-port"), "{out}");
        assert!(!out.contains("8080"), "{out}");
    }

    #[test]
    fn pipe_channel_keeps_other_arguments() {
        let out = rewrite_webhelper_cmdline(WEBHELPER, DebugChannel::Pipe);
        assert!(out.contains("--disable-quick-menu"), "{out}");
    }

    #[test]
    fn pipe_rewrite_is_idempotent() {
        let once = rewrite_webhelper_cmdline(WEBHELPER, DebugChannel::Pipe);
        assert_eq!(rewrite_webhelper_cmdline(&once, DebugChannel::Pipe), once);
    }

    #[test]
    fn switching_channels_never_leaves_both_flags() {
        // 端口模式的产物再按 pipe 改写, 不能两种参数并存.
        let ported = rewrite_webhelper_cmdline(WEBHELPER, DebugChannel::Port(51234));
        let piped = rewrite_webhelper_cmdline(&ported, DebugChannel::Pipe);
        assert!(!piped.contains("--remote-debugging-port"), "{piped}");
        assert_eq!(
            piped.matches("--remote-debugging-pipe").count(),
            1,
            "{piped}"
        );
    }

    #[test]
    fn pipe_channel_strips_wildcard_origin_too() {
        let cmd = r#""wh.exe" "--remote-allow-origins=*" --lang=zh"#;
        let out = rewrite_webhelper_cmdline(cmd, DebugChannel::Pipe);
        assert!(!out.contains("remote-allow-origins"), "{out}");
    }

    #[test]
    fn injects_session_port() {
        let out = rewrite_webhelper_cmdline(WEBHELPER, DebugChannel::Port(51234));
        assert!(out.contains("--remote-debugging-port=51234"), "{out}");
    }

    #[test]
    fn binds_debug_endpoint_to_loopback() {
        let out = rewrite_webhelper_cmdline(WEBHELPER, DebugChannel::Port(51234));
        assert!(
            out.contains("--remote-debugging-address=127.0.0.1"),
            "{out}"
        );
    }

    #[test]
    fn keeps_original_arguments() {
        let out = rewrite_webhelper_cmdline(WEBHELPER, DebugChannel::Port(51234));
        assert!(out.contains("--disable-quick-menu"), "{out}");
    }

    #[test]
    fn keeps_quoted_argv0_intact() {
        let out = rewrite_webhelper_cmdline(WEBHELPER, DebugChannel::Port(51234));
        assert!(
            out.starts_with(r#""E:\Steam\bin\cef\cef.win64\steamwebhelper.exe""#),
            "{out}"
        );
    }

    #[test]
    fn replaces_steam_own_debug_port() {
        let cmd = r#""wh.exe" --remote-debugging-port=8080 --lang=zh"#;
        let out = rewrite_webhelper_cmdline(cmd, DebugChannel::Port(51234));
        assert!(!out.contains("8080"), "{out}");
    }

    #[test]
    fn strips_quoted_debug_flags() {
        // 实机命令行里 Steam 给这个参数带了引号.
        let cmd = r#""wh.exe" "--remote-debugging-address=127.0.0.1" --lang=zh"#;
        let out = rewrite_webhelper_cmdline(cmd, DebugChannel::Port(51234));
        assert_eq!(
            out.matches("--remote-debugging-address").count(),
            1,
            "{out}"
        );
    }

    #[test]
    fn strips_steam_wildcard_allow_origins() {
        // 实机上 Steam 带 --remote-allow-origins=*, 等于拆掉 CEF 的 Origin 防线.
        let cmd = r#""wh.exe" "--remote-allow-origins=*" --lang=zh"#;
        let out = rewrite_webhelper_cmdline(cmd, DebugChannel::Port(51234));
        assert!(!out.contains("remote-allow-origins"), "{out}");
    }

    #[test]
    fn rewrite_is_idempotent() {
        let once = rewrite_webhelper_cmdline(WEBHELPER, DebugChannel::Port(51234));
        let twice = rewrite_webhelper_cmdline(&once, DebugChannel::Port(51234));
        assert_eq!(once, twice);
    }

    #[test]
    fn zero_port_strips_without_injecting() {
        // 工具关掉: webhelper 完全不开调试端点.
        let cmd = r#""wh.exe" --remote-debugging-port=8080 --lang=zh"#;
        let out = rewrite_webhelper_cmdline(cmd, DebugChannel::None);
        assert!(!out.contains("--remote-debugging-"), "{out}");
    }

    #[test]
    fn zero_port_keeps_other_arguments() {
        let cmd = r#""wh.exe" --remote-debugging-port=8080 --lang=zh"#;
        let out = rewrite_webhelper_cmdline(cmd, DebugChannel::None);
        assert!(out.contains("--lang=zh"), "{out}");
    }

    #[test]
    fn keeps_quoted_paths_with_spaces_as_one_token() {
        let cmd = r#""E:\Program Files\Steam\steamwebhelper.exe" --lang=zh"#;
        assert_eq!(split_args(cmd).len(), 2);
    }

    #[test]
    fn detects_webhelper_under_path_with_spaces() {
        let cmd = r#""E:\Program Files\Steam\bin\cef\steamwebhelper.exe" --lang=zh"#;
        assert!(is_webhelper_launch(None, Some(cmd)));
    }

    /// 实机采样 (2026-07-26, Steam buildid 1784778118) 的 webhelper 主进程命令行,
    /// 截去无关尾巴. 注意 Steam 自己带了 --remote-allow-origins=*.
    const REAL_CMDLINE: &str = r#""E:\Program Files\Steam\bin\cef\cef.win64\steamwebhelper.exe" -nocrashdialog "-lang=zh_CN" "-cachedir=C:\Users\x\AppData\Local\Steam\htmlcache" "-steampid=31300" "-clientui=E:\Program Files\Steam\clientui" --valve-enable-site-isolation "--remote-allow-origins=*" "--remote-debugging-address=127.0.0.1" "--remote-debugging-port=8080" --enable-smooth-scrolling --disable-quick-menu"#;

    #[test]
    fn real_cmdline_is_recognized_as_webhelper() {
        assert!(is_webhelper_launch(None, Some(REAL_CMDLINE)));
    }

    #[test]
    fn real_cmdline_loses_steam_debug_port() {
        let out = rewrite_webhelper_cmdline(REAL_CMDLINE, DebugChannel::Port(51234));
        assert!(!out.contains("8080"), "{out}");
    }

    #[test]
    fn real_cmdline_loses_wildcard_origin() {
        let out = rewrite_webhelper_cmdline(REAL_CMDLINE, DebugChannel::Port(51234));
        assert!(!out.contains("remote-allow-origins"), "{out}");
    }

    #[test]
    fn real_cmdline_keeps_client_ui_path_with_spaces() {
        let out = rewrite_webhelper_cmdline(REAL_CMDLINE, DebugChannel::Port(51234));
        assert!(
            out.contains(r#""-clientui=E:\Program Files\Steam\clientui""#),
            "{out}"
        );
    }

    #[test]
    fn real_cmdline_keeps_steampid() {
        let out = rewrite_webhelper_cmdline(REAL_CMDLINE, DebugChannel::Port(51234));
        assert!(out.contains(r#""-steampid=31300""#), "{out}");
    }

    #[test]
    fn real_cmdline_rewrite_is_idempotent() {
        let once = rewrite_webhelper_cmdline(REAL_CMDLINE, DebugChannel::Port(51234));
        assert_eq!(
            rewrite_webhelper_cmdline(&once, DebugChannel::Port(51234)),
            once
        );
    }

    #[test]
    fn real_cmdline_keeps_all_other_tokens() {
        // 只该少 2 个 (port/address) + 1 个 allow-origins, 再加回 2 个.
        let before = split_args(REAL_CMDLINE).len();
        let after = split_args(&rewrite_webhelper_cmdline(
            REAL_CMDLINE,
            DebugChannel::Port(51234),
        ))
        .len();
        assert_eq!(after, before - 3 + 2, "token 数对不上");
    }

    #[test]
    fn unallocated_port_falls_back_to_legacy() {
        // 没 hook 到时要能连上已经在跑的 webhelper.
        assert_eq!(host_port_for(0), format!("127.0.0.1:{LEGACY_CDP_PORT}"));
    }

    #[test]
    fn allocated_port_targets_loopback() {
        assert_eq!(host_port_for(51234), "127.0.0.1:51234");
    }
}
