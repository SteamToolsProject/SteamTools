//! 本机 HTTP 桥: 商店页按钮把 app_id POST 过来 (不依赖 CEF 8080).
//!
//! 监听 127.0.0.1 随机端口; 注入脚本里写死该端口.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

static BRIDGE_PORT: AtomicU16 = AtomicU16::new(0);
static BRIDGE_STARTED: OnceLock<()> = OnceLock::new();

/// 当前桥端口; 0 表示尚未 listen 成功.
pub fn click_bridge_port() -> u16 {
    BRIDGE_PORT.load(Ordering::SeqCst)
}

/// 启动一次监听线程. `on_app` 在工作线程调用.
pub fn ensure_click_bridge(mut on_app: impl FnMut(u32) + Send + 'static) {
    if BRIDGE_STARTED.set(()).is_err() {
        return;
    }
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(_) => return,
    };
    let port = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(_) => return,
    };
    BRIDGE_PORT.store(port, Ordering::SeqCst);
    let _ = std::thread::Builder::new()
        .name("stt-click-bridge".into())
        .spawn(move || {
            let _ = listener.set_nonblocking(false);
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { continue };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                if let Some(app_id) = handle_conn(&mut stream) {
                    on_app(app_id);
                }
            }
        });
}

fn handle_conn(stream: &mut TcpStream) -> Option<u32> {
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).ok()?;
    if n == 0 {
        return None;
    }
    let req = std::str::from_utf8(&buf[..n]).ok()?;
    // POST /stt/add?appid=123 或 body appid=123
    let mut app_id = None;
    if let Some(rest) = req.split_once("appid=").map(|(_, r)| r) {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        app_id = digits.parse().ok();
    }
    let body = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\nContent-Length: 2\r\n\r\nok";
    let _ = stream.write_all(body);
    app_id
}

/// 生成带桥端口的注入脚本 (在 STORE_INJECT_JS 基础上改 enqueue).
pub fn store_inject_js_with_bridge(port: u16, base_js: &str) -> String {
    if port == 0 {
        return base_js.to_string();
    }
    // 在 enqueue 里追加 fetch 到本机桥.
    let patch = format!(
        r##"
(function(){{
  var _p = {port};
  var _old = window.__SteamToolsEnqueueHook;
  window.__SteamToolsEnqueueHook = function(appId) {{
    try {{
      fetch("http://127.0.0.1:"+_p+"/stt/add?appid="+appId, {{method:"POST", mode:"no-cors"}}).catch(function(){{}});
    }} catch (e) {{}}
    if (typeof _old === "function") try {{ _old(appId); }} catch (e2) {{}}
  }};
}})();
"##
    );
    // 改写 base: enqueue 末尾调 hook
    let mut js = base_js.to_string();
    if let Some(idx) = js.find("window.__SteamToolsPending.push({") {
        // 在 push 块后插入 hook 调用 — 找 push 对象结束
        if let Some(end) = js[idx..].find("});") {
            let at = idx + end + 3;
            js.insert_str(
                at,
                "\n    try { if (window.__SteamToolsEnqueueHook) window.__SteamToolsEnqueueHook(id); } catch (eH) {}\n",
            );
        }
    }
    format!("{patch}\n{js}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_js_mentions_port() {
        let base = "window.__SteamToolsPending.push({\n      app_id: 1\n    });\n";
        let s = store_inject_js_with_bridge(12345, base);
        assert!(s.contains("12345"));
        assert!(s.contains("__SteamToolsEnqueueHook"));
        assert!(s.contains("appid="));
    }
}
