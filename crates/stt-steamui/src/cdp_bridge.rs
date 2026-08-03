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
use crate::store_debug::{cdp_host_port, session_port_live};
use crate::store_inject::{app_id_from_store_path, STORE_INJECT_JS};

pub(crate) const DRAIN_JS: &str = r#"(function(){var p=window.__SteamToolsPending||[];window.__SteamToolsPending=[];return p;})()"#;

/// 商店页一次入库意图 (主击 / 菜单 / DLC 列表 / DLC 确认).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorePendingJob {
    pub app_id: u32,
    /// full | game_only | select_dlc
    pub mode: String,
    /// 仅 select_dlc: list 拉候选 / commit 写入.
    pub stage: Option<String>,
    pub dlc_ids: Vec<u32>,
    pub reason: String,
}

impl StorePendingJob {
    pub fn is_dlc_list(&self) -> bool {
        self.mode == "select_dlc" && self.stage.as_deref() == Some("list")
    }

    pub fn is_dlc_commit(&self) -> bool {
        self.mode == "select_dlc" && self.stage.as_deref() != Some("list")
    }
}

/// CDP 专用短脚本: 大脚本在 CEF evaluate 上偶发挂起; 短脚本狗粮已验证 near-cart 可挂.
///
/// 点击只入 `window.__SteamToolsPending`, 由 [`DRAIN_JS`] 取走 — 商店页的 CSP
/// `connect-src` 只放行 `127.0.0.1:27060` (Steam 自己占着), 页面发不出到别的
/// 本机端口的请求, 所以不存在"直接回传"这条路.
///
/// 状态:
/// - STT managed / Steam already_owned → 灰「已入库」, 不可点
/// - 未管理 → split「入库 | ▾」: 主击全入库; 菜单 全入库/仅游戏/选择 DLC
pub const CDP_STORE_INJECT_JS: &str = r##"
(function(){
  window.__SteamToolsPending = window.__SteamToolsPending || [];
  window.__SteamToolsManaged = window.__SteamToolsManaged || [];
  var href = String(location.href||"");
  var m = String(location.pathname||"").match(/^\/(?:agecheck\/)?app\/(\d+)(?:\/|$)/);
  var appId = m ? m[1] : null;
  if(!appId) return "no-appid";
  function isManaged(){
    var list = window.__SteamToolsManaged || [];
    var id = Number(appId);
    for(var i=0;i<list.length;i++){ if(Number(list[i])===id) return true; }
    return false;
  }
  function isOwnedUi(){
    return !!document.querySelector(".game_area_already_owned,.game_area_already_owned_ctn,.already_in_library");
  }
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
  function closeMenu(){
    var mnu=document.getElementById("stt-store-menu");
    if(mnu && mnu.parentNode) mnu.parentNode.removeChild(mnu);
  }
  // Steam 原生按钮靠外层 a.btn_* + 内层 span 出渐变/高度; 不要改外层 display.
  function setLabel(wrap, text, withChev){
    wrap.innerHTML="";
    var shell=document.createElement("span");
    shell.style.cssText="display:inline-flex;align-items:center;gap:0;";
    var main=document.createElement("span");
    main.setAttribute("data-stt-main","1");
    main.textContent=text;
    shell.appendChild(main);
    if(withChev){
      var chev=document.createElement("span");
      chev.setAttribute("data-stt-chev","1");
      chev.setAttribute("aria-label","更多入库选项");
      // 小三角, 跟主文案同一行; 左边一条淡分隔, 不另起一块底色.
      chev.innerHTML="&#9662;";
      chev.style.cssText="margin-left:7px;padding-left:7px;border-left:1px solid rgba(255,255,255,.22);font-size:11px;line-height:1;opacity:.92;";
      shell.appendChild(chev);
    }
    wrap.appendChild(shell);
    return {main:main, chev:wrap.querySelector("[data-stt-chev]")};
  }
  function setBusy(wrap, text){
    closeMenu();
    wrap.setAttribute("data-stt-busy","1");
    wrap.className="btn_grey_steamui btn_medium";
    wrap.style.cursor="default";
    wrap.onclick=null;
    setLabel(wrap, text, false);
  }
  function enqueue(mode, stage, dlcIds, reason){
    var item={app_id:Number(appId),mode:mode||"full",reason:reason||"store_btn",href:href,ts:Date.now()};
    if(stage) item.stage=stage;
    if(dlcIds && dlcIds.length) item.dlc_ids=dlcIds;
    window.__SteamToolsPending.push(item);
  }
  function openMenu(wrap){
    closeMenu();
    if(wrap.getAttribute("data-stt-busy")) return;
    var menu=document.createElement("div");
    menu.id="stt-store-menu";
    menu.setAttribute("data-stt-store-menu","1");
    // 贴近 Steam 上下文菜单: 深底 + 细边 + 轻阴影.
    menu.style.cssText="position:absolute;z-index:2147483647;min-width:148px;background:#171a21;border:1px solid #3d4450;border-radius:2px;box-shadow:0 0 12px rgba(0,0,0,.55);padding:2px 0;font:13px/1.4 \"Motiva Sans\",Arial,sans-serif;color:#dcdedf;";
    function addItem(text, fn){
      var row=document.createElement("div");
      row.textContent=text;
      row.style.cssText="padding:7px 14px;cursor:pointer;white-space:nowrap;";
      row.onmouseenter=function(){row.style.background="#1a9fff"; row.style.color="#fff";};
      row.onmouseleave=function(){row.style.background=""; row.style.color="";};
      row.onclick=function(ev){try{ev.preventDefault();ev.stopPropagation();}catch(e){} closeMenu(); fn();};
      menu.appendChild(row);
    }
    addItem("全入库", function(){ enqueue("full",null,null,"store_menu"); setBusy(wrap,"已排队"); });
    addItem("仅游戏", function(){ enqueue("game_only",null,null,"store_menu"); setBusy(wrap,"已排队"); });
    addItem("选择 DLC…", function(){ enqueue("select_dlc","list",null,"store_dlc"); setBusy(wrap,"加载 DLC…"); });
    var rect=wrap.getBoundingClientRect();
    menu.style.left=(rect.left+(window.scrollX||0))+"px";
    menu.style.top=(rect.bottom+2+(window.scrollY||0))+"px";
    (document.body||document.documentElement).appendChild(menu);
    setTimeout(function(){
      function onDoc(ev){
        if(menu.contains(ev.target) || wrap.contains(ev.target)) return;
        closeMenu();
        document.removeEventListener("mousedown", onDoc, true);
      }
      document.addEventListener("mousedown", onDoc, true);
    },0);
  }
  function paintOwned(wrap){
    closeMenu();
    wrap.className="btn_grey_steamui btn_medium";
    wrap.style.cursor="default";
    wrap.style.display="";
    wrap.style.alignItems="";
    wrap.removeAttribute("data-stt-busy");
    wrap.onclick=null;
    setLabel(wrap, "已入库", false);
  }
  function paintActive(wrap){
    wrap.className="btn_blue_steamui btn_medium";
    wrap.style.cursor="pointer";
    wrap.style.display="";
    wrap.style.alignItems="";
    wrap.removeAttribute("data-stt-busy");
    // 一体式: 外层仍是单颗 Steam 蓝钮; 内层「入库 + ▾」共享同一渐变.
    var parts=setLabel(wrap, "入库", true);
    parts.main.style.cursor="pointer";
    if(parts.chev) parts.chev.style.cursor="pointer";
    parts.main.onclick=function(ev){
      try{ev.preventDefault();ev.stopPropagation();}catch(e){}
      if(wrap.getAttribute("data-stt-busy")) return;
      enqueue("full",null,null,"store_btn");
      setBusy(wrap,"已排队");
    };
    if(parts.chev){
      parts.chev.onclick=function(ev){
        try{ev.preventDefault();ev.stopPropagation();}catch(e){}
        if(wrap.getAttribute("data-stt-busy")) return;
        openMenu(wrap);
      };
    }
  }
  var host = anchorHost();
  var old = document.querySelector("[data-stt-store-btn]");
  var locked = isManaged() || isOwnedUi();
  if(old){
    if(old.getAttribute("data-stt-app") !== String(appId)){
      // app 变了: 重建
      if(old.parentNode) old.parentNode.removeChild(old);
      old=null;
    } else {
      if(old.getAttribute("data-stt-fallback") && host){
        toInline(old); host.appendChild(old);
      }
      // busy 中不打断; 否则按 managed 重绘 (也清掉旧版 split 的 display:flex 残留).
      if(!old.getAttribute("data-stt-busy")){
        if(locked) paintOwned(old); else paintActive(old);
      } else if(locked){
        paintOwned(old);
      }
      return old.getAttribute("data-stt-fallback") && host ? ("moved "+appId) : "already";
    }
  }
  var btn=document.createElement("a");
  btn.setAttribute("role","button");
  btn.setAttribute("data-stt-store-btn","1");
  btn.setAttribute("data-stt-app", String(appId));
  btn.style.cssText="margin-left:2px;";
  if(locked) paintOwned(btn); else paintActive(btn);
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
  var menu=document.getElementById("stt-store-menu");
  var picker=document.getElementById("stt-dlc-picker");
  var n=b.length;
  for(var i=0;i<b.length;i++){ if(b[i].remove) b[i].remove(); }
  if(menu && menu.parentNode) menu.parentNode.removeChild(menu);
  if(picker && picker.parentNode) picker.parentNode.removeChild(picker);
  if(!n && !menu && !picker) return "off";
  return "removed";
})()
"##;

/// 工具关掉时该往商店页发的脚本.
pub fn store_teardown_js() -> String {
    STORE_TEARDOWN_JS.to_owned()
}

/// 一次入库结果: 缺下载数据 (depot key / access token) 时提示用户.
///
/// 自绘 DOM 遮罩, 不用 alert — Steam 商店页 CSP / CEF 下 alert 可能被吞, 遮罩稳定可见.
/// `missing_text` 由调用方按 `MissingDownloadData::describe()` 生成, 如
/// "下载密钥 (Depot 43)" 或 "访问令牌"; 空串调用方不该调这个函数.
///
/// 幂等: 已存在弹窗就更新文案, 不重复叠加. 可每轮 evaluate.
pub fn store_missing_key_warn_js(app_id: u32, missing_text: &str) -> String {
    // 只允许中文/字母/数字/空格/逗号/括号, 防止引号/分号/换行进脚本.
    let safe: String = missing_text
        .chars()
        .filter(|c| {
            c.is_alphanumeric() || *c == ' ' || *c == ',' || *c == '、' || *c == '(' || *c == ')'
        })
        .take(80)
        .collect();
    format!(
        r##"(function(){{
  var d=document,wrap=d.getElementById("stt-missing-key");
  var text="入库成功, 但 AppID {app_id} 缺少{safe}, 可能无法下载。可在 Steam 库中右键该游戏「刷新清单」重试。";
  if(wrap){{ var old=wrap.querySelector(".stt-mk-body"); if(old) old.textContent=text; wrap.style.display=""; return "update"; }}
  wrap=d.createElement("div");
  wrap.id="stt-missing-key";
  wrap.style.cssText="position:fixed;inset:0;z-index:2147483647;background:rgba(0,0,0,.55);display:flex;align-items:center;justify-content:center;";
  var box=d.createElement("div");
  box.style.cssText="max-width:420px;background:#1b2838;border:1px solid #4b6b80;border-radius:6px;padding:16px 20px;color:#c7d5e0;font:13px/1.5 Arial,sans-serif;box-shadow:0 8px 32px rgba(0,0,0,.5);";
  var title=d.createElement("div");
  title.textContent="缺少下载数据";
  title.style.cssText="font-size:15px;font-weight:bold;color:#dbe9f4;margin-bottom:10px;";
  var body=d.createElement("div");
  body.className="stt-mk-body";
  body.textContent=text;
  var ok=d.createElement("button");
  ok.textContent="知道了";
  ok.style.cssText="margin-top:14px;padding:6px 18px;background:#66c0f4;border:none;border-radius:2px;color:#1b2838;font-size:13px;cursor:pointer;";
  ok.addEventListener("click",function(){{ wrap.style.display="none"; }});
  ok.addEventListener("keydown",function(ev){{
    if(ev.key==="Escape"){{ wrap.style.display="none"; }}
  }});
  box.appendChild(title); box.appendChild(body); box.appendChild(ok);
  wrap.appendChild(box);
  (d.body||d.documentElement).appendChild(wrap);
  try{{ ok.focus(); }}catch(e){{}}
  return "shown";
}})()"##
    )
}

/// 把一次入库结果写回商店按钮 (短脚本, 可每轮 evaluate).
///
/// 只改带 `data-stt-store-btn` 且 `data-stt-app` 对得上的按钮; 对不上就不动.
/// 成功/失败都用灰态: 成功是「已入库」; 失败保留文案, 下一轮 inject 可恢复 split.
pub fn store_button_result_js(app_id: u32, ok: bool, label: &str) -> String {
    let safe: String = label
        .chars()
        .map(|c| match c {
            // 单引号是字面量边界, 分号能终止语句, 都抹成空格 (对显示文本无害).
            '\\' | '"' | '\'' | ';' | '\n' | '\r' | '\u{2028}' | '\u{2029}' => ' ',
            _ => c,
        })
        .take(24)
        .collect();
    let _ = ok;
    format!(
        "(function(){{\n  var id=String({app_id});\n  \
  var nodes=document.querySelectorAll('[data-stt-store-btn]');\n  \
  for(var i=0;i<nodes.length;i++){{\n    var b=nodes[i];\n    \
    var marked=b.getAttribute('data-stt-app');\n    \
    if(marked && marked!==id) continue;\n    \
    b.setAttribute('data-stt-app', id);\n    \
    b.className='btn_grey_steamui btn_medium';\n    \
    b.style.cursor='default';\n    \
    if('{safe}'==='已入库'){{ b.removeAttribute('data-stt-busy'); }}\n    \
    else {{ b.setAttribute('data-stt-busy','1'); }}\n    \
    b.innerHTML='';\n    \
    var sp=document.createElement('span');\n    \
    sp.setAttribute('data-stt-main','1');\n    \
    sp.textContent='{safe}';\n    \
    b.appendChild(sp);\n  }}\n  return 'ok';\n}})()"
    )
}

/// 打开 DLC 多选浮层. `items_json` 为 `[{"id":n,"name":"..."},…]` (serde 生成).
pub fn store_dlc_picker_js(app_id: u32, items_json: &str, truncated: bool) -> String {
    let foot = if truncated {
        "列表已截断, 仅显示部分候选"
    } else {
        ""
    };
    format!(
        r##"(function(){{
  var d=document, appId={app_id};
  var old=d.getElementById("stt-dlc-picker");
  if(old && old.parentNode) old.parentNode.removeChild(old);
  var items={items_json};
  var wrap=d.createElement("div");
  wrap.id="stt-dlc-picker";
  wrap.style.cssText="position:fixed;inset:0;z-index:2147483647;background:rgba(0,0,0,.55);display:flex;align-items:center;justify-content:center;";
  var box=d.createElement("div");
  box.style.cssText="width:min(440px,92vw);max-height:80vh;display:flex;flex-direction:column;background:#1b2838;border:1px solid #4b6b80;border-radius:6px;padding:14px 16px;color:#c7d5e0;font:13px/1.45 Arial,sans-serif;box-shadow:0 8px 32px rgba(0,0,0,.5);";
  var title=d.createElement("div");
  title.textContent="选择要入库的 DLC";
  title.style.cssText="font-size:15px;font-weight:bold;color:#dbe9f4;margin-bottom:8px;";
  var sub=d.createElement("div");
  sub.textContent="AppID "+appId+(items.length?(" · "+items.length+" 项"):" · 未找到 DLC");
  sub.style.cssText="color:#8f98a0;margin-bottom:10px;font-size:12px;";
  var tools=d.createElement("div");
  tools.style.cssText="display:flex;gap:8px;margin-bottom:8px;flex-wrap:wrap;";
  function mkBtn(t){{ var b=d.createElement("button"); b.textContent=t; b.style.cssText="padding:4px 10px;background:#2a475e;border:1px solid #4b6b80;border-radius:2px;color:#c7d5e0;cursor:pointer;font-size:12px;"; return b; }}
  var all=mkBtn("全选"), none=mkBtn("全不选");
  tools.appendChild(all); tools.appendChild(none);
  var filter=d.createElement("input");
  filter.placeholder="筛选…";
  filter.style.cssText="flex:1;min-width:120px;padding:4px 8px;background:#0e1621;border:1px solid #4b6b80;border-radius:2px;color:#c7d5e0;";
  tools.appendChild(filter);
  var list=d.createElement("div");
  list.style.cssText="overflow:auto;flex:1;min-height:120px;max-height:46vh;border:1px solid #2a475e;border-radius:3px;padding:4px 0;";
  var checks=[];
  function render(q){{
    list.innerHTML=""; checks=[];
    q=(q||"").toLowerCase();
    for(var i=0;i<items.length;i++){{
      var it=items[i]; var name=String(it.name||("DLC "+it.id));
      if(q && name.toLowerCase().indexOf(q)<0 && String(it.id).indexOf(q)<0) continue;
      var row=d.createElement("label");
      row.style.cssText="display:flex;gap:8px;align-items:flex-start;padding:6px 10px;cursor:pointer;";
      var cb=d.createElement("input"); cb.type="checkbox"; cb.checked=true; cb.value=String(it.id);
      var tx=d.createElement("span"); tx.textContent=name+"  ("+it.id+")";
      row.appendChild(cb); row.appendChild(tx); list.appendChild(row); checks.push(cb);
    }}
    if(!checks.length){{ var empty=d.createElement("div"); empty.textContent=items.length?"无匹配项":"未找到 DLC, 确认将仅入库主游戏"; empty.style.cssText="padding:16px;color:#8f98a0;"; list.appendChild(empty); }}
  }}
  render("");
  all.onclick=function(){{ for(var i=0;i<checks.length;i++) checks[i].checked=true; }};
  none.onclick=function(){{ for(var i=0;i<checks.length;i++) checks[i].checked=false; }};
  filter.oninput=function(){{ render(filter.value); }};
  var foot=d.createElement("div");
  foot.textContent="{foot}";
  foot.style.cssText="color:#8f98a0;font-size:11px;margin-top:6px;min-height:14px;";
  var actions=d.createElement("div");
  actions.style.cssText="display:flex;justify-content:flex-end;gap:8px;margin-top:12px;";
  var cancel=mkBtn("取消"); var ok=mkBtn("确认入库");
  ok.style.background="#66c0f4"; ok.style.color="#1b2838"; ok.style.border="none";
  function close(){{ if(wrap.parentNode) wrap.parentNode.removeChild(wrap); }}
  function restoreBtn(){{
    var nodes=d.querySelectorAll("[data-stt-store-btn]");
    for(var i=0;i<nodes.length;i++){{
      var b=nodes[i];
      if(b.getAttribute("data-stt-app")!==String(appId)) continue;
      b.removeAttribute("data-stt-busy");
    }}
  }}
  cancel.onclick=function(){{ close(); restoreBtn(); }};
  ok.onclick=function(){{
    var ids=[];
    for(var i=0;i<checks.length;i++){{ if(checks[i].checked) ids.push(Number(checks[i].value)); }}
    window.__SteamToolsPending=window.__SteamToolsPending||[];
    window.__SteamToolsPending.push({{app_id:Number(appId),mode:"select_dlc",stage:"commit",dlc_ids:ids,reason:"store_dlc",href:String(location.href||""),ts:Date.now()}});
    close();
    var nodes=d.querySelectorAll("[data-stt-store-btn]");
    for(var j=0;j<nodes.length;j++){{
      var b=nodes[j];
      if(b.getAttribute("data-stt-app")!==String(appId)) continue;
      b.setAttribute("data-stt-busy","1");
      b.className="btn_grey_steamui btn_medium";
      b.style.cursor="default";
      b.innerHTML="";
      var sp=d.createElement("span"); sp.setAttribute("data-stt-main","1"); sp.textContent="已排队"; b.appendChild(sp);
    }}
  }};
  actions.appendChild(cancel); actions.appendChild(ok);
  box.appendChild(title); box.appendChild(sub); box.appendChild(tools); box.appendChild(list); box.appendChild(foot); box.appendChild(actions);
  wrap.appendChild(box);
  (d.body||d.documentElement).appendChild(wrap);
  return "picker";
}})()"##
    )
}

/// DLC 列表失败时恢复按钮并提示.
pub fn store_dlc_picker_error_js(app_id: u32, message: &str) -> String {
    let safe: String = message
        .chars()
        .map(|c| match c {
            '\\' | '"' | '\'' | ';' | '\n' | '\r' | '\u{2028}' | '\u{2029}' => ' ',
            _ => c,
        })
        .take(40)
        .collect();
    format!(
        "(function(){{\n  var id=String({app_id});\n  \
  var nodes=document.querySelectorAll('[data-stt-store-btn]');\n  \
  for(var i=0;i<nodes.length;i++){{\n    var b=nodes[i];\n    \
    if(b.getAttribute('data-stt-app')!==id) continue;\n    \
    b.removeAttribute('data-stt-busy');\n    \
    b.className='btn_grey_steamui btn_medium';\n    b.style.cursor='default';\n    \
    b.innerHTML=''; var sp=document.createElement('span');\n    \
    sp.setAttribute('data-stt-main','1'); sp.textContent='{safe}'; b.appendChild(sp);\n  }}\n  return 'err';\n}})()"
    )
}

/// 一次轮询结果.
#[derive(Debug, Default, Clone)]
pub struct StoreCdpPoll {
    pub cdp_up: bool,
    pub store_pages: usize,
    pub injected: usize,
    /// 兼容旧调用方: 仅 app_id 列表 (由 pending_jobs 派生).
    pub pending_app_ids: Vec<u32>,
    /// 结构化入库意图 (含 mode / dlc).
    pub pending_jobs: Vec<StorePendingJob>,
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
                for job in res.pending {
                    if !out.pending_jobs.iter().any(|j| {
                        j.app_id == job.app_id
                            && j.mode == job.mode
                            && j.stage == job.stage
                            && j.dlc_ids == job.dlc_ids
                    }) {
                        out.pending_app_ids.push(job.app_id);
                        out.pending_jobs.push(job);
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

/// 从 `/json/version` 的 `Browser` 字段解析 Chrome 主版本号.
///
/// 输入可以是整个 `/json/version` 响应, 也可以直接是 Browser 字段值;
/// `"Chrome/126.0.0.0"` → 126. 字段缺失 / 不是 Chrome / 垃圾输入一律 None.
pub fn parse_browser_major(version_json: &str) -> Option<u32> {
    let browser = serde_json::from_str::<Value>(version_json)
        .ok()
        .and_then(|v| v.get("Browser").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| version_json.to_string());
    let after = browser.split_once("Chrome/")?.1;
    let major: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    if major.is_empty() {
        None
    } else {
        major.parse().ok()
    }
}

/// 回退端口上的端点是不是 steamui 的页面.
///
/// 审计实测的 steamui 文档特征: 裸启动页带 `createflags`, 内存页
/// `data:text/html`, 或 `steamloopback.host` 域 (clientui 页面).
pub fn looks_like_steamui_target(url: &str) -> bool {
    url.starts_with("about:blank?createflags=")
        || url.starts_with("data:text/html")
        || url.contains("steamloopback.host")
}

/// 回退端口 (8080) 上的 CDP 端点校验: attach 前先确认它真是 steamui 的 webhelper.
///
/// 1. `/json/version` 的 Browser 必须是 Chrome 且主版本 >= 111 — 111 起
///    Chromium 默认拒绝带 Origin 头的 ws 升级, 低于它没有 Origin 防线;
/// 2. `/json` 里至少有一个 `type == "page"` 且带 steamui 页面特征的目标.
///
/// 任一失败返回 Err(reason), 调用方必须跳过本轮: 商店按钮降级, 不连陌生端点.
fn verify_cef_endpoint(host_port: &str) -> Result<(), String> {
    let version_body = http_get(
        &format!("http://{host_port}/json/version"),
        Duration::from_secs(2),
    )?;
    let ver: Value =
        serde_json::from_str(&version_body).map_err(|e| format!("version json: {e}"))?;
    let browser = ver.get("Browser").and_then(Value::as_str).unwrap_or("");
    let major = parse_browser_major(browser)
        .ok_or_else(|| format!("browser field not Chrome: {browser:?}"))?;
    if major < 111 {
        return Err(format!("chrome {major} < 111"));
    }
    let list_body = http_get(&format!("http://{host_port}/json"), Duration::from_secs(2))?;
    let targets: Value = serde_json::from_str(&list_body).map_err(|e| format!("list json: {e}"))?;
    let arr = targets
        .as_array()
        .ok_or_else(|| "list not array".to_string())?;
    let steamui = arr.iter().any(|t| {
        t.get("type").and_then(Value::as_str) == Some("page")
            && looks_like_steamui_target(t.get("url").and_then(Value::as_str).unwrap_or(""))
    });
    if !steamui {
        return Err("no steamui page target".into());
    }
    Ok(())
}

/// 后台循环: 注入按钮, 并把点击排下的 app_id 回调出去.
pub fn run_store_cdp_loop(
    poll_every: Duration,
    on_app: &mut dyn FnMut(StorePendingJob) -> Option<String>,
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
    on_app: &mut dyn FnMut(StorePendingJob) -> Option<String>,
    on_log: &mut dyn FnMut(String),
    mut panel: Option<&mut dyn PanelBridge>,
) {
    let mut panel_state = PanelState::default();
    let mut last_down_log = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .unwrap_or_else(Instant::now);
    // 回退校验失败也节流: 600ms 一轮, 不通时每 15s 最多记一条.
    let mut last_fallback_log = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .unwrap_or_else(Instant::now);
    let mut last_up = false;
    let mut last_pages: usize = 0;
    // "一个商店页都没摸到" 每轮都会复现: 只在刚进入这个状态时记一次.
    // 按时间节流不行 — 没开商店页是常态, 定时重记就是每小时几百行长日志.
    let mut zero_logged = false;
    loop {
        let js = make_js();
        // 回退端口 (8080) 是未知服务: hook 没赶上时 Steam 自己的调试参数可能
        // 原样活着. attach 前先验证它真是 steamui 的 webhelper, 不过就跳过本轮.
        if !session_port_live() {
            if let Err(reason) = verify_cef_endpoint(&cdp_host_port()) {
                if last_fallback_log.elapsed() >= Duration::from_secs(15) {
                    on_log(format!("store_cdp=unsafe_fallback_8080 reason={reason}"));
                    last_fallback_log = Instant::now();
                }
                std::thread::sleep(poll_every);
                continue;
            }
        }
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
            for job in r.pending_jobs {
                if let Some(js) = on_app(job) {
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
        let mut page = WsEval {
            ws: &mut ws,
            target_id: &t.key,
            url: &t.url,
            on_log,
        };
        match panel_step(&t.key, &mut page, bridge, state, role) {
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

struct WsEval<'a> {
    ws: &'a mut WsClient,
    target_id: &'a str,
    url: &'a str,
    on_log: &'a mut dyn FnMut(String),
}

impl EvalTarget for WsEval<'_> {
    fn eval(&mut self, phase: &str, js: &str) -> Result<Value, String> {
        let started = Instant::now();
        let result = self.ws.eval_value(js);
        let id = self.ws.next_id;
        if let Err(error) = &result {
            (self.on_log)(format!(
                "config_ui=eval transport=ws target={} url={} phase={phase} id={id} elapsed_ms={} class={} error={error}",
                self.target_id,
                sanitize_panel_url(self.url),
                started.elapsed().as_millis(),
                ws_error_class(error),
            ));
        }
        result
    }
}

fn sanitize_panel_url(url: &str) -> &str {
    url.split_once('?').map_or(url, |(base, _)| base)
}

fn ws_error_class(error: &str) -> &'static str {
    if error.starts_with("cdp error") {
        "cdp_error"
    } else if error.starts_with("js error") {
        "js_exception"
    } else if error.contains("timed out") {
        "reply_timeout"
    } else {
        "ws_error"
    }
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
        match WsClient::connect(&target.ws, Duration::from_millis(1500)) {
            Ok(mut page) => match page.eval("library_menu_drain", LIBRARY_MENU_DRAIN_JS) {
                Ok(value) => {
                    let has_actions = value
                        .get("q")
                        .and_then(Value::as_array)
                        .is_some_and(|actions| !actions.is_empty());
                    let (alive, dropped) = apply_library_menu_drain(&value, app_id, bridge, state);
                    if dropped > 0 {
                        on_log(format!("config_ui=library_menu dropped_actions={dropped}"));
                    }
                    if has_actions {
                        if let Err(e) = dispatch_escape(&mut page) {
                            on_log(format!("config_ui=library_menu escape_err {e}"));
                        }
                    }
                    if !alive {
                        state.clear_active_menu();
                    }
                }
                Err(_) => state.clear_active_menu(),
            },
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
        let result =
            WsClient::connect(&target.ws, Duration::from_millis(1500)).and_then(|mut page| {
                page.eval(
                    "library_menu_install",
                    &library_menu_inject_js(app_id, point),
                )
            });
        match result {
            Ok(value) => {
                let status = value.get("s").and_then(Value::as_str).unwrap_or("invalid");
                if matches!(status, "injected" | "already") {
                    state.activate_menu(&target.key, app_id);
                    on_log(format!(
                        "config_ui=library_menu {status} app_id={app_id} target=source"
                    ));
                    return;
                }
                on_log(format!(
                    "config_ui=library_menu inject_miss source state={status} point={point:?}"
                ));
            }
            Err(e) => on_log(format!("config_ui=library_menu inject_err source {e}")),
        }
    } else {
        on_log(format!(
            "config_ui=library_menu inject_miss source_key_not_found key={source_key}"
        ));
    }
    for target in targets
        .iter()
        .filter(|target| is_popup_menu_target(&target.url, &target.title))
    {
        let result =
            WsClient::connect(&target.ws, Duration::from_millis(1500)).and_then(|mut page| {
                page.eval(
                    "library_menu_install",
                    &library_menu_inject_js(app_id, None),
                )
            });
        let value = match result {
            Ok(value) => value,
            Err(e) => {
                on_log(format!(
                    "config_ui=library_menu inject_err popup title={} {e}",
                    clip(&target.title, 32)
                ));
                continue;
            }
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
            clip_why(&out.asked_why)
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

fn dispatch_escape(ws: &mut WsClient) -> Result<(), String> {
    for event_type in ["keyDown", "keyUp"] {
        ws.call("Input.dispatchKeyEvent", Some(escape_key_event(event_type)))?;
    }
    Ok(())
}

fn escape_key_event(event_type: &str) -> Value {
    json!({
        "type": event_type,
        "key": "Escape",
        "code": "Escape",
        "windowsVirtualKeyCode": 27,
        "nativeVirtualKeyCode": 27,
    })
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

/// 日志用的 why 值: 页面可塞任意长度与控制字符, 先剥 \0/\r/\n 再截 64 字符.
fn clip_why(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '\0' && *c != '\r' && *c != '\n')
        .take(64)
        .collect()
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
    pending: Vec<StorePendingJob>,
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
        Ok(pending) => parse_pending_jobs(&pending),
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

/// 解析 `__SteamToolsPending` 队列: mode / stage / dlc_ids.
pub fn parse_pending_jobs(v: &Value) -> Vec<StorePendingJob> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in arr {
        let Some(app_id) = item
            .get("app_id")
            .and_then(Value::as_u64)
            .and_then(|id| u32::try_from(id).ok())
            .filter(|&id| id > 0)
        else {
            continue;
        };
        let mode = item
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("full")
            .to_owned();
        let mode = match mode.as_str() {
            "game_only" | "select_dlc" | "full" => mode,
            _ => "full".to_owned(),
        };
        let stage = item.get("stage").and_then(Value::as_str).map(str::to_owned);
        let dlc_ids = item
            .get("dlc_ids")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_u64())
                    .filter_map(|id| u32::try_from(id).ok())
                    .filter(|&id| id > 0)
                    .collect()
            })
            .unwrap_or_default();
        let reason = item
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("store_btn")
            .to_owned();
        out.push(StorePendingJob {
            app_id,
            mode,
            stage,
            dlc_ids,
            reason,
        });
    }
    out
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

/// 单次 CDP 调用的总时限, 与 pipe 路径的 evaluate 上限一致.
const WS_CALL_TIMEOUT: Duration = Duration::from_millis(2500);

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
        // 总时限兜底: 把单次读超时收紧到 deadline 内, 这样阻塞的 read
        // 也会在总时限附近返回, 再经下方 deadline 检查转成明确超时分类 —
        // 任何丢回复场景都只让这一轮失败, 不把轮询线程永久挂住.
        let deadline = std::time::Instant::now() + WS_CALL_TIMEOUT;
        self.stream
            .set_read_timeout(Some(WS_CALL_TIMEOUT))
            .map_err(|e| e.to_string())?;
        loop {
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "reply timeout after {}ms for {method}",
                    WS_CALL_TIMEOUT.as_millis()
                ));
            }
            let raw = match self.recv_text() {
                Ok(raw) => raw,
                Err(e) => {
                    // 读超时先到 (read timeout == deadline): 归为总时限超时,
                    // 让上层能按 reply 超时分类, 而不是一堆底层 io 文案.
                    if std::time::Instant::now() >= deadline {
                        return Err(format!(
                            "reply timeout after {}ms for {method}",
                            WS_CALL_TIMEOUT.as_millis()
                        ));
                    }
                    return Err(e);
                }
            };
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
    fn eval(&mut self, _phase: &str, js: &str) -> Result<Value, String> {
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

    #[test]
    fn escape_key_events_use_cdp_input_fields() {
        let down = escape_key_event("keyDown");
        let up = escape_key_event("keyUp");
        for event in [&down, &up] {
            assert_eq!(event["key"], "Escape");
            assert_eq!(event["code"], "Escape");
            assert_eq!(event["windowsVirtualKeyCode"], 27);
            assert_eq!(event["nativeVirtualKeyCode"], 27);
        }
        assert_eq!(down["type"], "keyDown");
        assert_eq!(up["type"], "keyUp");
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
        // managed / split / agecheck / DLC menu
        assert!(s.contains("__SteamToolsManaged"));
        assert!(s.contains("data-stt-chev"));
        assert!(s.contains("select_dlc"));
        assert!(s.contains("agecheck"));
        assert!(s.contains("已入库"));
    }

    #[test]
    fn feedback_js_targets_our_button_and_escapes_label() {
        let js = store_button_result_js(570, true, "已入库 570");
        assert!(js.contains("data-stt-store-btn"));
        assert!(js.contains("data-stt-app"));
        assert!(js.contains("570"));
        assert!(js.contains("已入库 570"));
        // 引号/换行/单引号/分号不能原样进脚本, 否则 evaluate 直接炸.
        let bad = store_button_result_js(1, false, "失败: \"x\ny';pwn");
        assert!(bad.contains("失败:  x y  pwn"));
        assert!(!bad.contains("\"x"));
        assert!(!bad.contains("x\ny"));
        // 模板不能把标签里的引号漏进字面量; 成功/失败都用 grey.
        assert!(bad.contains("btn_grey_steamui"));
        assert_eq!(
            bad.matches('\'').count(),
            store_button_result_js(1, false, "safe")
                .matches('\'')
                .count()
        );
    }

    #[test]
    fn missing_key_warn_js_lists_depots_and_is_idempotent() {
        let js = store_missing_key_warn_js(570, "下载密钥 (Depot 43, 7)");
        assert!(js.contains("AppID 570"));
        assert!(js.contains("Depot 43, 7"));
        assert!(js.contains("stt-missing-key"));
        assert!(js.contains("刷新清单"));
        // 幂等: 已存在时只更新文案, 不重复建遮罩.
        assert!(js.contains("if(wrap)"));
        assert!(js.contains("return \"update\""));
    }

    #[test]
    fn missing_key_warn_js_renders_token_text() {
        let js = store_missing_key_warn_js(570, "访问令牌");
        assert!(js.contains("缺少访问令牌"));
        assert!(js.contains("stt-missing-key"));
        // 标题是通用的"缺少下载数据".
        assert!(js.contains("缺少下载数据"));
    }

    #[test]
    fn missing_key_warn_js_escapes_hostile_text() {
        // missing_text 若混入脚本字符必须被剥掉, 不能原样进脚本.
        // (textContent 赋值无 XSS, 但要保证引号/分号/换行进不了字符串字面量.)
        let js = store_missing_key_warn_js(1, "下载密钥\"; alert(1); //");
        assert!(!js.contains("; //"));
        assert!(!js.contains("alert(1);"));
        assert!(!js.contains("\\\""));
        assert!(js.contains("stt-missing-key"));
    }

    #[test]
    fn why_clips_control_chars_and_length() {
        // 页面可控的 why 值: 控制字符剥掉, 超过 64 字符截断, 不能带换行进日志.
        let long = format!("a\u{0}\r\n{}", "x".repeat(200));
        let clipped = clip_why(&long);
        assert_eq!(clipped.len(), 64);
        assert!(!clipped.contains('\0'));
        assert!(!clipped.contains('\r'));
        assert!(!clipped.contains('\n'));
        assert!(clipped.starts_with('a'));
        assert_eq!(clip_why("正常原因"), "正常原因");
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
        let simple = parse_pending_jobs(&v);
        assert_eq!(simple.len(), 1);
        assert_eq!(simple[0].app_id, 570);
        let jobs = parse_pending_jobs(&json!([{
            "app_id": 730,
            "mode": "select_dlc",
            "stage": "commit",
            "dlc_ids": [1, 2, 0],
            "reason": "store_dlc"
        }]));
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].app_id, 730);
        assert_eq!(jobs[0].mode, "select_dlc");
        assert_eq!(jobs[0].stage.as_deref(), Some("commit"));
        assert_eq!(jobs[0].dlc_ids, vec![1, 2]);
        assert!(jobs[0].is_dlc_commit());
    }

    /// 整个 /json/version 响应和 Browser 字段值两种输入都要能解析.
    #[test]
    fn browser_major_parses_from_version_json() {
        let full = r#"{"Browser": "Chrome/126.0.0.0", "Protocol-Version": "1.3"}"#;
        assert_eq!(parse_browser_major(full), Some(126));
        assert_eq!(parse_browser_major("Chrome/126.0.0.0"), Some(126));
        assert_eq!(parse_browser_major("Chrome/100"), Some(100));
        assert_eq!(parse_browser_major("Chrome/111"), Some(111));
    }

    #[test]
    fn browser_major_rejects_garbage() {
        assert_eq!(parse_browser_major("garbage"), None);
        assert_eq!(parse_browser_major(""), None);
        assert_eq!(parse_browser_major(r#"{"Browser": "Safari/17.4"}"#), None);
        assert_eq!(parse_browser_major("Chrome/"), None);
        assert_eq!(parse_browser_major("Chrome/abc"), None);
    }

    /// 回退校验只认 steamui 文档特征, 普通站点不算.
    #[test]
    fn steamui_target_signatures() {
        assert!(looks_like_steamui_target(
            "about:blank?createflags=274&minwidth=1010"
        ));
        assert!(looks_like_steamui_target(
            "data:text/html,<body></body><!--tracking:x:/library/home-->"
        ));
        assert!(looks_like_steamui_target(
            "https://steamloopback.host/index.html"
        ));
        assert!(!looks_like_steamui_target("https://evil.example/"));
        assert!(!looks_like_steamui_target("about:blank"));
        assert!(!looks_like_steamui_target(
            "https://store.steampowered.com/app/570/"
        ));
    }

    /// 服务端握手后不回任何消息时, call 必须在总时限内失败,
    /// 不能把轮询线程永久挂住 (实机: 外部客户端抢隐式会话后 reply 永不到).
    #[test]
    fn ws_call_bounds_reply_wait_with_deadline() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut req = Vec::new();
            let mut tmp = [0u8; 1];
            while req.len() < 8192 && !req.windows(4).any(|w| w == b"\r\n\r\n") {
                if sock.read(&mut tmp).unwrap() == 0 {
                    break;
                }
                req.push(tmp[0]);
            }
            // 101 握手完成, 之后保持连接但永不回复.
            sock.write_all(
                b"HTTP/1.1 101 Switching Protocols\r\n\
                  Upgrade: websocket\r\n\
                  Connection: Upgrade\r\n\
                  Sec-WebSocket-Accept: xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\n\
                  \r\n",
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_secs(4));
        });

        let ws_url = format!("ws://{addr}/devtools/page/probe");
        // 读超时给得比总时限长: 只有 deadline 分支能拦住这次等待.
        let mut client = WsClient::connect(&ws_url, Duration::from_secs(10)).unwrap();
        let start = std::time::Instant::now();
        let err = client.call("Runtime.evaluate", None).unwrap_err();
        let elapsed = start.elapsed();
        assert!(err.contains("reply timeout"), "预期超时分类, 实际: {err}");
        assert!(
            elapsed >= Duration::from_millis(2500) && elapsed < Duration::from_secs(6),
            "超时边界不对: {elapsed:?}"
        );
        server.join().unwrap();
    }
}
