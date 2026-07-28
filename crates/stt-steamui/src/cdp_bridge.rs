//! 进程内 CEF 远程调试桥: 向商店页注入脚本并取回入库点击.
//!
//! 端点由 `store_debug::cdp_host_port()` 给 (本会话端口, 见 ADR 0010);
//! hook 没赶上时回退 8080, 兼容已经在跑的 webhelper.
//! 不猜 CEF vtable; 跑在 host 工作线程.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config_panel::{
    apply_library_menu_drain, library_menu_inject_js, panel_step, EvalTarget, PanelBridge,
    PanelState, ViewRole, LIBRARY_MENU_DRAIN_JS,
};
use crate::store_debug::cdp_host_port;
use crate::store_inject::{app_id_from_store_path, STORE_INJECT_JS};

pub(crate) const DRAIN_JS: &str = r#"(function(){var p=window.__SteamToolsPending||[];window.__SteamToolsPending=[];return p;})()"#;

/// CDP 专用短脚本: 大脚本在 CEF evaluate 上偶发挂起; 短脚本狗粮已验证 near-cart 可挂.
///
/// 点击只入 `window.__SteamToolsPending`, 由 [`DRAIN_JS`] 取走 — 商店页的 CSP
/// `connect-src` 只放行 `127.0.0.1:27060` (Steam 自己占着), 页面发不出到别的
/// 本机端口的请求, 所以不存在"直接回传"这条路.
pub const CDP_STORE_INJECT_JS: &str = r##"
(function(){
  window.__SteamToolsPending = window.__SteamToolsPending || [];
  var href = String(location.href||"");
  var m = String(location.pathname||"").match(/^\/app\/(\d+)(?:\/|$)/);
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
  }
  // 用 Steam 自己的按钮类, 与「添加至购物车」同一套渐变/字号/圆角 (蓝色区分是我们的).
  var btn=document.createElement("a");
  btn.className="btn_blue_steamui btn_medium";
  btn.setAttribute("role","button");
  btn.setAttribute("data-stt-store-btn","1");
  btn.setAttribute("data-stt-app", String(appId));
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

/// CDP 短脚本 (已无占位符待填).
pub fn cdp_store_inject_js() -> String {
    CDP_STORE_INJECT_JS.to_owned()
}

/// 工具关掉时用它替换注入脚本: 把已经挂上的按钮摘掉.
///
/// 返回 `removed` 只会有一轮, 之后一直是 `off` — 与 `already` 一样不算注入,
/// 免得每轮刷一行日志.
pub const STORE_TEARDOWN_JS: &str = r##"
(function(){
  var b=document.querySelectorAll("[data-stt-store-btn]");
  if(!b.length) return "off";
  for(var i=0;i<b.length;i++){ if(b[i].remove) b[i].remove(); }
  return "removed";
})()
"##;

/// 工具关掉时该往商店页发的脚本.
pub fn store_teardown_js() -> String {
    STORE_TEARDOWN_JS.to_owned()
}

/// 把一次入库结果写回商店按钮 (短脚本, 可每轮 evaluate).
///
/// 只改带 `data-stt-store-btn` 且 `data-stt-app` 对得上的按钮; 对不上就不动,
/// 避免把别的 app 页按钮改错. 成功/失败都不再停在「已排队」.
pub fn store_button_result_js(app_id: u32, ok: bool, label: &str) -> String {
    let safe: String = label
        .chars()
        .map(|c| match c {
            '\\' | '"' | '\n' | '\r' | '\u{2028}' | '\u{2029}' => ' ',
            _ => c,
        })
        .take(24)
        .collect();
    let cls = if ok {
        "btn_blue_steamui btn_medium"
    } else {
        "btn_grey_steamui btn_medium"
    };
    format!(
        "(function(){{\n  var id=String({app_id});\n  \
  var nodes=document.querySelectorAll('[data-stt-store-btn]');\n  \
  for(var i=0;i<nodes.length;i++){{\n    var b=nodes[i];\n    \
    var marked=b.getAttribute('data-stt-app');\n    \
    if(marked && marked!==id) continue;\n    \
    b.setAttribute('data-stt-app', id);\n    \
    b.className='{cls}';\n    \
    b.style.cursor='default';\n    \
    var sp=b.querySelector('span');\n    \
    if(sp) sp.textContent='{safe}';\n    \
    else b.textContent='{safe}';\n  }}\n  return 'ok';\n}})()"
    )
}

/// 一次轮询结果.
#[derive(Debug, Default, Clone)]
pub struct StoreCdpPoll {
    pub cdp_up: bool,
    pub store_pages: usize,
    pub injected: usize,
    pub pending_app_ids: Vec<u32>,
    pub notes: Vec<String>,
    /// 本轮摸到的商店 app 页 (端口模式 = ws URL; 管道模式 = target id).
    ///
    /// 入库结果要回写按钮时, 宿主拿这份名单再 eval 一次短脚本.
    pub store_targets: Vec<String>,
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
                if res.removed {
                    out.notes
                        .push(format!("cdp_teardown ok url={}", clip(&url, 96)));
                } else if res.mounted {
                    out.injected += 1;
                    out.notes
                        .push(format!("cdp_injected ok url={}", clip(&url, 96)));
                }
                // 已确认是商店 app 页: 记下 ws, 供入库结果回写按钮.
                if !out.store_targets.contains(&ws_url) {
                    out.store_targets.push(ws_url.clone());
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

/// 后台循环: 注入按钮, 并把点击排下的 app_id 回调出去.
pub fn run_store_cdp_loop(
    poll_every: Duration,
    on_app: &mut dyn FnMut(u32) -> Option<String>,
    on_log: &mut dyn FnMut(String),
) {
    run_store_cdp_loop_with_js(
        poll_every,
        &mut || STORE_INJECT_JS.to_string(),
        on_app,
        on_log,
        None,
    );
}

/// 同上, 但每次轮询用 `make_js()` 现生成脚本, 并可捎带配置页.
///
/// `on_app` 返回值: 可选的短 JS, 在本轮商店页上 evaluate, 用来改按钮文案.
pub fn run_store_cdp_loop_with_js(
    poll_every: Duration,
    make_js: &mut dyn FnMut() -> String,
    on_app: &mut dyn FnMut(u32) -> Option<String>,
    on_log: &mut dyn FnMut(String),
    mut panel: Option<&mut dyn PanelBridge>,
) {
    let mut panel_state = PanelState::default();
    let mut last_down_log = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .unwrap_or_else(Instant::now);
    let mut last_up = false;
    let mut last_pages: usize = 0;
    // "一个商店页都没摸到" 每轮都会复现: 只在刚进入这个状态时记一次.
    // 按时间节流不行 — 没开商店页是常态, 定时重记就是每小时几百行长日志.
    let mut zero_logged = false;
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
            if r.store_pages > 0 {
                zero_logged = false;
            }
            for note in &r.notes {
                if note.starts_with("cdp_page_err")
                    || note.starts_with("cdp_injected")
                    || note.starts_with("cdp_teardown")
                    || note.starts_with("cdp_json_err")
                    || note.starts_with("cdp_store_hit")
                {
                    on_log(format!("catalog_add={note}"));
                } else if note.starts_with("cdp_store_pages=0") && !zero_logged {
                    on_log(format!("catalog_add={note}"));
                    zero_logged = true;
                }
            }
            for app_id in r.pending_app_ids {
                if let Some(js) = on_app(app_id) {
                    push_store_feedback_cdp(&r.store_targets, &js, on_log);
                }
            }
        }
        // 配置页搭同一趟车; 它出问题也不能连累入库那条路.
        if r.cdp_up {
            if let Some(p) = panel.as_mut() {
                let on = p.enabled();
                if panel_state.should_run(on) {
                    let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        poll_panel_cdp(&cdp_host_port(), &mut **p, &mut panel_state, on, on_log);
                    }));
                    if ok.is_err() {
                        on_log("config_ui=poll_panic (bridge kept alive)".into());
                    }
                }
            }
        }
        std::thread::sleep(poll_every);
    }
}

/// 配置页一轮 (端口通道): 找客户端窗口, 补挂入口, 需要时开面板并推快照.
pub(crate) fn poll_panel_cdp(
    host_port: &str,
    bridge: &mut dyn PanelBridge,
    state: &mut PanelState,
    enabled: bool,
    on_log: &mut dyn FnMut(String),
) {
    // 列不出来说明通道这轮不通; 商店那条路已经在报了, 这里不重复刷.
    let Ok(targets) = list_page_targets(host_port) else {
        return;
    };
    state.begin_round();
    for t in &targets {
        if !wants_panel_tick(&t.url, &t.title) {
            continue;
        }
        let mut ws = match WsClient::connect(&t.ws, Duration::from_millis(1500)) {
            Ok(w) => w,
            Err(_) => continue,
        };
        let role = ViewRole {
            enabled,
            hosts_entry: hosts_nav_entry(&t.url, &t.title),
        };
        match panel_step(&t.key, &mut ws, bridge, state, role) {
            Ok(out) => log_panel_step(&out, &t.title, on_log),
            Err(e) => on_log(format!(
                "config_ui=page_err title={} {e}",
                clip(&t.title, 32)
            )),
        }
    }
    // 右键后 Steam 才创建独立 popup; 重新取一次 target, 不必等下一轮.
    let refreshed = if state.pending_menu().is_some() {
        list_page_targets(host_port)
            .ok()
            .filter(|targets| !targets.is_empty())
    } else {
        None
    };
    let menu_targets = refreshed.as_deref().unwrap_or(&targets);
    poll_library_menu_cdp(menu_targets, bridge, state, on_log);
}

fn poll_library_menu_cdp(
    targets: &[PageTarget],
    bridge: &mut dyn PanelBridge,
    state: &mut PanelState,
    on_log: &mut dyn FnMut(String),
) {
    if let Some((key, app_id)) = state
        .active_menu()
        .map(|(key, app_id)| (key.to_owned(), app_id))
    {
        let Some(target) = targets.iter().find(|target| target.key == key) else {
            state.clear_active_menu();
            return;
        };
        match WsClient::connect(&target.ws, Duration::from_millis(1500))
            .and_then(|mut page| page.eval(LIBRARY_MENU_DRAIN_JS))
        {
            Ok(value) => {
                let (alive, dropped) = apply_library_menu_drain(&value, app_id, bridge, state);
                if dropped > 0 {
                    on_log(format!("config_ui=library_menu dropped_actions={dropped}"));
                }
                if !alive {
                    state.clear_active_menu();
                }
            }
            Err(_) => state.clear_active_menu(),
        }
    }

    let Some((source_key, app_id, point)) = state
        .pending_menu()
        .map(|(key, app_id, point)| (key.to_owned(), app_id, point))
    else {
        return;
    };
    if let Some(target) = targets.iter().find(|target| target.key == source_key) {
        let result = WsClient::connect(&target.ws, Duration::from_millis(1500))
            .and_then(|mut page| page.eval(&library_menu_inject_js(app_id, point)));
        if let Ok(value) = result {
            let status = value.get("s").and_then(Value::as_str).unwrap_or("invalid");
            if matches!(status, "injected" | "already") {
                state.activate_menu(&target.key, app_id);
                on_log(format!(
                    "config_ui=library_menu {status} app_id={app_id} target=source"
                ));
                return;
            }
        }
    }
    for target in targets
        .iter()
        .filter(|target| is_popup_menu_target(&target.url, &target.title))
    {
        let result = WsClient::connect(&target.ws, Duration::from_millis(1500))
            .and_then(|mut page| page.eval(&library_menu_inject_js(app_id, None)));
        let Ok(value) = result else {
            continue;
        };
        let status = value.get("s").and_then(Value::as_str).unwrap_or("invalid");
        if matches!(status, "injected" | "already") {
            state.activate_menu(&target.key, app_id);
            on_log(format!(
                "config_ui=library_menu {status} app_id={app_id} title={}",
                clip(&target.title, 32)
            ));
            return;
        }
        if status != "hidden" {
            on_log(format!(
                "config_ui=library_menu waiting state={status} title={}",
                clip(&target.title, 32)
            ));
        }
    }
}

/// 只有状态变了才写日志: 600ms 一轮, 常态一律闭嘴.
pub(crate) fn log_panel_step(
    out: &crate::config_panel::PanelStepOutcome,
    title: &str,
    on_log: &mut dyn FnMut(String),
) {
    if out.tick.just_mounted() || out.tick.state == "removed" {
        on_log(format!(
            "config_ui=entry {} title={}",
            out.tick.state,
            clip(title, 32)
        ));
    }
    if out.asked {
        on_log(format!(
            "config_ui=requested by={} via={}",
            clip(title, 32),
            out.asked_why
        ));
    }
    if out.opened {
        on_log("config_ui=panel opened".into());
    }
    if let Some(app_id) = out.tick.menu_app_id {
        on_log(format!("config_ui=library_menu captured app_id={app_id}"));
    }
    if out.tick.dropped > 0 {
        on_log(format!("config_ui=dropped_intents n={}", out.tick.dropped));
    }
}

/// 目标列表里的一页.
pub(crate) struct PageTarget {
    pub url: String,
    pub title: String,
    pub ws: String,
    /// 跨轮次认页面用的键.
    pub key: String,
}

fn list_page_targets(host_port: &str) -> Result<Vec<PageTarget>, String> {
    let body = http_get(&format!("http://{host_port}/json"), Duration::from_secs(2))?;
    let body = body.trim_start_matches('\u{feff}').trim();
    let targets: Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let arr = targets
        .as_array()
        .ok_or_else(|| "cdp_json=not_array".to_string())?;
    Ok(arr
        .iter()
        .filter_map(|t| {
            let kind = t.get("type").and_then(Value::as_str).unwrap_or("page");
            if kind != "page" && kind != "iframe" {
                return None;
            }
            let id = t.get("id").and_then(Value::as_str).unwrap_or("");
            let ws = t
                .get("webSocketDebuggerUrl")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    (!id.is_empty()).then(|| format!("ws://{host_port}/devtools/page/{id}"))
                })?;
            Some(PageTarget {
                url: t
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                title: t
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                key: if id.is_empty() {
                    ws.clone()
                } else {
                    id.to_owned()
                },
                ws,
            })
        })
        .collect())
}

/// 按字符截断; 按字节切可能落在 UTF-8 中间.
pub(crate) fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 一个调试目标该怎么处理.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetKind {
    /// 商店页: 挂入库按钮.
    Store,
    /// 客户端界面窗口: 挂配置入口.
    ClientUi,
    /// 不碰.
    Skip,
}

/// 只看 url 与标题分流; 具体页面对不对由注入脚本自己再判一次.
pub(crate) fn classify_target(url: &str, title: &str) -> TargetKind {
    if url.contains("store.steampowered.com") || url.contains("steamcommunity.com") {
        return if is_store_app_url(url) {
            TargetKind::Store
        } else {
            TargetKind::Skip
        };
    }
    // 共享 JS 上下文没有可见 DOM; 弹出菜单也不该挂东西.
    if title == "SharedJSContext" || title.contains("Menu") || title.contains("Supernav") {
        return TargetKind::Skip;
    }
    // 客户端窗口的文档是 about:blank?createflags=... 或 data:text/html 的空壳,
    // 界面由共享上下文渲染进去.
    if url.starts_with("about:blank")
        || url.starts_with("data:text/html")
        || url.contains("steamloopback.host")
    {
        return TargetKind::ClientUi;
    }
    TargetKind::Skip
}

/// Steam 的弹出菜单是独立 target. Supernav 菜单不是库右键菜单候选.
pub(crate) fn is_popup_menu_target(url: &str, title: &str) -> bool {
    title.contains("Menu")
        && !title.contains("Supernav")
        && (url.starts_with("about:blank") || url.starts_with("data:text/html"))
}

/// 配置页这一轮要问哪些文档.
///
/// 客户端外壳之外还要带上商店/社区那些网页视图: 它们是独立的 CEF 视图, 被合成在
/// 客户端文档之上, 面板画在下面那层会被整块盖住. 具体画在哪一个由
/// `PanelState::owner` 按"可见且够大"挑.
pub(crate) fn wants_panel_tick(url: &str, title: &str) -> bool {
    if title == "SharedJSContext" || title.contains("Menu") || title.contains("Supernav") {
        return false;
    }
    classify_target(url, title) == TargetKind::ClientUi
        || url.contains("store.steampowered.com")
        || url.contains("steamcommunity.com")
}

/// 这个文档里要不要找导航行挂入口 —— 只有客户端外壳里有那一行.
pub(crate) fn hosts_nav_entry(url: &str, title: &str) -> bool {
    classify_target(url, title) == TargetKind::ClientUi
}

pub(crate) fn is_store_app_url(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    let Some((host, path)) = rest.split_once('/') else {
        return false;
    };
    host == "store.steampowered.com"
        && !path.starts_with("agecheck/")
        && app_id_from_store_path(&format!("/{path}")).is_some()
}

/// 一次页面注入的结果.
struct InjectOutcome {
    /// 本轮真的挂上/搬动了按钮 (脚本返回 already 时为 false).
    mounted: bool,
    /// 本轮是把按钮摘了 (工具被关掉).
    removed: bool,
    pending: Vec<u32>,
}

fn session_inject_and_drain(ws_url: &str, inject_js: &str) -> Result<InjectOutcome, String> {
    // 单页超时, 避免 CEF 无响应卡死整条轮询线程.
    let mut ws = WsClient::connect(ws_url, Duration::from_millis(1500))?;
    // 先看是不是商店 app 页, 不是则秒退 (省时间).
    let probe = ws.eval_value(
        r#"(function(){var h=String(location.href||"");var p=String(location.pathname||"");return /store\.steampowered\.com/.test(h)&&/^\/app\/\d+(?:\/|$)/.test(p);})()"#,
    );
    match probe {
        Ok(v) if v.as_bool() == Some(true) => {}
        Ok(_) => return Err("no-appid".into()),
        Err(e) => return Err(format!("probe: {e}")),
    }
    let res = ws
        .eval_value(inject_js)
        .map_err(|e| format!("eval inject: {e}"))?;
    let mounted = is_mount_news(&res);
    let removed = res.as_str() == Some("removed");
    let pending = match ws.eval_value(DRAIN_JS) {
        Ok(pending) => parse_pending_app_ids(&pending),
        Err(_) => Vec::new(),
    };
    Ok(InjectOutcome {
        mounted,
        removed,
        pending,
    })
}

/// 在已知商店页 ws 上跑一段短脚本 (入库结果回写按钮).
fn session_eval_js(ws_url: &str, js: &str) -> Result<(), String> {
    let mut ws = WsClient::connect(ws_url, Duration::from_millis(1500))?;
    ws.eval_value(js)
        .map_err(|e| format!("eval feedback: {e}"))?;
    Ok(())
}

/// 把反馈脚本推到本轮摸到的商店页; 失败只记 note, 不拖垮轮询.
///
/// `targets` 在端口模式下是完整 `ws://` URL (见 `poll_store_cdp`).
pub fn push_store_feedback_cdp(targets: &[String], js: &str, on_log: &mut dyn FnMut(String)) {
    if js.is_empty() || targets.is_empty() {
        return;
    }
    for ws_url in targets {
        if !ws_url.starts_with("ws://") {
            continue;
        }
        if let Err(e) = session_eval_js(ws_url, js) {
            on_log(format!("catalog_add=feedback_err {e}"));
        }
    }
}

/// 注入脚本的返回值里, 哪些算"这轮真动了页面".
///
/// 挂上/搬动/摘掉算; `already` / `off` / `no-appid` 是常态, 记进日志就是刷屏.
/// 大脚本没有返回值 (Null), 按动了算.
pub(crate) fn is_mount_news(res: &Value) -> bool {
    res.as_str().is_none_or(|s| {
        !(s.starts_with("already") || s.starts_with("off") || s.starts_with("no-appid"))
    })
}

pub(crate) fn parse_pending_app_ids(v: &Value) -> Vec<u32> {
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
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
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
        //
        // userGesture: 没有它 `window.open` 会被当成非用户触发的弹窗直接拦掉
        // (实测 host.log 报 overlay:blocked). 配置页要开成真窗口就得靠这个.
        let result = self.call(
            "Runtime.evaluate",
            Some(json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": false,
                "userGesture": true,
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

impl EvalTarget for WsClient {
    fn eval(&mut self, js: &str) -> Result<Value, String> {
        self.eval_value(js)
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

    /// 客户端窗口的 url 就是这几种空壳; 认错了配置页就没地方挂.
    #[test]
    fn client_windows_are_told_apart_from_web_pages() {
        assert_eq!(
            classify_target("about:blank?createflags=274&minwidth=1010", "Steam"),
            TargetKind::ClientUi
        );
        assert_eq!(
            classify_target(
                "data:text/html,<body></body><!--tracking:x:/library/home-->",
                ""
            ),
            TargetKind::ClientUi
        );
        assert_eq!(
            classify_target("https://store.steampowered.com/app/570/", "Dota"),
            TargetKind::Store
        );
    }

    /// 共享上下文没有可见 DOM, 弹出菜单也不该挂东西.
    #[test]
    fn shared_context_and_popups_are_skipped() {
        assert_eq!(
            classify_target("https://steamloopback.host/index.html", "SharedJSContext"),
            TargetKind::Skip
        );
        assert_eq!(
            classify_target("about:blank", "Supernav Menu"),
            TargetKind::Skip
        );
        assert_eq!(
            classify_target("https://steamcommunity.com/app/570", "Community"),
            TargetKind::Skip
        );
    }

    #[test]
    fn library_popup_target_is_narrower_than_the_general_menu_skip() {
        assert!(is_popup_menu_target("about:blank", "Library Context Menu"));
        assert!(!is_popup_menu_target("about:blank", "Supernav Menu"));
        assert!(!is_popup_menu_target(
            "https://store.steampowered.com/app/570",
            "Library Context Menu"
        ));
    }

    /// 没有 userGesture, CEF 会把 `window.open` 当成广告弹窗拦掉 —— 配置页就退回
    /// 页内浮层, 又会被商店那层 CEF 视图盖住 (实测 overlay:blocked).
    #[test]
    fn evaluate_carries_a_user_gesture() {
        let js = include_str!("cdp_bridge.rs");
        assert_eq!(js.matches("\"userGesture\": true").count(), 1);
    }

    /// already/off 是常态, 记进日志就是每 600ms 刷一行.
    #[test]
    fn only_real_changes_count_as_news() {
        assert!(is_mount_news(&json!("near-cart 570")));
        assert!(is_mount_news(&json!("moved 570")));
        assert!(is_mount_news(&json!("removed")));
        assert!(is_mount_news(&Value::Null));
        assert!(!is_mount_news(&json!("already")));
        assert!(!is_mount_news(&json!("off")));
        assert!(!is_mount_news(&json!("no-appid")));
    }

    #[test]
    fn teardown_script_targets_our_button_only() {
        assert!(STORE_TEARDOWN_JS.contains("data-stt-store-btn"));
        assert!(STORE_TEARDOWN_JS.contains("\"off\""));
    }

    #[test]
    fn store_url_filter() {
        assert!(is_store_app_url(
            "https://store.steampowered.com/app/3240220/GTA/"
        ));
        assert!(!is_store_app_url("https://store.steampowered.com/"));
        assert!(!is_store_app_url(
            "https://store.steampowered.com/news/app/593110/"
        ));
        assert!(!is_store_app_url(
            "https://store.steampowered.com/agecheck/app/3240220/"
        ));
        assert!(!is_store_app_url("https://steamloopback.host/index.html"));
    }

    #[test]
    fn cdp_js_renders_the_store_button() {
        let s = cdp_store_inject_js();
        assert!(s.contains("data-stt-store-btn"));
        // 结果回写靠 data-stt-app 对号入座.
        assert!(s.contains("data-stt-app"));
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
    fn feedback_js_targets_our_button_and_escapes_label() {
        let js = store_button_result_js(570, true, "已入库 570");
        assert!(js.contains("data-stt-store-btn"));
        assert!(js.contains("data-stt-app"));
        assert!(js.contains("570"));
        assert!(js.contains("已入库 570"));
        // 引号/换行不能原样进脚本, 否则 evaluate 直接炸.
        let bad = store_button_result_js(1, false, "失败: \"x\ny");
        assert!(bad.contains("失败:  x y"));
        assert!(!bad.contains("\"x"));
        assert!(!bad.contains("x\ny"));
    }

    /// 页面 CSP 只放行 27060, 发不出去; 留着只会每次点击都报一条控制台错误.
    #[test]
    fn cdp_js_does_not_call_out_to_a_local_port() {
        let s = cdp_store_inject_js();
        assert!(!s.contains("fetch("), "{s}");
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
