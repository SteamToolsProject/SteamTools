//! 进程内 CEF 远程调试桥: 向商店页注入脚本并取回入库点击.
//!
//! 使用 127.0.0.1:8080 (CEF remote debugging).
//! 开关文件由 loader/host 自动创建, 用户无需手开.
//! 不猜 CEF vtable; 跑在 host 工作线程.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::store_debug::cdp_host_port;
use crate::store_inject::STORE_INJECT_JS;

const DRAIN_JS: &str = r#"(function(){var p=window.__SteamToolsPending||[];window.__SteamToolsPending=[];return p;})()"#;

/// CDP 专用短脚本: 大脚本在 CEF evaluate 上偶发挂起; 短脚本狗粮已验证 near-cart 可挂.
/// `{{PORT}}` 由 host 替换为 click_bridge 端口.
pub const CDP_STORE_INJECT_JS: &str = r##"
(function(){
  window.__SteamToolsPending = window.__SteamToolsPending || [];
  var href = String(location.href||"");
  var m = String(location.pathname||"").match(/\/app\/(\d+)/) || href.match(/\/app\/(\d+)/);
  var appId = m ? m[1] : null;
  if(!appId) return "no-appid";
  // 购买区: 按钮放进本体的 .game_purchase_action_bg, 与「添加至购物车」同一行.
  // 跳过试玩版 (demo_above_purchase) 与捆绑包 ([data-ds-bundleid]) 区块.
  function anchorHost(){
    var games = document.querySelectorAll(".game_area_purchase_game");
    for(var i=0;i<games.length;i++){
      var g = games[i];
      if(String(g.className).indexOf("demo_above_purchase") >= 0) continue;
      if(g.closest && (g.closest(".demo_above_purchase") || g.closest("[data-ds-bundleid]"))) continue;
      var cart = g.querySelector(".btn_addtocart");
      if(cart && cart.parentElement) return cart.parentElement;
      var bg = g.querySelector(".game_purchase_action_bg");
      if(bg) return bg;
    }
    var c = document.querySelector(".btn_addtocart");
    return (c && c.parentElement)
        || document.querySelector(".game_purchase_action_bg")
        || document.querySelector(".game_purchase_action");
  }
  function toFixed(el){
    el.style.position="fixed"; el.style.right="24px"; el.style.bottom="88px";
    el.style.marginLeft="0"; el.style.zIndex="2147483647";
    el.style.boxShadow="0 2px 8px rgba(0,0,0,.45)";
    el.setAttribute("data-stt-fallback","1");
  }
  function toInline(el){
    el.style.position=""; el.style.right=""; el.style.bottom="";
    el.style.marginLeft="2px"; el.style.zIndex=""; el.style.boxShadow="";
    el.removeAttribute("data-stt-fallback");
  }
  var host = anchorHost();
  var old = document.querySelector("[data-stt-store-btn]");
  if(old){
    // 页面刚导航时购买区常未渲染, 先挂兜底; 之后有锚点了再搬过去.
    if(old.getAttribute("data-stt-fallback") && host){
      toInline(old); host.appendChild(old); return "moved "+appId;
    }
    return "already";
  }
  function enqueue(id){
    window.__SteamToolsPending.push({app_id:Number(id),reason:"store_btn",href:href,ts:Date.now()});
    try{ fetch("http://127.0.0.1:{{PORT}}/stt/add?appid="+id,{method:"POST",mode:"no-cors"}).catch(function(){}); }catch(e){}
  }
  // 用 Steam 自己的按钮类, 与「添加至购物车」同一套渐变/字号/圆角 (蓝色区分是我们的).
  var btn=document.createElement("a");
  btn.className="btn_blue_steamui btn_medium";
  btn.setAttribute("role","button");
  btn.setAttribute("data-stt-store-btn","1");
  btn.style.cssText="cursor:pointer;margin-left:2px;";
  var label=document.createElement("span");
  label.textContent="入库";
  btn.appendChild(label);
  btn.addEventListener("click",function(ev){
    try{ev.preventDefault();ev.stopPropagation();}catch(e){}
    enqueue(appId);
    label.textContent="已排队";
    btn.className="btn_grey_steamui btn_medium";
    btn.style.cursor="default";
  });
  if(host){ host.appendChild(btn); return "near-cart "+appId; }
  toFixed(btn);
  (document.body||document.documentElement).appendChild(btn);
  return "fixed "+appId;
})()
"##;

/// 把 click_bridge 端口填进 CDP 短脚本.
pub fn cdp_store_inject_js(port: u16) -> String {
    CDP_STORE_INJECT_JS.replace("{{PORT}}", &port.to_string())
}

/// 一次轮询结果.
#[derive(Debug, Default, Clone)]
pub struct StoreCdpPoll {
    pub cdp_up: bool,
    pub store_pages: usize,
    pub injected: usize,
    pub pending_app_ids: Vec<u32>,
    pub notes: Vec<String>,
}

/// 列出调试目标并注入/取队列 (单次, 可失败).
pub fn poll_store_cdp(host_port: &str, inject_js: &str) -> StoreCdpPoll {
    let mut out = StoreCdpPoll::default();
    let list_url = format!("http://{host_port}/json");
    let body = match http_get(&list_url, Duration::from_secs(2)) {
        Ok(b) => b,
        Err(e) => {
            out.notes.push(format!("cdp_list_err={e}"));
            return out;
        }
    };
    out.cdp_up = true;
    // 去掉 BOM / 空白, 避免 CEF 偶发前缀导致 json 失败.
    let body = body.trim_start_matches('\u{feff}').trim();
    let raw_store_hits = body.matches("store.steampowered.com").count();
    let raw_app_hits = body.matches("/app/").count();
    let targets: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            out.notes.push(format!(
                "cdp_json_err={e} body_len={} store_hits={raw_store_hits} app_hits={raw_app_hits} head={}",
                body.len(),
                body.chars().take(80).collect::<String>().replace('\n', " ")
            ));
            return out;
        }
    };
    let Some(arr) = targets.as_array() else {
        out.notes.push("cdp_json=not_array".into());
        return out;
    };

    // 策略: 不只信 /json 的 url 字段 (有时商店页短暂不在列表 / 字段滞后).
    // 对所有带 ws 的 page 目标尝试短注入; 脚本内自检 /app/id, 非商店页秒退.
    let mut candidates: Vec<(String, String)> = Vec::new();
    let mut sample_urls: Vec<String> = Vec::new();
    for t in arr {
        let typ = t.get("type").and_then(|v| v.as_str()).unwrap_or("page");
        if typ != "page" && typ != "iframe" {
            continue;
        }
        let url = t.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let title = t.get("title").and_then(|v| v.as_str()).unwrap_or("");
        if sample_urls.len() < 8 {
            sample_urls.push(format!(
                "{}||{}",
                title.chars().take(24).collect::<String>(),
                url.chars().take(80).collect::<String>()
            ));
        }
        // 跳过明显无用的菜单空白页, 减负.
        if url.starts_with("about:blank") && title.contains("Menu") {
            continue;
        }
        if url.starts_with("about:blank") && title.contains("Supernav") {
            continue;
        }
        let ws = t
            .get("webSocketDebuggerUrl")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                t.get("id")
                    .and_then(|v| v.as_str())
                    .map(|id| format!("ws://{host_port}/devtools/page/{id}"))
            });
        let Some(ws) = ws else { continue };
        let label = if !url.is_empty() {
            url.to_string()
        } else {
            title.to_string()
        };
        // 优先商店 url/title
        let priority = is_store_app_url(url)
            || title.contains("Steam 上购买")
            || title.contains("on Steam")
            || url.contains("store.steampowered.com");
        if priority {
            candidates.insert(0, (label, ws));
        } else {
            candidates.push((label, ws));
        }
    }
    // 去重 ws
    let mut seen = std::collections::HashSet::new();
    candidates.retain(|(_, ws)| seen.insert(ws.clone()));
    // 最多试 6 个目标, 避免扫菜单拖死
    candidates.truncate(6);

    out.store_pages = candidates
        .iter()
        .filter(|(u, _)| is_store_app_url(u) || u.contains("store.steampowered.com"))
        .count();
    if candidates.is_empty() {
        out.notes.push(format!(
            "cdp_store_pages=0 total={} raw_store_hits={raw_store_hits} raw_app_hits={raw_app_hits} body_len={} samples={}",
            arr.len(),
            body.len(),
            sample_urls.join(" | ")
        ));
        return out;
    }

    for (url, ws_url) in candidates {
        match session_inject_and_drain(&ws_url, inject_js) {
            Ok(res) => {
                out.store_pages = out.store_pages.max(1);
                // 只有真的挂上才算一次注入; 按钮已在时 (already) 不刷日志.
                if res.mounted {
                    out.injected += 1;
                    out.notes.push(format!(
                        "cdp_injected ok url={}",
                        url.chars().take(96).collect::<String>()
                    ));
                }
                for id in res.pending {
                    if !out.pending_app_ids.contains(&id) {
                        out.pending_app_ids.push(id);
                    }
                }
            }
            Err(e) => {
                // no-appid 类失败不刷屏; 连接错误记一条
                let es = e.to_string();
                if !es.contains("no-appid") {
                    out.notes.push(format!(
                        "cdp_page_err url={} {es}",
                        url.chars().take(72).collect::<String>()
                    ));
                }
            }
        }
    }
    // 按钮已在 (already) 不算异常; 只有一个商店页都没摸到才记这条.
    if out.store_pages == 0 && out.injected == 0 && out.notes.is_empty() {
        out.notes.push(format!(
            "cdp_store_pages=0 total={} raw_store_hits={raw_store_hits} tried_candidates samples={}",
            arr.len(),
            sample_urls.join(" | ")
        ));
    }
    out
}

/// 默认脚本 + 默认端口的一次轮询.
pub fn poll_store_cdp_default() -> StoreCdpPoll {
    poll_store_cdp(&cdp_host_port(), STORE_INJECT_JS)
}

/// 后台循环: 注入 + 回调 app_id (pending 队列兜底; 点击优先走 click_bridge fetch).
pub fn run_store_cdp_loop(
    poll_every: Duration,
    on_app: impl FnMut(u32),
    on_log: impl FnMut(String),
) {
    run_store_cdp_loop_with_js(poll_every, || STORE_INJECT_JS.to_string(), on_app, on_log);
}

/// 同上, 但每次轮询用 `make_js()` 生成脚本 (可带 click_bridge 端口).
pub fn run_store_cdp_loop_with_js(
    poll_every: Duration,
    mut make_js: impl FnMut() -> String,
    mut on_app: impl FnMut(u32),
    mut on_log: impl FnMut(String),
) {
    let mut last_down_log = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .unwrap_or_else(Instant::now);
    let mut last_up = false;
    let mut last_pages: usize = 0;
    let mut last_zero_log = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .unwrap_or_else(Instant::now);
    loop {
        let js = make_js();
        // 单次轮询 panic 不能弄死整条桥: 之前 ws 握手 panic 让线程静默退出,
        // 表现就是日志停在 store_pages=0 且按钮永远挂不上.
        let r = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            poll_store_cdp(&cdp_host_port(), &js)
        })) {
            Ok(r) => r,
            Err(_) => {
                on_log("catalog_add=store_cdp poll_panic (bridge kept alive)".into());
                std::thread::sleep(poll_every);
                continue;
            }
        };
        if !r.cdp_up {
            if last_down_log.elapsed() >= Duration::from_secs(15) {
                let detail = r
                    .notes
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "cdp unreachable".into());
                on_log(format!("catalog_add=store_cdp down {detail}"));
                last_down_log = Instant::now();
            }
            last_up = false;
        } else {
            if !last_up || r.store_pages != last_pages || r.injected > 0 {
                on_log(format!(
                    "catalog_add=store_cdp up store_pages={} injected={}",
                    r.store_pages, r.injected
                ));
                last_up = true;
                last_pages = r.store_pages;
            }
            for note in &r.notes {
                if note.starts_with("cdp_page_err")
                    || note.starts_with("cdp_injected")
                    || note.starts_with("cdp_json_err")
                    || note.starts_with("cdp_store_hit")
                {
                    on_log(format!("catalog_add={note}"));
                } else if note.starts_with("cdp_store_pages=0")
                    && last_zero_log.elapsed() >= Duration::from_secs(10)
                {
                    on_log(format!("catalog_add={note}"));
                    last_zero_log = Instant::now();
                }
            }
            for app_id in r.pending_app_ids {
                on_app(app_id);
            }
        }
        std::thread::sleep(poll_every);
    }
}

fn is_store_app_url(url: &str) -> bool {
    // 商店 CEF 页: 只要 store.steampowered.com 就注入.
    // 脚本内再解析 /app/<id>; 过严的 /app/ 过滤会漏掉 SPA/重定向中间态.
    if url.contains("agecheck") {
        return false;
    }
    url.contains("store.steampowered.com")
}

/// 一次页面注入的结果.
struct InjectOutcome {
    /// 本轮真的挂上/搬动了按钮 (脚本返回 already 时为 false).
    mounted: bool,
    pending: Vec<u32>,
}

fn session_inject_and_drain(ws_url: &str, inject_js: &str) -> Result<InjectOutcome, String> {
    // 单页超时, 避免 CEF 无响应卡死整条轮询线程.
    let mut ws = WsClient::connect(ws_url, Duration::from_millis(1500))?;
    // 先看是不是商店 app 页, 不是则秒退 (省时间).
    let probe = ws.eval_value(
        r#"(function(){var h=String(location.href||"");var p=String(location.pathname||"");return /store\.steampowered\.com/.test(h)&&/\/app\/\d+/.test(p||h);})()"#,
    );
    match probe {
        Ok(v) if v.as_bool() == Some(true) => {}
        Ok(_) => return Err("no-appid".into()),
        Err(e) => return Err(format!("probe: {e}")),
    }
    let res = ws
        .eval_value(inject_js)
        .map_err(|e| format!("eval inject: {e}"))?;
    // 短脚本返回 near-cart/moved/fixed/already/no-appid; 大脚本无返回值 (Null).
    let mounted = res.as_str().is_none_or(|s| !s.starts_with("already"));
    let pending = match ws.eval_value(DRAIN_JS) {
        Ok(pending) => parse_pending_app_ids(&pending),
        Err(_) => Vec::new(),
    };
    Ok(InjectOutcome { mounted, pending })
}

fn parse_pending_app_ids(v: &Value) -> Vec<u32> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    // 负数 / 越界 / 0 都不是合法 app_id, 直接丢掉.
    arr.iter()
        .filter_map(|item| item.get("app_id")?.as_u64())
        .filter_map(|id| u32::try_from(id).ok())
        .filter(|&id| id > 0)
        .collect()
}

fn http_get(url: &str, timeout: Duration) -> Result<String, String> {
    // 仅支持 http://host:port/path
    // CEF 常不关连接; 有 Content-Length 按长度收; 否则读到可解析的 JSON 数组为止.
    let url = url
        .strip_prefix("http://")
        .ok_or_else(|| "only http".to_string())?;
    let (hostport, path) = url
        .split_once('/')
        .map(|(h, p)| (h, format!("/{p}")))
        .unwrap_or((url, "/".into()));
    let mut stream =
        TcpStream::connect(hostport).map_err(|e| format!("connect {hostport}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_millis(400)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\nAccept: */*\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| e.to_string())?;

    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let deadline = Instant::now() + timeout;
    let mut header_end: Option<usize> = None;
    let mut content_len: Option<usize> = None;

    loop {
        if Instant::now() >= deadline {
            break;
        }
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if header_end.is_none() {
                    if let Some(pos) = find_header_end(&buf) {
                        header_end = Some(pos);
                        let head = String::from_utf8_lossy(&buf[..pos]);
                        content_len = parse_content_length(&head);
                    }
                }
                if let (Some(he), Some(cl)) = (header_end, content_len) {
                    if buf.len().saturating_sub(he) >= cl {
                        break;
                    }
                }
                // 无 CL: 尝试把 body 当 JSON 数组解析, 成功即停.
                if let Some(he) = header_end {
                    if content_len.is_none() {
                        let body = &buf[he..];
                        if json_array_complete(body) {
                            break;
                        }
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // 短读超时: 若 body 已是完整 JSON 则结束, 否则继续直到总 deadline.
                if let Some(he) = header_end {
                    let body = &buf[he..];
                    if json_array_complete(body) {
                        break;
                    }
                    if let Some(cl) = content_len {
                        if body.len() >= cl {
                            break;
                        }
                    }
                }
                continue;
            }
            Err(e) => return Err(format!("http read: {e}")),
        }
    }

    let he = header_end.ok_or_else(|| format!("no http headers ({} bytes)", buf.len()))?;
    let mut body = buf[he..].to_vec();
    if let Some(cl) = content_len {
        if body.len() > cl {
            body.truncate(cl);
        } else if body.len() < cl {
            return Err(format!(
                "short body got={} want={cl} total_buf={}",
                body.len(),
                buf.len()
            ));
        }
    }
    let body = String::from_utf8_lossy(&body);
    let body = body.trim_start_matches('\u{feff}').trim();
    if body.is_empty() {
        return Err("empty http body".into());
    }
    Ok(body.to_string())
}

/// 粗判 JSON 数组是否收全 (括号平衡且以 ] 结束).
fn json_array_complete(body: &[u8]) -> bool {
    let b = trim_ascii(body);
    if b.first() != Some(&b'[') || b.last() != Some(&b']') {
        return false;
    }
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for &c in b {
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'[' | b'{' => depth += 1,
            b']' | b'}' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0 && serde_json::from_slice::<Value>(b).is_ok()
}

fn trim_ascii(mut b: &[u8]) -> &[u8] {
    while b.first().is_some_and(u8::is_ascii_whitespace) {
        b = &b[1..];
    }
    while b.last().is_some_and(u8::is_ascii_whitespace) {
        b = &b[..b.len() - 1];
    }
    b
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
}

fn parse_content_length(head: &str) -> Option<usize> {
    for line in head.lines() {
        let line = line.trim();
        if let Some(rest) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            return rest.trim().parse().ok();
        }
    }
    None
}

struct WsClient {
    stream: TcpStream,
    buf: Vec<u8>,
    next_id: u64,
}

impl WsClient {
    fn connect(url: &str, timeout: Duration) -> Result<Self, String> {
        let url = url
            .strip_prefix("ws://")
            .ok_or_else(|| "only ws://".to_string())?;
        let (hostport, path) = url
            .split_once('/')
            .map(|(h, p)| (h, format!("/{p}")))
            .unwrap_or((url, "/".into()));
        let mut stream = TcpStream::connect(hostport).map_err(|e| format!("ws connect: {e}"))?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|e| e.to_string())?;
        let key = base64_16_rand();
        let req = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {hostport}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
        );
        stream
            .write_all(req.as_bytes())
            .map_err(|e| e.to_string())?;
        let mut header = Vec::new();
        let mut tmp = [0u8; 1];
        loop {
            let n = stream.read(&mut tmp).map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("ws handshake eof".into());
            }
            header.push(tmp[0]);
            if header.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            if header.len() > 8192 {
                return Err("ws handshake too large".into());
            }
        }
        let hs = String::from_utf8_lossy(&header);
        if !hs.contains("101") {
            // 按字符截断: 按字节切可能落在 UTF-8 中间直接 panic.
            let head: String = hs.chars().take(200).collect();
            return Err(format!("ws handshake fail: {head}"));
        }
        Ok(Self {
            stream,
            buf: Vec::new(),
            next_id: 0,
        })
    }

    fn call(&mut self, method: &str, params: Option<Value>) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let mut msg = json!({"id": id, "method": method});
        if let Some(p) = params {
            msg["params"] = p;
        }
        self.send_text(&msg.to_string())?;
        loop {
            let raw = self.recv_text()?;
            let v: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
            if v.get("id").and_then(|x| x.as_u64()) == Some(id) {
                if let Some(err) = v.get("error") {
                    return Err(format!("cdp error: {err}"));
                }
                return Ok(v.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    fn eval_value(&mut self, expression: &str) -> Result<Value, String> {
        // awaitPromise 必须 false: 注入脚本不是 Promise, true 会在 CEF 上一直挂起,
        // 导致轮询卡死, host.log 永远停在 store_pages=0.
        let result = self.call(
            "Runtime.evaluate",
            Some(json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": false,
            })),
        )?;
        let r = result.get("result").cloned().unwrap_or(Value::Null);
        if r.get("subtype").and_then(|x| x.as_str()) == Some("error") {
            return Err(format!("js error: {r}"));
        }
        Ok(r.get("value").cloned().unwrap_or(Value::Null))
    }

    fn send_text(&mut self, text: &str) -> Result<(), String> {
        let payload = text.as_bytes();
        // 非密码学随机即可; 每帧换一个即可满足 RFC 6455.
        let t = WS_KEY_SEQ.fetch_add(1, Ordering::Relaxed) as u32 ^ 0x5a5a_1234;
        let mask = t.to_le_bytes();
        let mut frame = Vec::with_capacity(2 + 8 + 4 + payload.len());
        frame.push(0x81); // fin + text
        let n = payload.len();
        if n < 126 {
            frame.push(0x80 | (n as u8));
        } else if n < 65536 {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        for (i, b) in payload.iter().enumerate() {
            frame.push(b ^ mask[i % 4]);
        }
        self.stream.write_all(&frame).map_err(|e| e.to_string())
    }

    fn recv_text(&mut self) -> Result<String, String> {
        loop {
            while self.buf.len() < 2 {
                self.read_more()?;
            }
            let b1 = self.buf[0];
            let b2 = self.buf[1];
            let opcode = b1 & 0x0f;
            let masked = (b2 & 0x80) != 0;
            let mut len = (b2 & 0x7f) as usize;
            let mut off = 2;
            if len == 126 {
                while self.buf.len() < 4 {
                    self.read_more()?;
                }
                len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
                off = 4;
            } else if len == 127 {
                while self.buf.len() < 10 {
                    self.read_more()?;
                }
                len = u64::from_be_bytes(self.buf[2..10].try_into().unwrap()) as usize;
                off = 10;
            }
            let mask_len = if masked { 4 } else { 0 };
            while self.buf.len() < off + mask_len + len {
                self.read_more()?;
            }
            let mask = if masked {
                let m = [
                    self.buf[off],
                    self.buf[off + 1],
                    self.buf[off + 2],
                    self.buf[off + 3],
                ];
                off += 4;
                m
            } else {
                [0u8; 4]
            };
            let mut payload = self.buf[off..off + len].to_vec();
            self.buf.drain(..off + len);
            if masked {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i % 4];
                }
            }
            match opcode {
                0x1 => return String::from_utf8(payload).map_err(|e| e.to_string()),
                0x8 => return Err("ws closed".into()),
                0x9 => {
                    // pong
                    let mut frame = vec![0x8a];
                    if payload.len() < 126 {
                        frame.push(0x80 | payload.len() as u8);
                    } else {
                        return Err("ping too large".into());
                    }
                    let mask = [1u8, 2, 3, 4];
                    frame.extend_from_slice(&mask);
                    for (i, b) in payload.iter().enumerate() {
                        frame.push(b ^ mask[i % 4]);
                    }
                    self.stream.write_all(&frame).map_err(|e| e.to_string())?;
                }
                _ => {}
            }
        }
    }

    fn read_more(&mut self) -> Result<(), String> {
        let mut tmp = [0u8; 4096];
        let n = self.stream.read(&mut tmp).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("ws eof".into());
        }
        self.buf.extend_from_slice(&tmp[..n]);
        Ok(())
    }
}

/// ws key 计数器: 同一秒内多次握手也不重复.
static WS_KEY_SEQ: AtomicU64 = AtomicU64::new(0);

/// Sec-WebSocket-Key 用的 16 字节. 非密码学, CEF 只看长度与 base64.
fn base64_16_rand() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = WS_KEY_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut raw = [0u8; 16];
    raw[..8].copy_from_slice(&nanos.to_le_bytes());
    raw[8..].copy_from_slice(&seq.to_le_bytes());
    base64_encode(&raw)
}

fn base64_encode(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i];
        let b1 = if i + 1 < data.len() { data[i + 1] } else { 0 };
        let b2 = if i + 2 < data.len() { data[i + 2] } else { 0 };
        out.push(T[(b0 >> 2) as usize] as char);
        out.push(T[(((b0 & 3) << 4) | (b1 >> 4)) as usize] as char);
        if i + 1 < data.len() {
            out.push(T[(((b1 & 0xf) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if i + 2 < data.len() {
            out.push(T[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_url_filter() {
        assert!(is_store_app_url(
            "https://store.steampowered.com/app/3240220/GTA/"
        ));
        assert!(is_store_app_url("https://store.steampowered.com/"));
        assert!(!is_store_app_url(
            "https://store.steampowered.com/agecheck/app/3240220/"
        ));
        assert!(!is_store_app_url("https://steamloopback.host/index.html"));
    }

    #[test]
    fn cdp_js_has_port_placeholder() {
        let s = cdp_store_inject_js(12345);
        assert!(s.contains("12345"));
        assert!(s.contains("data-stt-store-btn"));
        assert!(!s.contains("{{PORT}}"));
        // 兜底按钮要能在购买区渲染好之后搬回购物车旁.
        assert!(s.contains("data-stt-fallback"));
        assert!(s.contains("\"moved \""));
        assert!(s.contains("game_purchase_action_bg"));
        // 与「添加至购物车」同一套 Steam 按钮类.
        assert!(s.contains("btn_blue_steamui btn_medium"));
        assert!(s.contains("btn_grey_steamui btn_medium"));
        // 试玩版/捆绑包区块不能抢锚点; 间距与原生同为 2px.
        assert!(s.contains("demo_above_purchase"));
        assert!(s.contains("data-ds-bundleid"));
        assert!(s.contains("margin-left:2px"));
    }

    #[test]
    fn ws_key_is_16_bytes_and_varies() {
        // 曾经这里 copy_from_slice 长度不匹配直接 panic, 把整条 CDP 线程带走.
        let a = base64_16_rand();
        let b = base64_16_rand();
        assert_eq!(a.len(), 24, "16 字节 base64 应为 24 字符");
        assert!(a.ends_with('='));
        assert_ne!(a, b);
    }

    #[test]
    fn parse_pending() {
        let v = json!([{"app_id": 570, "reason": "store_btn"}, {"app_id": 0}]);
        assert_eq!(parse_pending_app_ids(&v), vec![570]);
    }
}
