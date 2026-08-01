//! CDP over DevTools pipe: 不开端口的调试通道 (ADR 0010 B 档).
//!
//! 与 `cdp_bridge` 的区别只在传输与寻址:
//!
//! - 传输: 匿名管道上的 `\0` 分隔裸 JSON, 没有 HTTP 也没有 WebSocket 帧.
//! - 寻址: pipe 模式没有 `/json` 列表, 页面要靠 `Target.getTargets` 找,
//!   再用 `Target.attachToTarget{flatten:true}` 拿 `sessionId`; 之后每条消息带
//!   上它就等价于原来"连到某个 page 的 ws".
//!
//! 注入脚本与 pending 解析仍复用 `cdp_bridge`, 这里只负责把话送到.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use stt_platform::DevToolsPipe;

use crate::cdp_bridge::{
    hosts_nav_entry, is_mount_news, is_popup_menu_target, is_store_app_url, log_panel_step,
    parse_pending_app_ids, wants_panel_tick, StoreCdpPoll, DRAIN_JS,
};
use crate::config_panel::{
    apply_library_menu_drain, library_menu_inject_js, panel_step, EvalTarget, PanelBridge,
    PanelState, ViewRole, LIBRARY_MENU_DRAIN_JS,
};

/// 单次 CDP 调用的等待上限.
///
/// 比 ws 版的 1.5s 宽松一点: 管道没有连接建立开销, 但 CEF 忙时回得慢,
/// 卡死风险由读线程隔离, 不会拖住轮询线程之外的东西.
const CALL_TIMEOUT: Duration = Duration::from_millis(2500);

/// 通道满时被丢弃的 CDP 帧数 (事件流, 丢帧可接受, 计数供诊断).
static DROPPED_FRAMES: AtomicU64 = AtomicU64::new(0);

/// 一个 CDP 目标 (页面 / iframe).
#[derive(Debug, Clone)]
pub struct PipeTarget {
    pub target_id: String,
    pub kind: String,
    pub url: String,
    pub title: String,
}

/// 挂在 DevTools 管道上的 browser 级 CDP 会话.
pub struct CdpPipeSession {
    pipe: Arc<DevToolsPipe>,
    rx: mpsc::Receiver<Value>,
    next_id: u64,
    /// 已收到但当前调用不认领的消息 (事件等); 留着免得丢掉别人的回复.
    parked: VecDeque<Value>,
}

impl CdpPipeSession {
    /// 接管管道并起读线程.
    ///
    /// 匿名管道的 `ReadFile` 是阻塞的且没有超时, 所以读必须独占一个线程 —
    /// 否则 CEF 一不吭声就会把整条轮询线程钉死.
    pub fn new(pipe: DevToolsPipe) -> Self {
        let pipe = Arc::new(pipe);
        // 事件流, 满时丢帧可接受: 有界通道封顶读线程积压.
        let (tx, rx) = mpsc::sync_channel(64);
        let reader = Arc::clone(&pipe);
        // 起不来线程就没人收回复, 后续调用会全部超时 → 验活失败 → 回退端口模式.
        // 这条降级路径本来就有, 所以这里不必额外处理.
        let _ = std::thread::Builder::new()
            .name("stt-cdp-pipe".into())
            .spawn(move || read_loop(&reader, &tx));
        Self {
            pipe,
            rx,
            next_id: 0,
            parked: VecDeque::new(),
        }
    }

    /// 发一条命令并等它的回复. `session` 为 `None` 时走 browser 级.
    pub fn call(
        &mut self,
        method: &str,
        params: Option<Value>,
        session: Option<&str>,
    ) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let mut msg = json!({"id": id, "method": method});
        if let Some(p) = params {
            msg["params"] = p;
        }
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        self.pipe
            .send(&msg.to_string())
            .map_err(|e| format!("pipe send: {e}"))?;

        // 先翻一遍暂存区: 上一次调用可能已经把这条回复收下来了.
        if let Some(v) = self.take_parked(id) {
            return unwrap_reply(&v);
        }
        let deadline = std::time::Instant::now() + CALL_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(format!("pipe timeout: {method}"));
            }
            let v = self
                .rx
                .recv_timeout(left)
                .map_err(|_| format!("pipe timeout: {method}"))?;
            if v.get("id").and_then(Value::as_u64) == Some(id) {
                return unwrap_reply(&v);
            }
            // 不是给我们的 (事件, 或别的 id): 存着别丢.
            if self.parked.len() < 256 {
                self.parked.push_back(v);
            }
        }
    }

    fn take_parked(&mut self, id: u64) -> Option<Value> {
        let pos = self
            .parked
            .iter()
            .position(|v| v.get("id").and_then(Value::as_u64) == Some(id))?;
        self.parked.remove(pos)
    }

    /// 列出当前所有目标 (等价于 ws 版的 `GET /json`).
    pub fn targets(&mut self) -> Result<Vec<PipeTarget>, String> {
        let r = self.call("Target.getTargets", None, None)?;
        let arr = r
            .get("targetInfos")
            .and_then(Value::as_array)
            .ok_or("no targetInfos")?;
        Ok(arr
            .iter()
            .map(|t| PipeTarget {
                target_id: str_field(t, "targetId"),
                kind: str_field(t, "type"),
                url: str_field(t, "url"),
                title: str_field(t, "title"),
            })
            .collect())
    }

    /// 附着到目标, 拿到后续消息要带的 `sessionId`.
    ///
    /// `flatten: true` 是关键: 否则回复得靠 `Target.receivedMessageFromTarget`
    /// 事件套一层, 那套 API 已废弃.
    pub fn attach(&mut self, target_id: &str) -> Result<String, String> {
        let r = self.call(
            "Target.attachToTarget",
            Some(json!({"targetId": target_id, "flatten": true})),
            None,
        )?;
        r.get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "no sessionId".into())
    }

    pub fn detach(&mut self, session: &str) {
        let _ = self.call(
            "Target.detachFromTarget",
            Some(json!({"sessionId": session})),
            None,
        );
    }

    /// 在某个会话里求值并按值取回.
    ///
    /// `awaitPromise` 必须 false — 与 ws 版同因: 注入脚本不是 Promise,
    /// true 会在 CEF 上一直挂起.
    ///
    /// `userGesture` 同样与 ws 版一致: 没有它 `window.open` 会被当成非用户触发的
    /// 弹窗拦掉, 配置页就开不成真窗口.
    pub fn eval_value(&mut self, session: &str, expression: &str) -> Result<Value, String> {
        let result = self.call(
            "Runtime.evaluate",
            Some(json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": false,
                "userGesture": true,
            })),
            Some(session),
        )?;
        let r = result.get("result").cloned().unwrap_or(Value::Null);
        if r.get("subtype").and_then(Value::as_str) == Some("error") {
            return Err(format!("js error: {r}"));
        }
        Ok(r.get("value").cloned().unwrap_or(Value::Null))
    }
}

/// 页面自检: 是不是商店的 app 页. 与 ws 版同一段脚本, 行为保持一致.
const PROBE_JS: &str = r#"(function(){var h=String(location.href||"");var p=String(location.pathname||"");return /store\.steampowered\.com/.test(h)&&/^\/app\/\d+(?:\/|$)/.test(p);})()"#;

/// 一次轮询: 找商店页 → 注入 → 取回 pending.
pub fn poll_store_pipe(session: &mut CdpPipeSession, inject_js: &str) -> StoreCdpPoll {
    let mut out = StoreCdpPoll::default();
    let targets = match session.targets() {
        Ok(t) => t,
        Err(e) => {
            out.notes.push(format!("cdp_list_err={e}"));
            return out;
        }
    };
    // 能应答 Target.getTargets 就说明通道是活的.
    out.cdp_up = true;

    let mut samples: Vec<String> = Vec::new();
    for t in &targets {
        if t.kind != "page" && t.kind != "iframe" {
            continue;
        }
        // 与 ws 版一致: 跳过明显无用的菜单空白页.
        if t.url.starts_with("about:blank")
            && (t.title.contains("Menu") || t.title.contains("Supernav"))
        {
            continue;
        }
        if samples.len() < 8 {
            samples.push(format!(
                "{}||{}",
                t.title.chars().take(24).collect::<String>(),
                t.url.chars().take(80).collect::<String>()
            ));
        }
        // pipe 模式能直接拿到 url, 不必像 ws 版那样对所有 page 盲试.
        if !is_store_app_url(&t.url) {
            continue;
        }
        match inject_one(session, &t.target_id, inject_js) {
            // 商店首页 / 促销页不是 app 页, 是常态而非错误 — 记 note 会刷屏.
            Ok(Injected::NotAnAppPage) => {}
            Ok(Injected::Done {
                mounted,
                removed,
                pending,
            }) => {
                out.store_pages += 1;
                // 摘按钮不算注入, 否则关掉工具反而在日志里像挂上了.
                if removed {
                    out.notes.push(format!("cdp_teardown ok url={}", t.url));
                } else if mounted {
                    out.injected += 1;
                    out.notes.push(format!("cdp_injected ok url={}", t.url));
                }
                // 管道模式 store_targets 存 target id, 回写按钮时 attach 用.
                if !out.store_targets.contains(&t.target_id) {
                    out.store_targets.push(t.target_id.clone());
                }
                out.pending_app_ids.extend(pending);
            }
            Err(e) => out.notes.push(format!("cdp_page_err={e} url={}", t.url)),
        }
    }

    if out.store_pages == 0 && out.injected == 0 && out.notes.is_empty() {
        out.notes.push(format!(
            "cdp_store_pages=0 total={} samples={}",
            targets.len(),
            samples.join(" | ")
        ));
    }
    out
}

/// 一次注入的结果. 区分"不是 app 页"与"出错" — 前者是常态.
enum Injected {
    /// 页面在商店域下但不是 `/app/<id>`, 无事可做.
    NotAnAppPage,
    Done {
        mounted: bool,
        removed: bool,
        pending: Vec<u32>,
    },
}

/// 附着 → 自检 → 注入 → 取队列 → 脱离.
///
/// 无论中途哪步失败都要 detach, 否则 CEF 侧会攒下一堆僵尸会话.
fn inject_one(
    session: &mut CdpPipeSession,
    target_id: &str,
    inject_js: &str,
) -> Result<Injected, String> {
    let sid = session.attach(target_id)?;
    let r = inject_in_session(session, &sid, inject_js);
    session.detach(&sid);
    r
}

fn inject_in_session(
    session: &mut CdpPipeSession,
    sid: &str,
    inject_js: &str,
) -> Result<Injected, String> {
    match session.eval_value(sid, PROBE_JS) {
        Ok(v) if v.as_bool() == Some(true) => {}
        Ok(_) => return Ok(Injected::NotAnAppPage),
        Err(e) => return Err(format!("probe: {e}")),
    }
    let res = session
        .eval_value(sid, inject_js)
        .map_err(|e| format!("eval inject: {e}"))?;
    let mounted = is_mount_news(&res);
    let removed = res.as_str() == Some("removed");
    let pending = match session.eval_value(sid, DRAIN_JS) {
        Ok(p) => parse_pending_app_ids(&p),
        Err(_) => Vec::new(),
    };
    Ok(Injected::Done {
        mounted,
        removed,
        pending,
    })
}

/// 管道模式: 对已知 target 回写入库结果到按钮.
fn push_store_feedback_pipe(
    session: &mut CdpPipeSession,
    targets: &[String],
    js: &str,
    on_log: &mut dyn FnMut(String),
) {
    if js.is_empty() || targets.is_empty() {
        return;
    }
    for tid in targets {
        let Ok(sid) = session.attach(tid) else {
            continue;
        };
        if let Err(e) = session.eval_value(&sid, js) {
            on_log(format!("catalog_add=feedback_err {e}"));
        }
        session.detach(&sid);
    }
}

/// 管道会话上的一个页面; 有了它面板逻辑就能跟端口版共用一份.
struct PipeEval<'a> {
    session: &'a mut CdpPipeSession,
    sid: &'a str,
}

impl EvalTarget for PipeEval<'_> {
    fn eval(&mut self, js: &str) -> Result<Value, String> {
        self.session.eval_value(self.sid, js)
    }
}

/// 配置页一轮 (管道通道).
pub(crate) fn poll_panel_pipe(
    session: &mut CdpPipeSession,
    bridge: &mut dyn PanelBridge,
    state: &mut PanelState,
    enabled: bool,
    on_log: &mut dyn FnMut(String),
) {
    // 列不出来说明这轮通道不通; 商店那条路已经在报了.
    let Ok(targets) = session.targets() else {
        return;
    };
    state.begin_round();
    for t in &targets {
        if t.kind != "page" && t.kind != "iframe" {
            continue;
        }
        if !wants_panel_tick(&t.url, &t.title) {
            continue;
        }
        let Ok(sid) = session.attach(&t.target_id) else {
            continue;
        };
        let role = ViewRole {
            enabled,
            hosts_entry: hosts_nav_entry(&t.url, &t.title),
        };
        let stepped = {
            let mut page = PipeEval { session, sid: &sid };
            panel_step(&t.target_id, &mut page, bridge, state, role)
        };
        session.detach(&sid);
        match stepped {
            Ok(out) => log_panel_step(&out, &t.title, on_log),
            Err(e) => on_log(format!("config_ui=page_err {e}")),
        }
    }
    // 右键后 Steam 才创建独立 popup; 重新取一次 target, 不必等下一轮.
    let refreshed = if state.pending_menu().is_some() {
        session.targets().ok().filter(|targets| !targets.is_empty())
    } else {
        None
    };
    let menu_targets = refreshed.as_deref().unwrap_or(&targets);
    poll_library_menu_pipe(session, menu_targets, bridge, state, on_log);
}

fn poll_library_menu_pipe(
    session: &mut CdpPipeSession,
    targets: &[PipeTarget],
    bridge: &mut dyn PanelBridge,
    state: &mut PanelState,
    on_log: &mut dyn FnMut(String),
) {
    if let Some((key, app_id)) = state
        .active_menu()
        .map(|(key, app_id)| (key.to_owned(), app_id))
    {
        let Some(target) = targets.iter().find(|target| target.target_id == key) else {
            state.clear_active_menu();
            return;
        };
        let result = session.attach(&target.target_id).and_then(|sid| {
            let value = session.eval_value(&sid, LIBRARY_MENU_DRAIN_JS);
            session.detach(&sid);
            value
        });
        match result {
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
    if let Some(target) = targets.iter().find(|target| target.target_id == source_key) {
        let result = session.attach(&target.target_id).and_then(|sid| {
            let value = session.eval_value(&sid, &library_menu_inject_js(app_id, point));
            session.detach(&sid);
            value
        });
        if let Ok(value) = result {
            let status = value.get("s").and_then(Value::as_str).unwrap_or("invalid");
            if matches!(status, "injected" | "already") {
                state.activate_menu(&target.target_id, app_id);
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
        let result = session.attach(&target.target_id).and_then(|sid| {
            let value = session.eval_value(&sid, &library_menu_inject_js(app_id, None));
            session.detach(&sid);
            value
        });
        let Ok(value) = result else {
            continue;
        };
        let status = value.get("s").and_then(Value::as_str).unwrap_or("invalid");
        if matches!(status, "injected" | "already") {
            state.activate_menu(&target.target_id, app_id);
            on_log(format!(
                "config_ui=library_menu {status} app_id={app_id} title={}",
                target.title.chars().take(32).collect::<String>()
            ));
            return;
        }
        if status != "hidden" {
            on_log(format!(
                "config_ui=library_menu waiting state={status} title={}",
                target.title.chars().take(32).collect::<String>()
            ));
        }
    }
}

/// 等 detour 把管道交出来 (webhelper 得先被拉起来).
fn wait_for_pipe(timeout: Duration) -> Option<DevToolsPipe> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(p) = crate::cef_debug::take_devtools_pipe() {
            return Some(p);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// pipe 版商店桥.
///
/// 返回 `false` = 这条路没跑通, 调用方应回退到端口模式并记下来.
/// 返回只发生在"确定不可用"时; 一旦跑通就长驻不返回.
///
/// 回调用 `&mut dyn` 而非泛型: 调用方要在本函数返回 `false` 后把同一组闭包原样
/// 交给端口版循环, 借用比按值传更顺手; 600ms 一轮也谈不上分发开销.
pub fn run_store_pipe_loop(
    poll_every: Duration,
    make_js: &mut dyn FnMut() -> String,
    on_app: &mut dyn FnMut(u32) -> Option<String>,
    on_log: &mut dyn FnMut(String),
    ready_timeout: Duration,
    mut panel: Option<&mut dyn PanelBridge>,
) -> bool {
    let mut panel_state = PanelState::default();
    let Some(pipe) = wait_for_pipe(ready_timeout) else {
        on_log("catalog_add=store_cdp pipe_absent (hook never handed one over)".into());
        return false;
    };
    let mut session = CdpPipeSession::new(pipe);

    // 拿到管道 ≠ CEF 那头已经就绪. 多试几次再判死刑, 否则冷启动会误判.
    let mut ready = false;
    let mut last_err = String::new();
    for _ in 0..20 {
        match session.targets() {
            Ok(_) => {
                ready = true;
                break;
            }
            Err(e) => last_err = e,
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if !ready {
        on_log(format!("catalog_add=store_cdp pipe_dead {last_err}"));
        return false;
    }
    on_log("catalog_add=store_cdp pipe_up (no debug port opened)".into());

    let mut last_pages: usize = 0;
    let mut announced = false;
    let mut dead_rounds = 0u32;
    // "一个商店页都没摸到" 每轮都会复现: 只在刚进入这个状态时记一次.
    // 按时间节流不行 — 没开商店页是常态, 定时重记就是每小时几百行长日志.
    let mut zero_logged = false;
    loop {
        let js = make_js();
        let r = poll_store_pipe(&mut session, &js);
        if r.cdp_up {
            dead_rounds = 0;
            if !announced || r.store_pages != last_pages || r.injected > 0 {
                on_log(format!(
                    "catalog_add=store_cdp up store_pages={} injected={} via=pipe",
                    r.store_pages, r.injected
                ));
                announced = true;
                last_pages = r.store_pages;
            }
            if r.store_pages > 0 {
                zero_logged = false;
            }
            for note in &r.notes {
                if note.starts_with("cdp_store_pages=0") {
                    if !zero_logged {
                        on_log(format!("catalog_add={note}"));
                        zero_logged = true;
                    }
                } else {
                    on_log(format!("catalog_add={note}"));
                }
            }
            for app_id in r.pending_app_ids {
                if let Some(js) = on_app(app_id) {
                    push_store_feedback_pipe(&mut session, &r.store_targets, &js, on_log);
                }
            }
            // 配置页搭同一趟车; 它出问题也不能连累入库那条路.
            if let Some(p) = panel.as_mut() {
                let on = p.enabled();
                if panel_state.should_run(on) {
                    let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        poll_panel_pipe(&mut session, &mut **p, &mut panel_state, on, on_log);
                    }));
                    if ok.is_err() {
                        on_log("config_ui=poll_panic (bridge kept alive)".into());
                    }
                }
            }
        } else {
            dead_rounds += 1;
            // webhelper 崩了会重启, detour 会再递一根管道过来.
            if dead_rounds >= 10 {
                on_log("catalog_add=store_cdp pipe_lost (webhelper gone?), re-arming".into());
                match wait_for_pipe(Duration::from_secs(60)) {
                    Some(p) => {
                        session = CdpPipeSession::new(p);
                        dead_rounds = 0;
                        announced = false;
                    }
                    None => return true, // 通道本身是好的, 只是 Steam 没了
                }
            }
        }
        std::thread::sleep(poll_every);
    }
}

fn unwrap_reply(v: &Value) -> Result<Value, String> {
    if let Some(err) = v.get("error") {
        return Err(format!("cdp error: {err}"));
    }
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// 读线程: 按 `\0` 切帧, 解析成 JSON 丢给主逻辑.
fn read_loop(pipe: &DevToolsPipe, tx: &mpsc::SyncSender<Value>) {
    let mut acc: Vec<u8> = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = match pipe.read(&mut buf) {
            Ok(0) | Err(_) => return, // 对端关了 = webhelper 没了
            Ok(n) => n,
        };
        acc.extend_from_slice(&buf[..n]);
        // 一次读可能带回多条消息, 全部切出来.
        while let Some(i) = acc.iter().position(|&b| b == 0) {
            let frame: Vec<u8> = acc.drain(..=i).collect();
            let Ok(v) = serde_json::from_slice::<Value>(&frame[..i]) else {
                continue; // 坏帧丢掉, 不拖累后面的
            };
            match tx.try_send(v) {
                Ok(()) => {}
                // 满时丢新帧可接受 (事件流), 不能阻塞读线程.
                Err(mpsc::TrySendError::Full(_)) => {
                    DROPPED_FRAMES.fetch_add(1, Ordering::Relaxed);
                }
                Err(mpsc::TrySendError::Disconnected(_)) => return, // 会话没了
            }
        }
        // 防御: 对端一直不发 \0 就别无限涨.
        if acc.len() > 64 * 1024 * 1024 {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_with_error_is_an_err() {
        let v = json!({"id": 1, "error": {"code": -32000, "message": "boom"}});
        assert!(unwrap_reply(&v).is_err());
    }

    #[test]
    fn reply_without_result_is_null_not_error() {
        let v = json!({"id": 1});
        assert_eq!(unwrap_reply(&v).unwrap(), Value::Null);
    }

    #[test]
    fn missing_target_fields_default_to_empty() {
        let t = json!({"targetId": "abc"});
        assert_eq!(str_field(&t, "targetId"), "abc");
        assert_eq!(str_field(&t, "url"), "");
    }

    #[test]
    fn bounded_channel_full_drops_without_blocking() {
        // 有界通道: 满时 try_send 立即返回 Full, 不阻塞读线程; 接收侧照常工作.
        let (tx, rx) = mpsc::sync_channel::<u32>(4);
        for i in 0..8 {
            let r = tx.try_send(i);
            assert!(r.is_ok() || matches!(r, Err(mpsc::TrySendError::Full(_))));
        }
        // 只送出容量内的帧, 多出的被丢; 接收侧 API 与普通 channel 一致.
        assert_eq!(rx.recv_timeout(Duration::from_millis(100)).unwrap(), 0);
        assert_eq!(rx.try_iter().count(), 3);
    }
}
