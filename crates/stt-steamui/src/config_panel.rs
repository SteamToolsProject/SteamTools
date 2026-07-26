//! 客户端界面里的配置入口与面板 (走 CDP 注入, 不碰原生符号).
//!
//! 入口: 克隆导航栏上现成的一个 tab, 只改文字 — 字体/间距/hover 全是 Steam 自己的,
//! 我们既不用认它那串 hash 类名, 也不用手写像素.
//! 面板: 我们自己的一层 DOM, 跟 React 无关, 所以里面爱怎么画怎么画.
//!
//! 回传只走页内队列 + 宿主轮询取走: 页面发不出网络请求到本机端口 (CSP),
//! 这条路已经证伪过一次, 别再往那边走.

use serde_json::Value;
use stt_config::{ConfigIntent, ConfigSnapshot};

/// 一次 tick 最多认这么多条意图; 页面刷再多也没用.
const MAX_INTENTS: usize = 16;

/// 小于这个尺寸的视图不算"摆在用户面前".
///
/// SharedJSContext 报的是 visible 但只有 1x1, 各种弹出菜单也都很小 —— 面板画到
/// 那里面等于没画.
const MIN_SURFACE: f64 = 400.0;

/// 从 tick 结果里读一条边长; 读不到当 0 (也就是"不算摆在面前").
fn surface_side(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

/// 每轮都跑的短脚本: 补挂入口 + 取走意图 + 报告面板状态.
///
/// 挂进导航行以后就是一次 `querySelector` 直接返回; 只有还在兜底状态时才隔几轮
/// 重扫一次 —— Steam 刚启动时窗口文档是个空壳, 那时候扫不到导航行是常态,
/// 挂完兜底就不再看等于永远错过.
pub const NAV_TICK_JS: &str = r##"
(function(){
  var d=document,b=d.body;
  var out={s:"empty",want:false,has:false,q:[]};
  // 状态先报完再谈别的: 空文档也要报出尺寸与可见性, 否则宿主挑不出该画在哪个视图.
  var q=window.__SteamToolsIntents||[];
  window.__SteamToolsIntents=[];
  out.q=q;
  out.has=!!d.getElementById("stt-panel");
  // want / closed 都是一次性事件, 读完就清 —— 面板开在哪个文档由宿主决定,
  // 这个文档只负责报告"用户点了入口"和"用户关了面板".
  out.want=!!window.__SteamToolsWant;
  window.__SteamToolsWant=false;
  out.why=window.__SteamToolsWantWhy||"";
  window.__SteamToolsWantWhy="";
  out.closed=!!window.__SteamToolsClosed;
  window.__SteamToolsClosed=false;
  // 这个视图此刻是不是真的摆在用户面前 —— 宿主据此挑面板画在哪一个.
  // 实测: 主窗口 visible 1137x740, 商店视图 visible 1136x598,
  // 其余菜单/隐藏视图一律 hidden, SharedJSContext 虽然 visible 但只有 1x1.
  out.vis=(d.visibilityState!=="hidden");
  out.w=window.innerWidth; out.h=window.innerHeight;
  out.menu=!!window.__SteamToolsMenuArmed;
  if(!b||!b.children.length) return out;

  function open(e){
    try{e.preventDefault();e.stopPropagation();}catch(x){}
    // 记下这一下是谁点的: 真实点击 isTrusted 为真, 脚本合成的为假.
    try{
      window.__SteamToolsWantWhy=(e&&e.type?e.type:"?")
        +(e&&e.isTrusted?":user":":synthetic");
    }catch(x2){}
    window.__SteamToolsWant=true;
  }
  // 顶部一行等高、文字都很短的兄弟节点 = 导航 tab 行.
  // 不认类名也不认文案 (要考虑多语言); 允许夹着没文字的图标 (前进/后退箭头).
  //
  // 菜单栏 (Steam/查看/好友/游戏/帮助) 与标签行 (商店/库/社区/用户名) 长得一样,
  // 光取文档顺序第一个会挂进菜单栏. 标签行的字明显更大、位置更靠下, 按这个打分.
  function findTabs(){
    var els=d.querySelectorAll("div,nav,ul"),lim=Math.min(els.length,4000);
    var best=null,bestScore=-1;
    for(var i=0;i<lim;i++){
      var e=els[i],r=e.getBoundingClientRect();
      if(r.top>180||r.height<16||r.height>72||r.width<160) continue;
      var kids=e.children;
      if(kids.length<3||kids.length>10) continue;
      var top=null,ok=true,n=0,h=0;
      for(var j=0;j<kids.length;j++){
        var kr=kids[j].getBoundingClientRect(),t=(kids[j].textContent||"").trim();
        if(kr.width<1||kr.height<1) continue;
        if(t.length>28){ok=false;break;}
        if(kr.height<12){ok=false;break;}
        if(top===null)top=kr.top; else if(Math.abs(kr.top-top)>8){ok=false;break;}
        if(t){ n++; if(kr.height>h) h=kr.height; }
      }
      if(!ok||n<3) continue;
      var score=h*4+r.top;
      if(score>bestScore){ bestScore=score; best=e; }
    }
    return best;
  }
  // 克隆最后一个有文字的 tab (用户名), 紧挨着它排 —— 不要 margin-left:auto,
  // 那会把入口顶到行尾, 离用户名老远.
  function mountTab(tabs){
    var src=null;
    for(var i=tabs.children.length-1;i>=0;i--){
      if((tabs.children[i].textContent||"").trim()){ src=tabs.children[i]; break; }
    }
    if(!src) return false;
    var mine=src.cloneNode(false);
    mine.className=String(src.className||"").split(/\s+/).filter(function(c){
      return c && !/active|select|current|highlight/i.test(c);
    }).join(" ");
    if(mine.removeAttribute) mine.removeAttribute("id");
    mine.textContent="SteamTools";
    mine.setAttribute("data-stt-nav","1");
    mine.style.cursor="pointer";
    mine.addEventListener("click",open,true);
    tabs.appendChild(mine);
    window.__SteamToolsNavRow=tabs;
    return true;
  }
  function recon(){
    var els=d.querySelectorAll("div,nav,ul"),a=[];
    for(var i=0;i<els.length&&a.length<24;i++){
      var e=els[i],r=e.getBoundingClientRect();
      if(r.top>180||r.height<12||r.width<60) continue;
      var kt=[];
      for(var j=0;j<e.children.length&&j<6;j++){
        var kr=e.children[j].getBoundingClientRect();
        kt.push(((e.children[j].textContent||"").trim().slice(0,12)||"-")
          +"@"+Math.round(kr.top)+"h"+Math.round(kr.height));
      }
      a.push(e.tagName+"."+String(e.className||"").slice(0,44)
        +" kids="+e.children.length
        +" box="+Math.round(r.left)+","+Math.round(r.top)+","+Math.round(r.width)+"x"+Math.round(r.height)
        +" ["+kt.join(",")+"]");
    }
    return a.join(" | ");
  }

  var mine=d.querySelector("[data-stt-nav]");
  // 网页视图 (商店/社区) 里没有那一行导航, 也就不该有入口 —— 早先在这儿挂兜底,
  // 结果商店页角上多出一个按钮. 顺手把旧版本留下的清掉.
  if(!window.__SteamToolsMountEntry){
    if(mine && mine.remove) mine.remove();
    out.s="guest";
    return out;
  }
  if(mine && !mine.getAttribute("data-stt-fallback")){ out.s="already"; return out; }
  if(mine){
    // 还在兜底: 每 8 轮找一次导航行, 别每轮都扫全文档.
    var n=(window.__SteamToolsRescan||0)+1;
    window.__SteamToolsRescan=n;
    if(n%8){ out.s="already"; return out; }
  }
  var tabs=findTabs();
  if(tabs && mountTab(tabs)){
    if(mine && mine.remove) mine.remove();
    out.s = mine ? "moved" : "nav";
    return out;
  }
  if(mine){
    // 导航行还没出来, 兜底先留着; 再给一次勘察样本 (这时界面多半渲染完了).
    if(window.__SteamToolsRescan>=24 && window.__SteamToolsRecon!==2){
      window.__SteamToolsRecon=2; out.recon=recon();
    }
    out.s="already"; return out;
  }
  // 界面还没渲染出来就别急着挂兜底, 否则一挂上就错过了真正的导航行.
  if(d.getElementsByTagName("*").length<30){ out.s="sparse"; return out; }
  // 小窗口 (好友列表这类) 不挂兜底入口, 否则每个窗口角上都多一块.
  if(window.innerWidth<900||window.innerHeight<500){ out.s="small"; return out; }
  var f=d.createElement("div");
  f.setAttribute("data-stt-nav","1");
  f.setAttribute("data-stt-fallback","1");
  f.textContent="SteamTools";
  f.style.cssText="position:fixed;right:16px;bottom:56px;z-index:2147483646;padding:6px 12px;"
    +"background:#1b2838;color:#c7d5e0;border:1px solid #66c0f4;border-radius:2px;cursor:pointer;"
    +"font:12px/16px Arial,sans-serif;box-shadow:0 2px 8px rgba(0,0,0,.5)";
  f.addEventListener("click",open,true);
  b.appendChild(f);
  out.s="float";
  window.__SteamToolsRecon=1;
  out.recon=recon();
  return out;
})()
"##;

/// 库里的右键菜单 (只对我们自己加的 app 生效).
///
/// 装一次监听就够, 之后每轮只是确认还在. 判 app_id 靠胶囊图的 URL:
/// 本地是 `steamloopback.host/assets/<id>/`, 远程是 `.../apps/<id>/` ——
/// 库行的类名是混淆的, 认不得, 但图片路径稳定.
///
/// **不是我们加的一律不拦**: 用户自己的游戏照常弹 Steam 的菜单.
pub const LIBRARY_MENU_JS: &str = r##"
(function(){
  if(window.__SteamToolsMenuArmed) return "already";
  var d=document;
  function appIdOf(node){
    var e=node;
    for(var up=0; e && up<12; up++, e=e.parentElement){
      var html=e.innerHTML;
      if(!html) continue;
      var m=/(?:assets|apps)\/(\d{3,8})\//.exec(html);
      if(m) return Number(m[1]);
    }
    return 0;
  }
  function close(){
    var old=d.getElementById("stt-ctx");
    if(old&&old.remove) old.remove();
  }
  function item(txt,cb){
    var e=d.createElement("div");
    e.textContent=txt;
    e.style.cssText="padding:8px 16px;cursor:pointer;white-space:nowrap;color:#dcdedf;";
    e.className="stt-ctx-i";
    e.addEventListener("click",function(ev){
      ev.stopPropagation(); close(); cb();
    });
    return e;
  }
  d.addEventListener("contextmenu",function(ev){
    close();
    var managed=window.__SteamToolsManaged||[];
    if(!managed.length) return;
    var id=appIdOf(ev.target);
    if(!id || managed.indexOf(id)<0) return;   // 不是我们加的 -> 让 Steam 自己弹
    ev.preventDefault(); ev.stopPropagation();

    var box=d.createElement("div");
    box.id="stt-ctx";
    box.style.cssText="position:fixed;z-index:9100;min-width:168px;padding:6px 0;"
      +"background:#23262e;border:1px solid rgba(0,0,0,.5);border-radius:3px;"
      +"box-shadow:0 6px 24px rgba(0,0,0,.6);font:14px/18px 'Motiva Sans',Helvetica,sans-serif;";
    var head=d.createElement("div");
    head.textContent="SteamTools · "+id;
    head.style.cssText="padding:6px 16px 8px;color:#7d8894;font-size:12px;"
      +"border-bottom:1px solid rgba(255,255,255,.07);margin-bottom:4px;";
    box.appendChild(head);
    box.appendChild(item("刷新清单",function(){
      (window.__SteamToolsIntents=window.__SteamToolsIntents||[])
        .push({kind:"refresh_app",app_id:id});
    }));
    box.appendChild(item("移除入库",function(){
      (window.__SteamToolsIntents=window.__SteamToolsIntents||[])
        .push({kind:"remove_app",app_id:id});
    }));
    box.appendChild(item("打开 SteamTools",function(){ window.__SteamToolsWant=true; }));
    // 先挂上再量, 否则量不到尺寸; 贴着光标, 越界就翻到另一侧.
    box.style.left="0px"; box.style.top="0px";
    (d.body||d.documentElement).appendChild(box);
    var r=box.getBoundingClientRect();
    var x=ev.clientX, y=ev.clientY;
    if(x+r.width>window.innerWidth) x=Math.max(0,x-r.width);
    if(y+r.height>window.innerHeight) y=Math.max(0,y-r.height);
    box.style.left=x+"px"; box.style.top=y+"px";
  },true);
  d.addEventListener("click",close,true);
  d.addEventListener("keydown",function(e){ if(e.key==="Escape") close(); },true);

  var css=d.createElement("style");
  css.textContent="#stt-ctx .stt-ctx-i:hover{background:#1a9fff;color:#fff}";
  (d.head||d.documentElement).appendChild(css);
  window.__SteamToolsMenuArmed=true;
  return "armed";
})()
"##;

/// 把"哪些 app 是我们加的"推给页面 —— 右键时要即刻判断, 来不及问宿主.
pub fn managed_apps_js(ids: &[u32]) -> String {
    let list: Vec<String> = ids.iter().map(u32::to_string).collect();
    format!("window.__SteamToolsManaged=[{}];", list.join(","))
}

/// 工具关掉时把右键菜单也撤掉.
pub const LIBRARY_MENU_TEARDOWN_JS: &str = r##"
(function(){
  var p=document.getElementById("stt-ctx");
  if(p&&p.remove) p.remove();
  window.__SteamToolsManaged=[];
  return "off";
})()
"##;

/// 每轮的 tick 脚本.
///
/// `mount_entry` 只在客户端外壳里为真: 网页视图里没有导航行, 在那儿挂兜底入口
/// 会让商店页角上多出一个按钮.
pub fn nav_tick_js(mount_entry: bool) -> String {
    format!("window.__SteamToolsMountEntry={mount_entry};{NAV_TICK_JS}")
}

/// 打开配置面板 (只在宿主判定该开时跑一次, 不进每轮的热路径).
///
/// 照 Steam 自己的设置对话框来: 居中浮层 + 左侧分区栏 + 右侧卡片行.
/// 用的都是从 `steamui/css` 里挖出来的它自己的数值 —— 卡片底
/// `rgba(85,85,85,.067)` + 3px 圆角, 按钮底 `hsla(0,0%,100%,.15)` + 2px 圆角,
/// 面板底 `#23262e`, 侧栏标题 `#66c0f4`, 强调 `#1a9fff`.
/// 不自创配色: 这东西住在 Steam 里, 像客人一样穿衣服.
pub const PANEL_JS: &str = r##"
(function(){
  // 意图排在本文档的队列里, 宿主下一轮从同一个文档取走.
  window.__SteamToolsIntents=window.__SteamToolsIntents||[];
  function push(o){window.__SteamToolsIntents.push(o);}

  // 面板就画在当前文档里. 试过开真窗口 (`BrowserView.CreatePopup` + `window.open`):
  // 窗口确实能开, 但 Steam 的弹窗管理器会接管它, 把我们写进去的 DOM 重新渲染掉,
  // 窗口还停在 1x1. 那条路是给它自己托管 BrowserView 用的, 不是给外人当画布的.
  //
  // 所以仍然是页内浮层, 但**只画在当前最上面那个视图里** —— 商店/社区是独立的
  // CEF 视图, 合成在客户端文档之上, 画在下面那层会被整块盖住. 谁在上面由宿主
  // 按 visibilityState + 尺寸挑, 见 `PanelState::owner`.
  var d=document;
  if(d.getElementById("stt-panel")) return "already";
  function el(tag,txt,css){
    var e=d.createElement(tag);
    if(txt!=null)e.textContent=txt;
    if(css)e.style.cssText=css;
    return e;
  }
  function svg(path){
    var s=d.createElementNS("http://www.w3.org/2000/svg","svg");
    s.setAttribute("viewBox","0 0 24 24");
    s.setAttribute("width","19");s.setAttribute("height","19");
    s.setAttribute("fill","none");
    s.setAttribute("stroke","currentColor");
    s.setAttribute("stroke-width","1.7");
    s.setAttribute("stroke-linecap","round");
    s.setAttribute("stroke-linejoin","round");
    var p=d.createElementNS("http://www.w3.org/2000/svg","path");
    p.setAttribute("d",path);
    s.appendChild(p);
    s.style.cssText="flex:none;opacity:.9;";
    return s;
  }

  // 全部取自 steamui/css, 不自创.
  var ACCENT="#1a9fff";
  var TITLE="#66c0f4";
  var SURFACE="#23262e";
  var CARD="rgba(85,85,85,.0666666667)";
  var BTN="hsla(0,0%,100%,.15)";
  var TEXT="#dcdedf";
  var MUTE="#7d8894";
  var SANS="'Motiva Sans',Helvetica,sans-serif";
  var MONO="Consolas,'Cascadia Mono',ui-monospace,monospace";

  var navRow=window.__SteamToolsNavRow||null;

  var root=el("div");
  root.id="stt-panel";
  // 9000: 压得住内容 (Steam 自己的内容都在 4100 以下), 又低于它的菜单与模态
  // (7000 以上那几层), 弹出菜单仍会正常压在我们上面.
  // 四边写全不用 inset 简写 —— CEF 认不认它不好说, 认不出这一条就整块不可见.
  root.style.cssText="position:fixed;left:0;top:0;right:0;bottom:0;z-index:9000;display:flex;"
    +"align-items:center;justify-content:center;background:rgba(0,0,0,.6);"
    +"font-family:"+SANS+";color:"+TEXT+";";

  // 样式挂在 root 里, 关窗时跟着消失; 逐条用 #stt-panel 收口, 不漏进 Steam.
  var css=el("style");
  css.textContent=
    "#stt-panel .stt-nav:hover{background:rgba(255,255,255,.045)}"
   +"#stt-panel .stt-b{transition:background .12s}"
   +"#stt-panel .stt-b:hover{background:hsla(0,0%,100%,.24)}"
   +"#stt-panel .stt-rm:hover{background:rgba(200,70,70,.32)}"
   +"#stt-panel .stt-x:hover{color:#fff}"
   +"#stt-panel .stt-i::placeholder{color:"+MUTE+"}"
   +"#stt-panel .stt-i:focus{border-color:"+ACCENT+"}"
   +"#stt-panel [tabindex]:focus-visible,#stt-panel .stt-i:focus-visible{"
   +"outline:2px solid "+ACCENT+";outline-offset:2px}"
   +"#stt-panel ::-webkit-scrollbar{width:8px}"
   +"#stt-panel ::-webkit-scrollbar-thumb{background:rgba(255,255,255,.14);border-radius:4px}";
  root.appendChild(css);

  // 同理不用 min(): 宽高走 width + max-width 这套老写法.
  var box=el("div",null,"width:880px;max-width:92vw;height:660px;max-height:86vh;display:flex;"
    +"background:"+SURFACE+";border-radius:3px;overflow:hidden;"
    +"box-shadow:0 12px 48px rgba(0,0,0,.6);");
  root.appendChild(box);

  var side=el("div",null,"width:196px;flex:none;padding:22px 0;overflow-y:auto;"
    +"border-right:1px solid rgba(0,0,0,.36);");
  var pane=el("div",null,"flex:1;min-width:0;display:flex;flex-direction:column;");
  box.appendChild(side);box.appendChild(pane);

  side.appendChild(el("div","STEAMTOOLS 设置",
    "padding:0 22px;margin-bottom:20px;font-size:15px;font-weight:700;color:"+TITLE+";"
    +"letter-spacing:.6px;"));
  var navBox=el("div");
  side.appendChild(navBox);

  var head=el("div",null,"display:flex;align-items:center;justify-content:space-between;"
    +"gap:16px;padding:26px 28px 16px;flex:none;");
  var title=el("div",null,"font-size:22px;line-height:28px;font-weight:700;color:#fff;");
  var shutBtn=el("div","✕","cursor:pointer;font-size:17px;color:"+MUTE+";flex:none;"
    +"padding:2px 6px;line-height:1;");
  shutBtn.className="stt-x";
  shutBtn.tabIndex=0;
  head.appendChild(title);head.appendChild(shutBtn);
  var body=el("div",null,"flex:1;min-height:0;overflow-y:auto;padding:0 28px 28px;");
  pane.appendChild(head);pane.appendChild(body);
  (d.body||d.documentElement).appendChild(root);

  var rowClick=null;
  // 关掉要报给宿主: 面板可能在别的视图里也有一份, 这边关了别处得跟着关.
  function shut(){
    window.__SteamToolsWant=false;
    window.__SteamToolsClosed=true;
    window.__SteamToolsClose=null;
    if(root.parentNode)root.parentNode.removeChild(root);
    d.removeEventListener("keydown",onKey,true);
    if(navRow&&rowClick)navRow.removeEventListener("click",rowClick,true);
  }
  function onKey(e){ if(e.key==="Escape") shut(); }
  shutBtn.addEventListener("click",shut);
  shutBtn.addEventListener("keydown",function(e){
    if(e.key==="Enter"||e.key===" "){e.preventDefault();shut();}
  });
  d.addEventListener("keydown",onKey,true);
  // 点浮层外面关掉, 与 Steam 的对话框一致.
  root.addEventListener("click",function(e){ if(e.target===root) shut(); });
  if(navRow){
    rowClick=function(e){
      var t=e.target;
      while(t&&t!==navRow){
        if(t.getAttribute&&t.getAttribute("data-stt-nav")) return;
        t=t.parentNode;
      }
      shut();
    };
    navRow.addEventListener("click",rowClick,true);
  }
  window.__SteamToolsClose=shut;

  // 一行 = 一张卡片, 卡片之间靠间距分开 —— 与 Steam 设置页里那些行同一种做法.
  function card(){
    var c=el("div",null,"display:flex;align-items:center;justify-content:space-between;"
      +"gap:20px;background:"+CARD+";border-radius:3px;padding:14px 18px;margin-bottom:8px;");
    body.appendChild(c);
    return c;
  }
  function label(main_,sub_,mono){
    var w=el("div",null,"min-width:0;");
    w.appendChild(el("div",main_,"font-size:15px;line-height:20px;color:"+TEXT+";"
      +(mono?"font-family:"+MONO+";font-size:13.5px;overflow:hidden;text-overflow:ellipsis;":"")));
    if(sub_){
      w.appendChild(el("div",sub_,"margin-top:3px;font-size:12.5px;line-height:17px;color:"+MUTE+";"));
    }
    return w;
  }
  function btn(txt,cls){
    var b=el("div",txt,"background:"+BTN+";color:"+TEXT+";border-radius:2px;flex:none;"
      +"padding:8px 18px;font-size:14px;line-height:18px;cursor:pointer;text-align:center;");
    b.className="stt-b"+(cls?" "+cls:"");
    b.tabIndex=0;
    return b;
  }
  function onHit(e,fn){
    e.addEventListener("click",fn);
    e.addEventListener("keydown",function(ev){
      if(ev.key==="Enter"||ev.key===" "){ev.preventDefault();fn();}
    });
  }
  function toggle(on,cb){
    var rail=el("div");
    rail.tabIndex=0;
    rail.setAttribute("role","switch");
    rail.setAttribute("aria-checked",on?"true":"false");
    function paint(v){
      rail.style.cssText="position:relative;width:38px;height:21px;border-radius:11px;flex:none;"
        +"cursor:pointer;transition:background .16s;"
        +"background:"+(v?ACCENT:"rgba(255,255,255,.16)")+";";
    }
    paint(on);
    var knob=el("div",null,"position:absolute;top:3px;left:"+(on?"20px":"3px")+";width:15px;"
      +"height:15px;border-radius:50%;background:#fff;transition:left .16s;");
    rail.appendChild(knob);
    onHit(rail,function(){
      // 先自己动起来, 别等宿主那一轮回来才给反馈.
      paint(!on);
      knob.style.left=on?"3px":"20px";
      rail.setAttribute("aria-checked",on?"false":"true");
      cb();
    });
    return rail;
  }
  // 选项组: 一条分段控件, 当前项用 Steam 的强调蓝.
  function seg(list,cur,cb){
    var w=el("div",null,"display:flex;flex:none;border-radius:2px;overflow:hidden;"
      +"background:rgba(0,0,0,.24);");
    (list||[]).forEach(function(v){
      var on=v===cur;
      var b=el("div",v,"padding:7px 14px;font-family:"+MONO+";font-size:13px;line-height:16px;"
        +"cursor:pointer;"+(on?"background:"+ACCENT+";color:#fff;":"color:"+MUTE+";"));
      if(!on) b.className="stt-b";
      b.tabIndex=0;
      onHit(b,function(){ if(!on){ b.textContent="…"; cb(v);} });
      w.appendChild(b);
    });
    return w;
  }

  // 图标: 滑杆 / 下载 / 文件夹 / 信息. 工具那个别用三条横线, 那读起来是"菜单".
  var PAGES=[
    ["tools","工具","M4 8h3m4 0h9M4 16h9m4 0h3"
      +"M11 8a2 2 0 10-4 0 2 2 0 004 0M17 16a2 2 0 10-4 0 2 2 0 004 0"],
    ["source","上游与日志","M12 3v12m0 0l-4-4m4 4l4-4M4 19h16"],
    ["apps","已入库","M20 7l-8-4-8 4m16 0l-8 4m8-4v10l-8 4m0-10L4 7m8 4v10M4 7v10l8 4"],
    ["lua","Lua 目录","M3 7a2 2 0 012-2h4l2 2h8a2 2 0 012 2v8a2 2 0 01-2 2H5a2 2 0 01-2-2z"],
    ["status","状态","M12 21a9 9 0 100-18 9 9 0 000 18zM12 8h.01M11 12h1v5h1"]
  ];
  // 这两个要跨重绘活着: 快照每变一次就整窗重画.
  var page="tools";
  var typed="";

  function renderTools(s){
    (s.tools||[]).forEach(function(x){
      var c=card();
      // 开着不等于跑起来了: 第二行说清它此刻在干什么 / 为什么没干成.
      var left=label(x.name,x.detail||x.id);
      if(x.placeholder){
        // 占位是第三种状态, 开关表达不了 —— 给它一个标记, 别塞进名字里.
        left.firstChild.appendChild(el("span","占位",
          "margin-left:10px;font-size:11px;color:"+MUTE+";background:rgba(0,0,0,.3);"
          +"border-radius:2px;padding:2px 7px;vertical-align:1px;"));
      }
      c.appendChild(left);
      c.appendChild(toggle(x.enabled,function(){
        push({kind:"set_tool",id:x.id,on:!x.enabled});
      }));
    });
  }
  // 已入库: 只列我们自己写的那些 lua, 每项能刷新 / 移除.
  function renderApps(s){
    var ids=s.managed||[];
    if(!ids.length){
      var e=card();
      e.appendChild(label("还没有入库的应用","从商店页点「入库」, 或往 steamtools/inbox 丢 app_id"));
      return;
    }
    ids.forEach(function(id){
      var c=card();
      c.appendChild(label(String(id),"config/lua/stt_"+id+".lua",true));
      var box=el("div",null,"display:flex;gap:8px;flex:none;");
      var re=btn("刷新");
      onHit(re,function(){ re.textContent="…"; push({kind:"refresh_app",app_id:id}); });
      var rm=btn("移除","stt-rm");
      onHit(rm,function(){ rm.textContent="…"; push({kind:"remove_app",app_id:id}); });
      box.appendChild(re); box.appendChild(rm);
      c.appendChild(box);
    });
  }
  function renderSource(s){
    var a=card();
    a.appendChild(label("上游源","入库时按这个源拉清单"));
    a.appendChild(seg(s.manifest_sources,s.manifest_url,function(v){
      push({kind:"set_manifest_url",value:v});
    }));
    var b=card();
    b.appendChild(label("日志级别","写进 steamtools/host.log"));
    b.appendChild(seg(s.log_levels,s.log_level,function(v){
      push({kind:"set_log_level",value:v});
    }));
  }
  function renderLua(s){
    var def=card();
    def.appendChild(label(s.lua_dir||"","总是加载, 不可移除",true));
    def.appendChild(el("div","默认","font-size:12.5px;color:"+MUTE+";flex:none;"));
    (s.lua_paths||[]).forEach(function(p){
      var c=card();
      c.appendChild(label(p,null,true));
      var rm=btn("移除","stt-rm");
      onHit(rm,function(){ rm.textContent="…"; push({kind:"remove_lua_path",value:p}); });
      c.appendChild(rm);
    });
    var add=card();
    var inp=d.createElement("input");
    inp.className="stt-i";
    inp.placeholder="再加一个 lua 目录的绝对路径";
    inp.value=typed;
    inp.style.cssText="flex:1;min-width:0;background:rgba(0,0,0,.3);border:1px solid rgba(0,0,0,.4);"
      +"border-radius:2px;padding:8px 12px;color:"+TEXT+";font-family:"+MONO+";font-size:13.5px;"
      +"outline:none;transition:border-color .15s;";
    inp.addEventListener("input",function(){typed=inp.value;});
    var ab=btn("添加");
    function submit(){
      var v=(inp.value||"").trim();
      if(!v) return;
      inp.value="";typed="";ab.textContent="…";
      push({kind:"add_lua_path",value:v});
    }
    inp.addEventListener("keydown",function(e){if(e.key==="Enter")submit();});
    onHit(ab,submit);
    add.appendChild(inp);add.appendChild(ab);
  }
  function renderStatus(s){
    var tools=s.tools||[];
    var on=tools.filter(function(x){return x.enabled;}).length;
    [["调试通道",s.channel||"-","宿主与 Steam 界面之间的通道"],
     ["已入库应用",String(s.owned_count==null?"-":s.owned_count),"config/lua 里记下的 app"],
     ["规则版本",String(s.epoch==null?"-":s.epoch),"配置每变一次加一"],
     ["已启用工具",on+" / "+tools.length,""],
     ["版本","v"+(s.version||"?"),""]
    ].forEach(function(r){
      var c=card();
      c.appendChild(label(r[0],r[2]||null));
      c.appendChild(el("div",r[1],"font-family:"+MONO+";font-size:14px;color:#fff;flex:none;"));
    });
  }

  function render(s){
    navBox.textContent="";
    PAGES.forEach(function(p){
      var it=el("div",null,"display:flex;align-items:center;gap:12px;padding:10px 22px;"
        +"cursor:pointer;font-size:15px;line-height:20px;"
        +(p[0]===page
          ? "background:rgba(255,255,255,.08);color:#fff;box-shadow:inset 3px 0 0 "+ACCENT+";"
          : "color:#b6bcc2;"));
      it.className="stt-nav";
      it.tabIndex=0;
      it.appendChild(svg(p[2]));
      it.appendChild(el("span",p[1]));
      onHit(it,function(){ page=p[0]; render(s); });
      navBox.appendChild(it);
    });
    var name="";
    PAGES.forEach(function(p){ if(p[0]===page) name=p[1]; });
    title.textContent=name;
    body.textContent="";
    if(page==="tools") renderTools(s);
    else if(page==="apps") renderApps(s);
    else if(page==="source") renderSource(s);
    else if(page==="lua") renderLua(s);
    else renderStatus(s);
    if(s.note){
      var n=el("div",s.note,"margin-top:14px;padding:11px 16px;border-radius:3px;font-size:13.5px;"
        +"line-height:18px;"
        +(s.note.indexOf("失败")===0
          ? "background:rgba(190,60,60,.16);color:#f0a3a3;"
          : "background:rgba(26,159,255,.13);color:#9ed4ff;"));
      body.appendChild(n);
    }
  }
  window.__SteamToolsPanel={update:function(s){
    try{render(s);}catch(e){ title.textContent="SteamTools"; }
  }};
  render({});
  return "opened";
})()
"##;

/// 只摘面板, 留着入口 — 用户在别的视图里关掉时, 宿主用它同步其余视图.
pub const PANEL_CLOSE_JS: &str = r##"
(function(){
  window.__SteamToolsWant=false;
  var p=document.getElementById("stt-panel");
  if(!p){ window.__SteamToolsClosed=false; return "none"; }
  if(typeof window.__SteamToolsClose==="function"){
    // 走它自己的 shut, 事件监听才解得干净.
    window.__SteamToolsClose();
  }else if(p.remove){
    p.remove();
  }
  window.__SteamToolsClosed=false;
  return "closed";
})()
"##;

/// 工具关掉时摘掉入口与面板 — 留着一个点了没反应的入口比没有还糟.
pub const NAV_TEARDOWN_JS: &str = r##"
(function(){
  var d=document,n=0,i;
  var e=d.querySelectorAll("[data-stt-nav]");
  for(i=0;i<e.length;i++){ if(e[i].remove){ e[i].remove(); n++; } }
  var p=d.getElementById("stt-panel");
  if(p&&p.remove){ p.remove(); n++; }
  window.__SteamToolsWant=false;
  window.__SteamToolsClosed=false;
  window.__SteamToolsClose=null;
  return n?"removed":"off";
})()
"##;

/// 把快照推给已经打开的面板.
pub fn panel_update_js(snapshot_json: &str) -> String {
    format!(
        "(function(){{var p=window.__SteamToolsPanel;if(!p)return \"no-panel\";\
         p.update({snapshot_json});return \"ok\";}})()"
    )
}

/// 快照序列化; 失败就当没有快照可推.
pub fn snapshot_json(snapshot: &ConfigSnapshot) -> Option<String> {
    serde_json::to_string(snapshot).ok()
}

/// 能在某个页面里求值的东西 (ws 会话或管道会话); 面板逻辑因此只写一份.
pub(crate) trait EvalTarget {
    fn eval(&mut self, js: &str) -> Result<Value, String>;
}

/// 宿主这侧的面板后端.
pub trait PanelBridge {
    /// 配置页这一层开着没有.
    fn enabled(&mut self) -> bool;
    /// 当前要显示的快照 (面板开着时才要).
    fn snapshot(&mut self) -> Option<ConfigSnapshot>;
    /// 我们自己入库的 app —— 库右键每轮都要, 所以宿主该缓存, 别每轮扫盘.
    fn managed_apps(&mut self) -> Vec<u32>;
    /// 页面发回来的改动.
    fn on_intents(&mut self, intents: &[ConfigIntent]);
    /// 认不出导航行时的候选样本, 供勘察.
    fn on_recon(&mut self, sample: &str);
}

/// 跨轮次记住的东西: 同一份快照不重复推.
#[derive(Debug, Default)]
pub struct PanelState {
    last_json: std::collections::HashMap<String, String>,
    /// 上一轮工具是开着的.
    armed: bool,
    /// 面板该不该显示 —— 整个会话一份, 不是每个文档一份.
    open: bool,
    /// 上一轮各视图的样子, 用来挑面板画在哪个. 上一轮的数据就够 ——
    /// 从 Steam 起来就一直在收, 等用户点入口时早就齐了.
    views: Vec<ViewInfo>,
    /// 本轮正在收的; 一轮结束时顶替 `views`.
    pending: Vec<ViewInfo>,
    /// 上次推给各文档的"我们管着哪些 app", 没变就不重推.
    last_managed: std::collections::HashMap<String, String>,
}

/// 这一轮怎么对待某个视图.
///
/// 单独成型而不是两个相邻的 `bool` 参数: 那种调用点写反了也照样编译.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewRole {
    /// 配置页这一层开着; 关掉时只做收尾.
    pub enabled: bool,
    /// 这个文档里要找导航行挂入口 —— 只有客户端外壳有那一行.
    pub hosts_entry: bool,
}

/// 一个视图上一轮的样子.
#[derive(Debug, Clone)]
struct ViewInfo {
    key: String,
    /// 客户端外壳 (导航行在这儿), 而不是商店/社区那种网页视图.
    shell: bool,
    showing: bool,
}

impl PanelState {
    /// 新一轮开始: 上一轮收齐的顶替旧的.
    pub fn begin_round(&mut self) {
        if !self.pending.is_empty() {
            self.views = std::mem::take(&mut self.pending);
        }
        self.pending.clear();
    }

    fn note_view(&mut self, key: &str, shell: bool, showing: bool) {
        self.pending.push(ViewInfo {
            key: key.to_owned(),
            shell,
            showing,
        });
    }

    /// 面板画在哪个视图.
    ///
    /// 商店/社区是独立的 CEF 视图, 被合成在客户端文档之上 —— 它摆在面前时,
    /// 画在客户端文档里的会被整块盖住 (只从边上露一条). 所以显示中的网页视图优先,
    /// 没有才回落到客户端外壳.
    fn owner(&self) -> Option<&str> {
        // 头一轮还没有上一轮的数据, 那就用本轮已经收到的 —— 不然冷启动要白等一轮.
        let seen = if self.views.is_empty() {
            &self.pending
        } else {
            &self.views
        };
        seen.iter()
            .find(|v| !v.shell && v.showing)
            .or_else(|| seen.iter().find(|v| v.shell && v.showing))
            .map(|v| v.key.as_str())
    }
}

impl PanelState {
    fn forget(&mut self, key: &str) {
        self.last_json.remove(key);
    }

    /// 这轮要不要碰页面: 开着就碰; 刚关掉还得再碰一次把入口摘干净.
    pub fn should_run(&mut self, enabled: bool) -> bool {
        let run = enabled || self.armed;
        self.armed = enabled;
        run
    }
}

/// 一个目标走完一轮的结果.
pub(crate) struct PanelStepOutcome {
    pub tick: PanelTick,
    /// 这轮刚把面板打开.
    pub opened: bool,
    /// 这轮是这个文档报的"用户要看面板" —— 用来确认没人点时不该自己开.
    pub asked: bool,
    /// 那一下是怎么来的 (`click:user` / `click:synthetic`), 供诊断.
    pub asked_why: String,
}

/// 一个页面一轮: 补挂入口 → 收意图 → 按会话状态开关面板 → 推快照.
///
/// `enabled` 为假时只做一件事: 把上一次挂的东西摘掉.
///
/// 只在客户端外壳里跑 —— 面板本体会开成一个**真窗口**, 不再是页内浮层,
/// 所以不必往商店/社区那些 CEF 视图里各放一份.
pub(crate) fn panel_step(
    key: &str,
    target: &mut dyn EvalTarget,
    bridge: &mut dyn PanelBridge,
    state: &mut PanelState,
    role: ViewRole,
) -> Result<PanelStepOutcome, String> {
    if !role.enabled {
        let _ = target.eval(LIBRARY_MENU_TEARDOWN_JS);
        state.last_managed.remove(key);
        let res = target.eval(NAV_TEARDOWN_JS)?;
        state.forget(key);
        state.open = false;
        return Ok(PanelStepOutcome {
            tick: PanelTick {
                state: res.as_str().unwrap_or("off").to_owned(),
                ..PanelTick::default()
            },
            opened: false,
            asked: false,
            asked_why: String::new(),
        });
    }
    let tick = parse_panel_tick(&target.eval(&nav_tick_js(role.hosts_entry))?);
    if !tick.intents.is_empty() {
        bridge.on_intents(&tick.intents);
    }
    if let Some(sample) = tick.recon.as_deref() {
        bridge.on_recon(sample);
    }
    state.note_view(key, role.hosts_entry, tick.showing);
    // 库在客户端外壳里, 右键菜单只装那儿; 装一次, 之后每轮只推一次"哪些是我们的".
    if role.hosts_entry && tick.mounted() {
        if !tick.menu_armed {
            target.eval(LIBRARY_MENU_JS)?;
        }
        {
            let js = managed_apps_js(&bridge.managed_apps());
            if state.last_managed.get(key) != Some(&js) {
                target.eval(&js)?;
                state.last_managed.insert(key.to_owned(), js);
            }
        }
    }
    let mut just_asked = false;
    if tick.want {
        just_asked = !state.open;
        state.open = true;
    }
    if tick.closed {
        state.open = false;
    }
    if tick.state == "empty" {
        // 文档还是空壳 (Steam 先建窗口再渲染), 下轮再来.
        state.forget(key);
        return Ok(PanelStepOutcome {
            asked_why: tick.why.clone(),

            tick,
            opened: false,
            asked: just_asked,
        });
    }

    if !state.open || state.owner() != Some(key) {
        // 关掉了, 或者这个文档压根不该显示面板 —— 收干净.
        if tick.has {
            target.eval(PANEL_CLOSE_JS)?;
        }
        state.forget(key);
        return Ok(PanelStepOutcome {
            asked_why: tick.why.clone(),

            tick,
            opened: false,
            asked: just_asked,
        });
    }

    let opened = !tick.has;
    if opened {
        target.eval(PANEL_JS)?;
    }
    if let Some(json) = bridge.snapshot().as_ref().and_then(snapshot_json) {
        // 面板刚开或者内容真的变了才推, 否则 600ms 一轮白跑.
        if opened || state.last_json.get(key) != Some(&json) {
            target.eval(&panel_update_js(&json))?;
            state.last_json.insert(key.to_owned(), json);
        }
    }
    Ok(PanelStepOutcome {
        asked_why: tick.why.clone(),
        tick,
        opened,
        asked: just_asked,
    })
}

/// 一次 tick 的结果.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PanelTick {
    /// empty / already / nav / float, 供日志用.
    pub state: String,
    /// 用户点了入口, 想看面板.
    pub want: bool,
    /// 用户在这个视图里关掉了面板.
    pub closed: bool,
    /// 那一下"要看面板"是怎么来的 (`click:user` / `click:synthetic`), 供诊断.
    pub why: String,
    /// 这个视图此刻真的摆在用户面前 (可见且有实际尺寸).
    pub showing: bool,
    /// 库右键菜单已经装过监听了.
    pub menu_armed: bool,
    /// 面板已经在页面上.
    pub has: bool,
    pub intents: Vec<ConfigIntent>,
    /// 认不出来被丢掉的条数.
    pub dropped: usize,
    /// 认不出导航行时的候选样本.
    pub recon: Option<String>,
}

impl PanelTick {
    /// 入口挂上了没有 (already 也算挂着).
    pub fn mounted(&self) -> bool {
        matches!(self.state.as_str(), "nav" | "float" | "moved" | "already")
    }

    /// 这轮是不是刚挂上/搬过位置 (already 不算, 否则 600ms 一轮把日志刷爆).
    pub fn just_mounted(&self) -> bool {
        matches!(self.state.as_str(), "nav" | "float" | "moved")
    }
}

pub fn parse_panel_tick(v: &Value) -> PanelTick {
    let (intents, dropped) = parse_intents(v.get("q"));
    PanelTick {
        state: v
            .get("s")
            .and_then(Value::as_str)
            .unwrap_or("none")
            .to_owned(),
        want: v.get("want").and_then(Value::as_bool).unwrap_or(false),
        closed: v.get("closed").and_then(Value::as_bool).unwrap_or(false),
        why: v
            .get("why")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        // 只有可见且够大才算摆在面前: SharedJSContext 是 visible 的, 但只有 1x1.
        // 尺寸按 f64 读 —— 缩放下 innerWidth 会是小数, 按整数读会解析失败当成 0.
        menu_armed: v.get("menu").and_then(Value::as_bool).unwrap_or(false),
        showing: v.get("vis").and_then(Value::as_bool).unwrap_or(false)
            && surface_side(v, "w") >= MIN_SURFACE
            && surface_side(v, "h") >= MIN_SURFACE,
        has: v.get("has").and_then(Value::as_bool).unwrap_or(false),
        intents,
        dropped,
        recon: v
            .get("recon")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
    }
}

/// 页面回来的意图数组 → 已校验的意图; 认不出来的直接丢.
fn parse_intents(v: Option<&Value>) -> (Vec<ConfigIntent>, usize) {
    let Some(arr) = v.and_then(Value::as_array) else {
        return (Vec::new(), 0);
    };
    let mut out = Vec::new();
    let mut dropped = 0;
    for item in arr.iter().take(MAX_INTENTS) {
        match parse_one(item) {
            Some(intent) => out.push(intent),
            None => dropped += 1,
        }
    }
    dropped += arr.len().saturating_sub(MAX_INTENTS);
    (out, dropped)
}

/// 意图里的 app_id; 负数 / 越界 / 0 都不是合法 app.
fn app_id(item: &Value) -> Option<u32> {
    let id = item.get("app_id")?.as_u64()?;
    u32::try_from(id).ok().filter(|&id| id > 0)
}

fn parse_one(item: &Value) -> Option<ConfigIntent> {
    let kind = item.get("kind")?.as_str()?;
    let value = item.get("value").and_then(Value::as_str).unwrap_or("");
    match kind {
        "set_tool" => {
            let id = item.get("id")?.as_str()?;
            let on = item.get("on")?.as_bool()?;
            ConfigIntent::set_tool(id, on)
        }
        "set_log_level" => ConfigIntent::set_log_level(value),
        "set_manifest_url" => ConfigIntent::set_manifest_url(value),
        "add_lua_path" => ConfigIntent::add_lua_path(value),
        "remove_lua_path" => ConfigIntent::remove_lua_path(value),
        "refresh_app" => ConfigIntent::refresh_app(app_id(item)?),
        "remove_app" => ConfigIntent::remove_app(app_id(item)?),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use stt_config::ToolId;

    /// 只看面板相关的动作 —— 库右键那两下 (装监听 / 推 managed) 与面板无关.
    fn panel_acts(page: &FakePage) -> Vec<&'static str> {
        page.seen
            .iter()
            .copied()
            .filter(|a| !matches!(*a, "menu" | "managed" | "menu_off"))
            .collect()
    }

    /// 测试里的角色速写, 免得每处都展开字段.
    fn role(enabled: bool, hosts_entry: bool) -> ViewRole {
        ViewRole {
            enabled,
            hosts_entry,
        }
    }

    #[test]
    fn tick_reports_state_and_panel_flags() {
        let tick = parse_panel_tick(&json!({"s":"nav","want":true,"has":false,"q":[]}));
        assert_eq!(tick.state, "nav");
        assert!(tick.want);
        assert!(!tick.has);
        assert!(tick.just_mounted());
        assert!(tick.mounted());
    }

    /// already 是常态, 不能当成"刚挂上"再打一行日志.
    #[test]
    fn already_is_mounted_but_not_news() {
        let tick = parse_panel_tick(&json!({"s":"already"}));
        assert!(tick.mounted());
        assert!(!tick.just_mounted());
    }

    #[test]
    fn intents_are_parsed_and_validated() {
        let tick = parse_panel_tick(&json!({"s":"already","q":[
            {"kind":"set_tool","id":"library_ux","on":false},
            {"kind":"set_log_level","value":"info"},
            {"kind":"add_lua_path","value":"D:/lua"}
        ]}));
        assert_eq!(
            tick.intents,
            vec![
                ConfigIntent::SetTool {
                    id: ToolId::LibraryUx,
                    on: false
                },
                ConfigIntent::SetLogLevel("info".into()),
                ConfigIntent::AddLuaPath("D:/lua".into()),
            ]
        );
        assert_eq!(tick.dropped, 0);
    }

    /// 页面上的东西不全是我们的 — 认不出的一律丢掉, 不猜.
    #[test]
    fn unknown_intents_are_dropped() {
        let tick = parse_panel_tick(&json!({"q":[
            {"kind":"exec","value":"calc.exe"},
            {"kind":"set_tool","id":"nope","on":true},
            {"kind":"set_log_level","value":"verbose"},
            {"nope":1}
        ]}));
        assert!(tick.intents.is_empty());
        assert_eq!(tick.dropped, 4);
    }

    #[test]
    fn a_flood_of_intents_is_capped() {
        let q: Vec<Value> = (0..64)
            .map(|_| json!({"kind":"set_log_level","value":"warn"}))
            .collect();
        let tick = parse_panel_tick(&json!({ "q": q }));
        assert_eq!(tick.intents.len(), MAX_INTENTS);
        assert_eq!(tick.dropped, 64 - MAX_INTENTS);
    }

    #[test]
    fn missing_fields_do_not_panic() {
        let tick = parse_panel_tick(&json!({}));
        assert_eq!(tick.state, "none");
        assert!(!tick.mounted());
        assert!(tick.intents.is_empty());
        assert!(tick.recon.is_none());
    }

    /// 小窗口既不挂入口也不算挂上, 免得每个弹窗角上都多一块.
    #[test]
    fn small_windows_are_left_alone() {
        let tick = parse_panel_tick(&json!({"s":"small"}));
        assert!(!tick.mounted());
        assert!(!tick.just_mounted());
        assert!(NAV_TICK_JS.contains("innerWidth"));
    }

    /// Steam 刚起时窗口只有个空根节点; 这时候挂兜底就等于永远错过导航行.
    #[test]
    fn a_bare_document_is_not_a_mount_point() {
        let tick = parse_panel_tick(&json!({"s":"sparse"}));
        assert!(!tick.mounted());
        assert!(!tick.just_mounted());
        assert!(NAV_TICK_JS.contains("getElementsByTagName"));
    }

    /// 导航行后来渲染出来, 兜底要换成真 tab, 并且值得记一行.
    #[test]
    fn the_fallback_is_upgraded_when_the_row_shows_up() {
        let tick = parse_panel_tick(&json!({"s":"moved"}));
        assert!(tick.mounted());
        assert!(tick.just_mounted());
        assert!(NAV_TICK_JS.contains("data-stt-fallback"));
        assert!(NAV_TICK_JS.contains("__SteamToolsRescan"));
    }

    /// 菜单栏与标签行长得一样, 取文档顺序第一个会挂进菜单栏 (实测挂到了「帮助」后面).
    #[test]
    fn the_tab_row_is_scored_not_just_the_first_match() {
        assert!(NAV_TICK_JS.contains("bestScore"));
        assert!(NAV_TICK_JS.contains("h*4+r.top"));
    }

    #[test]
    fn tick_script_keeps_its_contract() {
        // 入口标记与队列名是宿主这侧解析的依据, 改名要同步.
        assert!(NAV_TICK_JS.contains("data-stt-nav"));
        assert!(NAV_TICK_JS.contains("__SteamToolsIntents"));
        assert!(NAV_TICK_JS.contains("__SteamToolsWant"));
        assert!(NAV_TICK_JS.contains("stt-panel"));
        // 已挂上时必须早退, 否则每轮都去扫全文档.
        assert!(NAV_TICK_JS.contains("[data-stt-nav]"));
    }

    #[test]
    fn panel_script_keeps_its_contract() {
        assert!(PANEL_JS.contains("__SteamToolsPanel"));
        assert!(PANEL_JS.contains("stt-panel"));
        assert!(PANEL_JS.contains("set_tool"));
        assert!(PANEL_JS.contains("add_lua_path"));
    }

    /// 配色全部取自 steamui/css, 不自创 —— 这东西住在 Steam 里, 得像客人一样穿衣服.
    #[test]
    fn every_colour_comes_from_steams_own_css() {
        // 卡片底与按钮底是它自己那两个值, 照抄才不出戏.
        assert!(PANEL_JS.contains("rgba(85,85,85,.0666666667)"));
        assert!(PANEL_JS.contains("hsla(0,0%,100%,.15)"));
        assert!(PANEL_JS.contains("#1a9fff"));
        assert!(PANEL_JS.contains("#23262e"));
        assert!(PANEL_JS.contains("#66c0f4"));
    }

    /// 等宽只给 ASCII 机器值; 中文一律 Motiva —— 也顺带躲开中文落进等宽的字形错乱.
    #[test]
    fn machine_values_are_set_in_mono() {
        assert!(PANEL_JS.contains("Motiva Sans"));
        assert!(PANEL_JS.contains("Consolas"));
    }

    /// 注入的样式必须全部收在 #stt-panel 里, 不能漏进 Steam 自己的界面.
    #[test]
    fn injected_css_stays_inside_our_page() {
        let opens = PANEL_JS.matches("#stt-panel ").count();
        assert!(opens >= 5, "样式没有逐条收口");
        assert!(!PANEL_JS.contains("\nbody{"));
    }

    /// 别再试着开真窗口: `BrowserView.CreatePopup` 造出的窗口归 Steam 的弹窗管理器,
    /// 它会把我们写进去的 DOM 重新渲染掉, 窗口还停在 1x1 (实测).
    #[test]
    fn the_panel_does_not_try_to_open_a_window() {
        assert!(!PANEL_JS.contains("window.open("));
        assert!(!PANEL_JS.contains("BrowserView.CreatePopup("));
    }

    /// 挑视图要看"可见 + 够大"两样: SharedJSContext 是 visible 的, 但只有 1x1.
    #[test]
    fn the_tick_reports_what_it_takes_to_pick_a_view() {
        assert!(NAV_TICK_JS.contains("visibilityState"));
        assert!(NAV_TICK_JS.contains("out.w=window.innerWidth"));
        assert!(!parse_panel_tick(&json!({"vis":true,"w":1,"h":1})).showing);
        assert!(parse_panel_tick(&json!({"vis":true,"w":1200,"h":800})).showing);
        assert!(!parse_panel_tick(&json!({"vis":false,"w":1200,"h":800})).showing);
    }

    /// 别用新语法写关键的布局: 认不出 `inset:0` 就整块不可见 (实测栽过一次).
    #[test]
    fn layout_avoids_shorthand_cef_might_not_know() {
        assert!(!PANEL_JS.contains("inset:0"));
        // 只挑 CSS 里的 min(); JS 的 Math.min 与注释不算.
        assert!(!PANEL_JS.contains("width:min("));
        assert!(!PANEL_JS.contains("height:min("));
        assert!(PANEL_JS.contains("left:0;top:0;right:0;bottom:0"));
    }

    /// 弹窗: 居中浮层 + 遮罩, 关的路子要留够 (✕ / 点遮罩 / Esc / 再点入口).
    #[test]
    fn the_dialog_can_always_be_closed() {
        assert!(PANEL_JS.contains("rgba(0,0,0,.6)"));
        assert!(PANEL_JS.contains("Escape"));
        assert!(PANEL_JS.contains("e.target===root"));
        assert!(PANEL_JS.contains("__SteamToolsClose"));
        assert!(NAV_TICK_JS.contains("__SteamToolsClose"));
        // 层级要压过内容区, 又不能盖掉 Steam 自己的菜单.
        assert!(PANEL_JS.contains("z-index:9000"));
    }

    /// 入口要紧挨着用户名; margin-left:auto 会把它顶到行尾去.
    #[test]
    fn the_entry_sits_next_to_its_neighbour() {
        assert!(!NAV_TICK_JS.contains("marginLeft=\"auto\""));
        assert!(NAV_TICK_JS.contains("__SteamToolsNavRow"));
    }

    /// 左侧分区栏 + 右侧卡片行, 与 Steam 设置对话框同一种骨架.
    #[test]
    fn the_dialog_is_shaped_like_steam_settings() {
        assert!(PANEL_JS.contains("STEAMTOOLS 设置"));
        for page in ["\"tools\"", "\"source\"", "\"lua\"", "\"status\""] {
            assert!(PANEL_JS.contains(page), "缺分区 {page}");
        }
        // 当前分区: 浅底 + 左侧一道强调色, 照它的做法.
        assert!(PANEL_JS.contains("inset 3px 0 0 "));
    }

    /// 开关是画出来的圆轨 + 白钮, 不是一颗写着字的按钮.
    #[test]
    fn tools_are_switched_not_buttoned() {
        assert!(PANEL_JS.contains("border-radius:11px"));
        assert!(PANEL_JS.contains("border-radius:50%"));
        assert!(PANEL_JS.contains("role\",\"switch\""));
    }

    /// 键盘要能走完全程: 开关/选项/移除/添加/返回都不是原生控件.
    #[test]
    fn everything_is_reachable_by_keyboard() {
        let tabbable = PANEL_JS.matches("tabIndex=0").count();
        assert!(tabbable >= 5, "只有 {tabbable} 处可聚焦");
        assert!(PANEL_JS.contains("focus-visible"));
    }

    /// 输入到一半的路径不能被快照重绘冲掉.
    #[test]
    fn the_page_keeps_its_own_state_across_redraws() {
        assert!(PANEL_JS.contains("var typed=\"\""));
    }

    #[test]
    fn update_script_carries_the_snapshot() {
        let js = panel_update_js("{\"owned_count\":3}");
        assert!(js.contains("__SteamToolsPanel"));
        assert!(js.contains("{\"owned_count\":3}"));
    }

    /// 假页面: 记下宿主发过来的每段脚本, tick 的返回值由测试摆布.
    struct FakePage {
        tick: Value,
        seen: Vec<&'static str>,
    }

    impl FakePage {
        /// 默认就是"摆在用户面前"的视图; 要测隐藏视图的用例自己写 vis/w/h.
        fn new(mut tick: Value) -> Self {
            if let Some(o) = tick.as_object_mut() {
                o.entry("vis").or_insert(json!(true));
                o.entry("w").or_insert(json!(1200));
                o.entry("h").or_insert(json!(800));
            }
            Self {
                tick,
                seen: Vec::new(),
            }
        }
    }

    impl EvalTarget for FakePage {
        fn eval(&mut self, js: &str) -> Result<Value, String> {
            // tick 脚本带一段前缀 (挂不挂入口的开关), 所以按包含判断.
            if js.ends_with(NAV_TICK_JS) {
                self.seen.push("tick");
                return Ok(self.tick.clone());
            }
            if js == PANEL_JS {
                self.seen.push("open");
                return Ok(json!("popup"));
            }
            if js == NAV_TEARDOWN_JS {
                self.seen.push("teardown");
                return Ok(json!("removed"));
            }
            if js == PANEL_CLOSE_JS {
                self.seen.push("close");
                return Ok(json!("closed"));
            }
            if js == LIBRARY_MENU_JS {
                self.seen.push("menu");
                return Ok(json!("armed"));
            }
            if js == LIBRARY_MENU_TEARDOWN_JS {
                self.seen.push("menu_off");
                return Ok(json!("off"));
            }
            if js.starts_with("window.__SteamToolsManaged=") {
                self.seen.push("managed");
                return Ok(Value::Null);
            }
            self.seen.push("update");
            Ok(json!("ok"))
        }
    }

    #[derive(Default)]
    struct FakeHost {
        /// 算过几次整份快照 —— 它要扫目录, 不该在面板关着时发生.
        snapshots: usize,
        note: String,
        got: Vec<ConfigIntent>,
        recon: Vec<String>,
    }

    impl PanelBridge for FakeHost {
        fn enabled(&mut self) -> bool {
            true
        }
        fn snapshot(&mut self) -> Option<ConfigSnapshot> {
            self.snapshots += 1;
            Some(ConfigSnapshot::from_state(
                &stt_config::ConfigState::new(),
                std::path::Path::new("C:/steam"),
                "pipe",
                &self.note,
            ))
        }
        fn managed_apps(&mut self) -> Vec<u32> {
            Vec::new()
        }
        fn on_intents(&mut self, intents: &[ConfigIntent]) {
            self.got.extend_from_slice(intents);
        }
        fn on_recon(&mut self, sample: &str) {
            self.recon.push(sample.to_owned());
        }
    }

    #[test]
    fn clicking_the_entry_opens_the_panel_and_fills_it() {
        let mut page = FakePage::new(json!({"s":"already","want":true,"has":false}));
        let mut host = FakeHost::default();
        let mut state = PanelState::default();
        let out = panel_step("t1", &mut page, &mut host, &mut state, role(true, true)).unwrap();
        assert!(out.opened);
        assert_eq!(panel_acts(&page), vec!["tick", "open", "update"]);
    }

    /// 面板开着但内容没变就别推 — 600ms 一轮, 白推就是白烧.
    #[test]
    fn an_unchanged_snapshot_is_not_pushed_twice() {
        let mut page = FakePage::new(json!({"s":"already","want":true,"has":true}));
        let mut host = FakeHost::default();
        let mut state = PanelState::default();
        panel_step("t1", &mut page, &mut host, &mut state, role(true, true)).unwrap();
        panel_step("t1", &mut page, &mut host, &mut state, role(true, true)).unwrap();
        assert_eq!(panel_acts(&page), vec!["tick", "update", "tick"]);

        host.note = "已保存".into();
        panel_step("t1", &mut page, &mut host, &mut state, role(true, true)).unwrap();
        assert_eq!(panel_acts(&page).last(), Some(&"update"));
    }

    /// 面板没开就只 tick, 不该往页面里塞任何东西.
    #[test]
    fn a_closed_panel_costs_one_call() {
        let mut page = FakePage::new(json!({"s":"already","want":false,"has":false}));
        let mut host = FakeHost::default();
        let mut state = PanelState::default();
        panel_step("t1", &mut page, &mut host, &mut state, role(true, true)).unwrap();
        assert_eq!(panel_acts(&page), vec!["tick"]);
    }

    /// 窗口刚建出来时 body 是空的, 这轮什么都别做.
    #[test]
    fn an_empty_document_is_left_alone() {
        let mut page = FakePage::new(json!({"s":"empty","want":true,"has":false}));
        let mut host = FakeHost::default();
        let mut state = PanelState::default();
        let out = panel_step("t1", &mut page, &mut host, &mut state, role(true, true)).unwrap();
        assert!(!out.opened);
        assert_eq!(panel_acts(&page), vec!["tick"]);
    }

    /// 商店视图摆在面前时, 面板必须画进它 —— 画在客户端文档里会被它整块盖住.
    #[test]
    fn the_panel_goes_into_the_view_on_top() {
        let mut state = PanelState::default();
        let mut host = FakeHost::default();

        // 先跑一轮把两个视图的样子收齐 (商店摆在面前).
        state.begin_round();
        let mut shell = FakePage::new(json!({"s":"already","want":true}));
        panel_step("shell", &mut shell, &mut host, &mut state, role(true, true)).unwrap();
        let mut store = FakePage::new(json!({"s":"guest"}));
        panel_step(
            "store",
            &mut store,
            &mut host,
            &mut state,
            role(true, false),
        )
        .unwrap();

        // 下一轮: 面板该落在商店视图, 不该落在外壳.
        state.begin_round();
        let mut shell2 = FakePage::new(json!({"s":"already"}));
        let a = panel_step(
            "shell",
            &mut shell2,
            &mut host,
            &mut state,
            role(true, true),
        )
        .unwrap();
        let mut store2 = FakePage::new(json!({"s":"guest"}));
        let b = panel_step(
            "store",
            &mut store2,
            &mut host,
            &mut state,
            role(true, false),
        )
        .unwrap();
        assert!(!a.opened, "面板落进了会被盖住的客户端文档");
        assert!(b.opened, "面板没落到最上面那个视图");
    }

    /// 商店视图收起来之后, 面板要回到客户端外壳.
    #[test]
    fn the_panel_falls_back_to_the_shell() {
        let mut state = PanelState::default();
        let mut host = FakeHost::default();
        state.begin_round();
        let mut shell = FakePage::new(json!({"s":"already","want":true}));
        panel_step("shell", &mut shell, &mut host, &mut state, role(true, true)).unwrap();
        // 商店视图这轮是隐藏的.
        let mut store = FakePage::new(json!({"s":"guest","vis":false}));
        panel_step(
            "store",
            &mut store,
            &mut host,
            &mut state,
            role(true, false),
        )
        .unwrap();

        state.begin_round();
        let mut shell2 = FakePage::new(json!({"s":"already"}));
        let a = panel_step(
            "shell",
            &mut shell2,
            &mut host,
            &mut state,
            role(true, true),
        )
        .unwrap();
        assert!(a.opened, "商店收起来了, 面板没回到外壳");
    }

    /// SharedJSContext 报 visible 但只有 1x1, 画进去等于没画.
    #[test]
    fn a_pinhole_view_is_never_chosen() {
        let mut state = PanelState::default();
        let mut host = FakeHost::default();
        state.begin_round();
        let mut ctx = FakePage::new(json!({"s":"guest","want":true,"w":1,"h":1}));
        panel_step("ctx", &mut ctx, &mut host, &mut state, role(true, false)).unwrap();
        state.begin_round();
        let mut ctx2 = FakePage::new(json!({"s":"guest","w":1,"h":1}));
        let out = panel_step("ctx", &mut ctx2, &mut host, &mut state, role(true, false)).unwrap();
        assert!(!out.opened, "面板画进了 1x1 的视图");
    }

    /// 库右键只对我们加的 app 生效 —— 用户自己的游戏要照常弹 Steam 的菜单.
    #[test]
    fn the_library_menu_defers_to_steam_for_other_games() {
        assert!(LIBRARY_MENU_JS.contains("managed.indexOf(id)<0) return"));
        assert!(LIBRARY_MENU_JS.contains("preventDefault"));
        // 认 app 靠图片路径, 不靠混淆的类名.
        assert!(LIBRARY_MENU_JS.contains("(?:assets|apps)"));
    }

    /// 菜单挂一次就够, tick 要能报出来, 否则每轮重装一遍监听.
    #[test]
    fn the_library_menu_is_armed_once() {
        assert!(NAV_TICK_JS.contains("out.menu="));
        assert!(parse_panel_tick(&json!({"menu":true})).menu_armed);
        assert!(!parse_panel_tick(&json!({})).menu_armed);
    }

    /// 面板关着时不该去算整份快照 —— 那里面的受管列表要扫目录, 每轮来太贵.
    #[test]
    fn a_closed_panel_costs_no_snapshot() {
        let mut state = PanelState::default();
        let mut host = FakeHost::default();
        let mut page = FakePage::new(json!({"s":"already","menu":true}));
        panel_step("shell", &mut page, &mut host, &mut state, role(true, true)).unwrap();
        assert_eq!(host.snapshots, 0, "面板没开却算了快照");
        assert!(page.seen.contains(&"managed"), "受管列表还是要推的");
    }

    #[test]
    fn managed_ids_reach_the_page_as_a_literal_array() {
        assert_eq!(
            managed_apps_js(&[7, 42]),
            "window.__SteamToolsManaged=[7,42];"
        );
        assert_eq!(managed_apps_js(&[]), "window.__SteamToolsManaged=[];");
    }

    #[test]
    fn app_intents_are_parsed() {
        let tick = parse_panel_tick(&json!({"q":[
            {"kind":"refresh_app","app_id":730},
            {"kind":"remove_app","app_id":440}
        ]}));
        assert_eq!(
            tick.intents,
            vec![ConfigIntent::RefreshApp(730), ConfigIntent::RemoveApp(440)]
        );
    }

    /// app_id 缺失 / 为 0 / 越界都不是合法目标.
    #[test]
    fn bad_app_ids_are_dropped() {
        let tick = parse_panel_tick(&json!({"q":[
            {"kind":"remove_app"},
            {"kind":"remove_app","app_id":0},
            {"kind":"refresh_app","app_id":99999999999u64}
        ]}));
        assert!(tick.intents.is_empty());
        assert_eq!(tick.dropped, 3);
    }

    /// 用户关掉面板 (tick 报 closed), 宿主要认账, 否则会以为还开着, 再也不给开.
    #[test]
    fn closing_lets_it_be_reopened() {
        let mut state = PanelState::default();
        let mut host = FakeHost::default();
        state.begin_round();
        let mut p0 = FakePage::new(json!({"s":"already"}));
        panel_step("shell", &mut p0, &mut host, &mut state, role(true, true)).unwrap();

        state.begin_round();
        let mut p1 = FakePage::new(json!({"s":"already","want":true,"has":false}));
        let out = panel_step("shell", &mut p1, &mut host, &mut state, role(true, true)).unwrap();
        assert!(out.opened);

        state.begin_round();
        let mut p2 = FakePage::new(json!({"s":"already","closed":true,"has":false}));
        panel_step("shell", &mut p2, &mut host, &mut state, role(true, true)).unwrap();

        state.begin_round();
        let mut p3 = FakePage::new(json!({"s":"already","want":true,"has":false}));
        let out = panel_step("shell", &mut p3, &mut host, &mut state, role(true, true)).unwrap();
        assert!(out.opened, "关掉之后再也开不出来了");
    }

    /// 工具关掉要把入口摘干净: 留个点了没反应的入口比没有还糟.
    #[test]
    fn turning_the_tool_off_takes_the_entry_down() {
        let mut page = FakePage::new(json!({"s":"already","want":false,"has":true}));
        let mut host = FakeHost::default();
        let mut state = PanelState::default();
        let out = panel_step("t1", &mut page, &mut host, &mut state, role(false, true)).unwrap();
        assert_eq!(panel_acts(&page), vec!["teardown"]);
        assert_eq!(out.tick.state, "removed");
    }

    /// 关掉之后只收尾一轮, 之后别再碰页面.
    #[test]
    fn the_teardown_round_happens_once() {
        let mut state = PanelState::default();
        assert!(state.should_run(true));
        assert!(state.should_run(false));
        assert!(!state.should_run(false));
        assert!(state.should_run(true));
    }

    #[test]
    fn intents_and_recon_reach_the_host() {
        let mut page = FakePage::new(json!({
            "s":"float","recon":"DIV.foo kids=4",
            "q":[{"kind":"set_tool","id":"catalog_add","on":false}]
        }));
        let mut host = FakeHost::default();
        let mut state = PanelState::default();
        panel_step("t1", &mut page, &mut host, &mut state, role(true, true)).unwrap();
        assert_eq!(
            host.got,
            vec![ConfigIntent::SetTool {
                id: ToolId::CatalogAdd,
                on: false
            }]
        );
        assert_eq!(host.recon, vec!["DIV.foo kids=4"]);
    }

    #[test]
    fn snapshot_serializes_for_the_page() {
        let state = stt_config::ConfigState::new();
        let snap =
            ConfigSnapshot::from_state(&state, std::path::Path::new("C:/steam"), "pipe", "ok");
        let json = snapshot_json(&snap).unwrap();
        assert!(json.contains("\"config_ui\""), "{json}");
        assert!(json.contains("\"channel\":\"pipe\""), "{json}");
    }
}
