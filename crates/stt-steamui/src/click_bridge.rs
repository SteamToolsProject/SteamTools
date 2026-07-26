//! 本机 HTTP 桥: 商店页按钮把 app_id POST 过来 (不依赖 CEF 调试端点).
//!
//! 监听 127.0.0.1 随机端口; 注入脚本里写死端口与会话 token.
//!
//! # 为什么需要 token
//!
//! 光靠"端口随机 + 只绑 loopback"挡不住恶意网页: 页面可以拿 `<img>` 对上万个
//! 高位端口盲喷, 且**不需要读到响应** — 副作用在猜中的那个端口上自然发生
//! (经典 CSRF). CORS 头也管不了这个: `Access-Control-Allow-Origin` 只决定
//! 攻击者能否读响应, 请求照发.
//!
//! 真正的防线是猜不到的 token. 拿不到系统随机数就干脆不启动这座桥 (fail closed),
//! 绝不退化成固定或可预测的 token.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

/// 会话 token 长度; 128 位, 盲猜不现实.
const TOKEN_BYTES: usize = 16;
/// 只认这一条路径.
const ADD_PATH: &str = "/stt/add";

static BRIDGE_PORT: AtomicU16 = AtomicU16::new(0);
static BRIDGE_STARTED: OnceLock<()> = OnceLock::new();
static BRIDGE_TOKEN: OnceLock<String> = OnceLock::new();

/// 当前桥端口; 0 表示尚未 listen 成功.
pub fn click_bridge_port() -> u16 {
    BRIDGE_PORT.load(Ordering::SeqCst)
}

/// 本会话的桥 token; 桥没起来时为空.
pub fn click_bridge_token() -> &'static str {
    BRIDGE_TOKEN.get().map_or("", String::as_str)
}

/// 启动一次监听线程. `on_app` 在工作线程调用.
///
/// 取不到系统随机数就直接不启动 — 没有 token 的桥等于给本机任何程序留了个
/// 无鉴权入口, 宁可让入库按钮失效.
pub fn ensure_click_bridge(mut on_app: impl FnMut(u32) + Send + 'static) {
    if BRIDGE_STARTED.set(()).is_err() {
        return;
    }
    let Some(token) = stt_platform::random_hex_token(TOKEN_BYTES) else {
        return;
    };
    let _ = BRIDGE_TOKEN.set(token);
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
    let app_id = parse_add_request(req, click_bridge_token());
    // 不回 Access-Control-Allow-Origin: 我方脚本用 no-cors 发请求, 不读响应;
    // 放开只会让别人读到结果, 没有任何用处.
    let status: &[u8] = if app_id.is_some() {
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\nContent-Length: 2\r\n\r\nok"
    } else {
        b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    };
    let _ = stream.write_all(status);
    app_id
}

/// 从请求里取出 app_id; 任何一项不合规都返回 `None`.
///
/// 只认 `POST /stt/add?...` 的请求行, 且 query 里 token 必须匹配.
/// 参数只从请求行的 query 取 — 早先在整个请求里搜 `appid=` 会把 `Referer`
/// 这类头里的同名参数也当成命令.
fn parse_add_request(req: &str, expected_token: &str) -> Option<u32> {
    // 没 token 说明桥没正常起来, 一律拒绝.
    if expected_token.is_empty() {
        return None;
    }
    let request_line = req.split("\r\n").next()?;
    let mut parts = request_line.split(' ');
    if parts.next()? != "POST" {
        return None;
    }
    let target = parts.next()?;
    let (path, query) = target.split_once('?')?;
    if path != ADD_PATH {
        return None;
    }
    if !stt_platform::constant_time_eq(query_param(query, "token")?, expected_token) {
        return None;
    }
    query_param(query, "appid")?.parse().ok()
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

/// 生成带桥端口与会话 token 的注入脚本 (在 STORE_INJECT_JS 基础上改 enqueue).
///
/// 端口或 token 缺一就不接桥: 没有 token 的请求服务端也会拒, 接了只是白发.
pub fn store_inject_js_with_bridge(port: u16, base_js: &str) -> String {
    let token = click_bridge_token();
    if port == 0 || token.is_empty() {
        return base_js.to_string();
    }
    // 在 enqueue 里追加 fetch 到本机桥.
    let patch = format!(
        r##"
(function(){{
  var _p = {port};
  var _t = "{token}";
  var _old = window.__SteamToolsEnqueueHook;
  window.__SteamToolsEnqueueHook = function(appId) {{
    try {{
      fetch("http://127.0.0.1:"+_p+"/stt/add?token="+_t+"&appid="+appId, {{method:"POST", mode:"no-cors"}}).catch(function(){{}});
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

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn post(query: &str) -> String {
        format!("POST /stt/add?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
    }

    #[test]
    fn accepts_well_formed_request() {
        let req = post(&format!("token={TOKEN}&appid=730"));
        assert_eq!(parse_add_request(&req, TOKEN), Some(730));
    }

    #[test]
    fn rejects_wrong_token() {
        let req = post(&format!("token={}&appid=730", "f".repeat(32)));
        assert_eq!(parse_add_request(&req, TOKEN), None);
    }

    #[test]
    fn rejects_missing_token() {
        assert_eq!(parse_add_request(&post("appid=730"), TOKEN), None);
    }

    /// 恶意页面用 `<img src=...>` 盲喷端口只能发 GET, 挡在方法这一关.
    #[test]
    fn rejects_get_navigation() {
        let req = format!("GET /stt/add?token={TOKEN}&appid=730 HTTP/1.1\r\n\r\n");
        assert_eq!(parse_add_request(&req, TOKEN), None);
    }

    #[test]
    fn rejects_other_paths() {
        let req = format!("POST /evil?token={TOKEN}&appid=730 HTTP/1.1\r\n\r\n");
        assert_eq!(parse_add_request(&req, TOKEN), None);
    }

    /// 早先在整个请求里搜 `appid=`, 请求头里带同名参数就会被当成命令.
    #[test]
    fn ignores_parameters_outside_the_request_line() {
        let req = format!(
            "POST /stt/add?token={TOKEN} HTTP/1.1\r\nReferer: http://evil.test/?appid=999\r\n\r\n"
        );
        assert_eq!(parse_add_request(&req, TOKEN), None);
    }

    #[test]
    fn ignores_token_supplied_only_in_a_header() {
        let req = format!("POST /stt/add?appid=730 HTTP/1.1\r\nX-Token: {TOKEN}\r\n\r\n");
        assert_eq!(parse_add_request(&req, TOKEN), None);
    }

    /// 桥没起来时 token 为空; 此时任何请求都不能通过.
    #[test]
    fn rejects_everything_when_no_token_is_configured() {
        assert_eq!(parse_add_request(&post("token=&appid=730"), ""), None);
    }

    #[test]
    fn rejects_non_numeric_app_id() {
        let req = post(&format!("token={TOKEN}&appid=../../etc"));
        assert_eq!(parse_add_request(&req, TOKEN), None);
    }

    #[test]
    fn rejects_request_without_query() {
        assert_eq!(
            parse_add_request("POST /stt/add HTTP/1.1\r\n\r\n", TOKEN),
            None
        );
    }

    #[test]
    fn accepts_parameters_in_any_order() {
        let req = post(&format!("appid=440&token={TOKEN}"));
        assert_eq!(parse_add_request(&req, TOKEN), Some(440));
    }

    #[test]
    fn inject_js_without_a_bridge_is_left_untouched() {
        let base = "window.__SteamToolsPending.push({\n      app_id: 1\n    });\n";
        // 端口为 0 = 桥没起来.
        assert_eq!(store_inject_js_with_bridge(0, base), base);
    }
}
