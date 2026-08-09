//! CatMisteam (ManifestHub-GUI) 社区源.
//!
//! 与 CaiGamer 同级的私有社区源, 但无加密、无鉴权: 直接 HTTP 拉现成 lua / depotkeys.
//! 解密/协议细节只留在本文件; community 编排只通过 CatalogProvider / CatalogEnricher 装配.
//!
//! 调研: `docs/plan/17-catmisteam-sources.md`.

use std::collections::{HashMap, HashSet};

use stt_core::{AppId, CatalogBundle, DepotId};
use stt_platform::{winhttp_get, HttpError, WinHttpGetOptions};

use crate::{
    keys_parse::{collect_lua_keys, collect_lua_tickets, LuaTicketSet},
    validate_bundle, CatalogEnricher, CatalogError, CatalogFetchOutcome, CatalogProvider,
    CatalogResult, CatalogTraceEntry, CatalogTraceOutcome, EnrichContext, ProviderErrorKind,
};

const PROVIDER: &str = "community:catmisteam";
const DEFAULT_LUA_URL: &str = "https://catmisteam.com/steam-game-list/lua/{app_id}.lua";
/// 单份 lua 远小于 1 MiB; 上限防异常响应.
const MAX_LUA_BYTES: usize = 256 * 1024;

/// CatMisteam 目录源: 完整源 (lua 直下) + Community 成功路径上的 key 补全.
#[derive(Debug, Clone)]
pub struct CatmisteamCatalogProvider {
    options: WinHttpGetOptions,
    lua_url_template: &'static str,
}

impl CatmisteamCatalogProvider {
    pub const fn new(options: WinHttpGetOptions) -> Self {
        Self {
            options,
            lua_url_template: DEFAULT_LUA_URL,
        }
    }

    /// 测试用: 指向本地假服务器.
    #[cfg(test)]
    pub(crate) fn with_url(options: WinHttpGetOptions, lua_url_template: &'static str) -> Self {
        Self {
            options,
            lua_url_template,
        }
    }

    fn fetch_outcome(&self, app_id: AppId) -> CatalogResult<CatalogFetchOutcome> {
        let text = self.fetch_lua(app_id)?;
        let bundle = parse_lua_catalog(app_id, &text)?;
        let bundle = validate_bundle(app_id, bundle)?;
        Ok(CatalogFetchOutcome {
            bundle,
            source: PROVIDER.to_owned(),
            trace: vec![CatalogTraceEntry {
                provider: PROVIDER.to_owned(),
                outcome: CatalogTraceOutcome::Hit,
            }],
            manifest_blobs: Vec::new(),
            related_dlc_ids: Vec::new(),
        })
    }

    fn fetch_lua(&self, app_id: AppId) -> CatalogResult<String> {
        let url = self
            .lua_url_template
            .replacen("{app_id}", &app_id.to_string(), 1);
        let mut options = self.options;
        options.max_body_bytes = options.max_body_bytes.min(MAX_LUA_BYTES);
        let response = winhttp_get(&url, options).map_err(map_http_error)?;
        if !(200..300).contains(&response.status) {
            let kind = if response.status == 404 {
                ProviderErrorKind::NotFound
            } else {
                ProviderErrorKind::Rejected
            };
            return Err(provider_error(
                format!("HTTP status {}", response.status),
                kind,
            ));
        }
        if response.body.len() > MAX_LUA_BYTES {
            return Err(CatalogError::PayloadTooLarge {
                actual: response.body.len(),
                limit: MAX_LUA_BYTES,
            });
        }
        let text = std::str::from_utf8(&response.body).map_err(|_| {
            provider_error("lua response is not UTF-8", ProviderErrorKind::Rejected)
        })?;
        Ok(text.to_owned())
    }
}

impl CatalogProvider for CatmisteamCatalogProvider {
    fn id(&self) -> &str {
        PROVIDER
    }

    fn fetch(&self, app_id: AppId) -> CatalogResult<CatalogBundle> {
        self.fetch_outcome(app_id).map(|outcome| outcome.bundle)
    }

    fn fetch_with_trace(&self, app_id: AppId) -> CatalogResult<CatalogFetchOutcome> {
        self.fetch_outcome(app_id)
    }
}

/// Community 成功路径上的 best-effort key/ticket 补全 (不覆盖已有).
impl CatalogEnricher for CatmisteamCatalogProvider {
    fn id(&self) -> &str {
        PROVIDER
    }

    fn enrich(&self, ctx: &mut EnrichContext<'_>) {
        let missing_keys: Vec<DepotId> = ctx
            .bundle
            .app_depots
            .values()
            .flatten()
            .copied()
            .filter(|depot_id| !ctx.bundle.depot_keys.contains_key(depot_id))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let need_tickets = ctx.bundle.app_tickets.is_empty()
            && ctx.bundle.etickets.is_empty()
            && ctx.bundle.steam_ids.is_empty();
        if missing_keys.is_empty() && !need_tickets {
            return;
        }
        let text = match self.fetch_lua(ctx.app_id) {
            Ok(text) => text,
            Err(error) => {
                if !missing_keys.is_empty() {
                    ctx.trace.push(CatalogTraceEntry {
                        provider: "community:catmisteam_key".to_owned(),
                        outcome: CatalogTraceOutcome::Failed(classify_provider_err(&error)),
                    });
                }
                if need_tickets {
                    ctx.trace.push(CatalogTraceEntry {
                        provider: "community:catmisteam_ticket".to_owned(),
                        outcome: CatalogTraceOutcome::Failed(classify_provider_err(&error)),
                    });
                }
                return;
            }
        };

        if !missing_keys.is_empty() {
            let mut keys = HashMap::new();
            let _ = collect_lua_keys(&text, &mut keys);
            let mut filled = 0;
            for depot_id in &missing_keys {
                if let Some(key) = keys.get(depot_id) {
                    if !ctx.bundle.depot_keys.contains_key(depot_id) {
                        ctx.bundle.depot_keys.insert(*depot_id, key.clone());
                        filled += 1;
                    }
                }
            }
            ctx.trace.push(CatalogTraceEntry {
                provider: "community:catmisteam_key".to_owned(),
                outcome: if filled > 0 {
                    CatalogTraceOutcome::Hit
                } else {
                    CatalogTraceOutcome::Failed(ProviderErrorKind::NotFound)
                },
            });
        }

        if need_tickets {
            let mut tickets = LuaTicketSet::default();
            let _ = collect_lua_tickets(&text, &mut tickets);
            if tickets.is_empty() {
                ctx.trace.push(CatalogTraceEntry {
                    provider: "community:catmisteam_ticket".to_owned(),
                    outcome: CatalogTraceOutcome::Failed(ProviderErrorKind::NotFound),
                });
            } else {
                tickets.merge_into_bundle(ctx.bundle);
                for &app_id in tickets
                    .app_tickets
                    .keys()
                    .chain(tickets.etickets.keys())
                    .chain(tickets.steam_ids.keys())
                {
                    if app_id != 0 && !ctx.bundle.apps.contains(&app_id) {
                        ctx.bundle.apps.push(app_id);
                    }
                }
                ctx.trace.push(CatalogTraceEntry {
                    provider: "community:catmisteam_ticket".to_owned(),
                    outcome: CatalogTraceOutcome::Hit,
                });
            }
        }
    }
}

/// 把 CatMisteam 现成 lua 收成 CatalogBundle.
///
/// 兼容面 (与调研样例一致, 不执行脚本):
/// - `addappid(appId)` / `addappid(appId, purchaseTime)` → apps (+ purchase_times)
/// - `addappid(depotId, _, "64hex")` → depot_keys; 全部挂到请求 app 的 app_depots
/// - `setAppticket` / `setETicket` / `setStat` → ticket 字段 (写 lua / 注册表)
///
/// 不含 setManifestid / token; 完整下载仍依赖上游 metadata / archive / request-code.
fn parse_lua_catalog(app_id: AppId, text: &str) -> CatalogResult<CatalogBundle> {
    let mut depot_keys = HashMap::new();
    collect_lua_keys(text, &mut depot_keys)?;
    let mut tickets = LuaTicketSet::default();
    collect_lua_tickets(text, &mut tickets)?;

    let mut apps = Vec::new();
    let mut purchase_times = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("--") {
            continue;
        }
        let Some(arguments) = line.strip_prefix("addappid(") else {
            continue;
        };
        let Some(arguments) = arguments.split_once(')').map(|(arguments, _)| arguments) else {
            continue;
        };
        let parts: Vec<&str> = arguments.split(',').map(str::trim).collect();
        match parts.as_slice() {
            [id_text] => {
                let Ok(id) = id_text.parse::<AppId>() else {
                    continue;
                };
                if id != 0 && !apps.contains(&id) {
                    apps.push(id);
                }
            }
            [id_text, purchase_text] => {
                let Ok(id) = id_text.parse::<AppId>() else {
                    continue;
                };
                if id == 0 {
                    continue;
                }
                if !apps.contains(&id) {
                    apps.push(id);
                }
                if let Ok(purchase) = purchase_text.parse::<u32>() {
                    if purchase != 0 {
                        purchase_times.insert(id, purchase);
                    }
                }
            }
            // 三参 key 行由 collect_lua_keys 处理; 这里不重复.
            _ => {}
        }
    }

    if depot_keys.is_empty() && !apps.contains(&app_id) {
        return Err(provider_error(
            "lua has no app or depot keys",
            ProviderErrorKind::NotFound,
        ));
    }
    if !apps.contains(&app_id) {
        apps.insert(0, app_id);
    }
    for &ticket_app in tickets
        .app_tickets
        .keys()
        .chain(tickets.etickets.keys())
        .chain(tickets.steam_ids.keys())
    {
        if ticket_app != 0 && !apps.contains(&ticket_app) {
            apps.push(ticket_app);
        }
    }

    let mut depots: Vec<DepotId> = depot_keys.keys().copied().collect();
    depots.sort_unstable();
    // 没有 key 的主 app 行本身不是 depot; app_depots 只列有 key 的 depot.
    // 若上游只给了 app 行、没有任何 depot key, 仍允许空 depots (validate 放行),
    // 但完整源价值很低 — 视为 NotFound 以免空壳覆盖后续源.
    if depots.is_empty() {
        return Err(provider_error(
            "lua has no depot keys",
            ProviderErrorKind::NotFound,
        ));
    }

    let mut bundle = CatalogBundle {
        apps,
        app_depots: HashMap::from([(app_id, depots)]),
        depot_keys,
        purchase_times,
        ..CatalogBundle::default()
    };
    tickets.merge_into_bundle(&mut bundle);
    Ok(bundle)
}

fn classify_provider_err(error: &CatalogError) -> ProviderErrorKind {
    match error {
        CatalogError::Provider { kind, .. } => *kind,
        CatalogError::RequestedAppMissing(_) => ProviderErrorKind::NotFound,
        _ => ProviderErrorKind::Rejected,
    }
}

fn map_http_error(error: HttpError) -> CatalogError {
    match error {
        HttpError::Timeout { .. } => {
            provider_error("HTTP request failed", ProviderErrorKind::Timeout)
        }
        HttpError::ResponseTooLarge { actual, limit } => {
            CatalogError::PayloadTooLarge { actual, limit }
        }
        HttpError::RequestTooLarge { .. }
        | HttpError::InvalidUrl(_)
        | HttpError::InvalidOptions(_)
        | HttpError::Windows { .. } => {
            provider_error("HTTP request failed", ProviderErrorKind::Unavailable)
        }
    }
}

fn provider_error(detail: impl Into<String>, kind: ProviderErrorKind) -> CatalogError {
    CatalogError::Provider {
        provider: PROVIDER.to_owned(),
        kind,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_template_points_at_catmisteam_lua() {
        assert_eq!(
            DEFAULT_LUA_URL,
            "https://catmisteam.com/steam-game-list/lua/{app_id}.lua"
        );
    }

    #[test]
    fn parse_lua_catalog_matches_survey_sample() {
        // docs/plan/17-catmisteam-sources.md §3 (Mafia II 1030830).
        let lua = r#"
addappid(1030830)
addappid(1030831,0,"32530b6455aff62129bdca96b7eade4476d7d5aa76a6233563dca1ccad754475")
addappid(1523211,0,"6111b2e3d21efa5e5b26a91f75ff32d12aa8c5bb547c4d0215eda184a7f4c49b")
"#;
        let bundle = parse_lua_catalog(1030830, lua).unwrap();
        assert_eq!(bundle.apps, vec![1030830]);
        assert_eq!(bundle.app_depots[&1030830], vec![1030831, 1523211]);
        assert_eq!(
            bundle.depot_keys[&1030831],
            "32530b6455aff62129bdca96b7eade4476d7d5aa76a6233563dca1ccad754475"
        );
        assert_eq!(
            bundle.depot_keys[&1523211],
            "6111b2e3d21efa5e5b26a91f75ff32d12aa8c5bb547c4d0215eda184a7f4c49b"
        );
        assert!(bundle.manifests.is_empty());
        assert!(bundle.access_tokens.is_empty());
    }

    #[test]
    fn parse_lua_catalog_rejects_empty_or_app_only() {
        assert!(parse_lua_catalog(42, "").is_err());
        assert!(parse_lua_catalog(42, "addappid(42)\n").is_err());
        assert!(parse_lua_catalog(42, "-- comment only\n").is_err());
    }

    #[test]
    fn parse_lua_catalog_skips_invalid_key_length() {
        let lua = r#"
addappid(42)
addappid(43,0,"short")
addappid(44,0,"abbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
"#;
        // 64 hex only for 44; short key skipped by collect_lua_keys.
        let key44 = "ab".repeat(32);
        let lua = lua.replace(
            "abbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            &key44,
        );
        let bundle = parse_lua_catalog(42, &lua).unwrap();
        assert!(!bundle.depot_keys.contains_key(&43));
        assert_eq!(bundle.depot_keys[&44], key44);
        assert_eq!(bundle.app_depots[&42], vec![44]);
    }

    #[test]
    fn provider_fetches_lua_over_http() {
        let _guard = crate::http_test_guard();
        let key = "ab".repeat(32);
        let lua = format!("addappid(42)\naddappid(43,0,\"{key}\")\n");
        let server = FakeLuaServer::spawn(lua.into_bytes());
        let provider = CatmisteamCatalogProvider::with_url(test_options(), server.template);
        let outcome = provider.fetch_with_trace(42).unwrap();
        assert_eq!(outcome.source, PROVIDER);
        assert_eq!(outcome.bundle.depot_keys[&43], key);
        assert_eq!(outcome.bundle.app_depots[&42], vec![43]);
    }

    #[test]
    fn enricher_fills_missing_keys_only_over_http() {
        let _guard = crate::http_test_guard();
        let key_existing = "ab".repeat(32);
        let key_new = "ef".repeat(32);
        // 上游 43 是旧值, 不应覆盖; 44 是缺失, 应补上.
        let lua = format!(
            "addappid(42)\naddappid(43,0,\"{}\")\naddappid(44,0,\"{key_new}\")\n",
            "cd".repeat(32)
        );
        let server = FakeLuaServer::spawn(lua.into_bytes());
        let provider = CatmisteamCatalogProvider::with_url(test_options(), server.template);

        let mut bundle = CatalogBundle {
            apps: vec![42],
            app_depots: HashMap::from([(42, vec![43, 44])]),
            depot_keys: HashMap::from([(43, key_existing.clone())]),
            ..CatalogBundle::default()
        };
        let mut blobs = Vec::new();
        let mut trace = Vec::new();
        let mut ctx = EnrichContext {
            app_id: 42,
            bundle: &mut bundle,
            manifest_blobs: &mut blobs,
            trace: &mut trace,
        };
        provider.enrich(&mut ctx);

        assert_eq!(bundle.depot_keys[&43], key_existing);
        assert_eq!(bundle.depot_keys[&44], key_new);
        assert!(
            trace
                .iter()
                .any(|entry| entry.provider == "community:catmisteam_key"
                    && entry.outcome == CatalogTraceOutcome::Hit),
            "{trace:?}"
        );
    }

    #[test]
    fn enricher_records_not_found_when_lua_missing_depot() {
        let _guard = crate::http_test_guard();
        let lua = format!("addappid(42)\naddappid(99,0,\"{}\")\n", "ab".repeat(32));
        let server = FakeLuaServer::spawn(lua.into_bytes());
        let provider = CatmisteamCatalogProvider::with_url(test_options(), server.template);

        let mut bundle = CatalogBundle {
            apps: vec![42],
            app_depots: HashMap::from([(42, vec![43])]),
            ..CatalogBundle::default()
        };
        let mut blobs = Vec::new();
        let mut trace = Vec::new();
        let mut ctx = EnrichContext {
            app_id: 42,
            bundle: &mut bundle,
            manifest_blobs: &mut blobs,
            trace: &mut trace,
        };
        provider.enrich(&mut ctx);

        assert!(bundle.depot_keys.is_empty());
        assert!(
            trace.iter().any(|entry| {
                entry.provider == "community:catmisteam_key"
                    && entry.outcome == CatalogTraceOutcome::Failed(ProviderErrorKind::NotFound)
            }),
            "{trace:?}"
        );
    }

    fn test_options() -> WinHttpGetOptions {
        WinHttpGetOptions {
            timeouts: stt_platform::WinHttpTimeouts {
                resolve_ms: 1_000,
                connect_ms: 1_000,
                send_ms: 1_000,
                receive_ms: 1_000,
            },
            max_body_bytes: 64 * 1024,
        }
    }

    /// 单连接假 HTTP: 响应固定 body, 路径任意 (lua 模板带 app_id).
    struct FakeLuaServer {
        template: &'static str,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeLuaServer {
        fn spawn(body: Vec<u8>) -> Self {
            use std::io::{Read, Write};
            use std::net::{Shutdown, TcpListener};
            use std::time::{Duration, Instant};

            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let template =
                Box::leak(format!("http://127.0.0.1:{port}/lua/{{app_id}}.lua").into_boxed_str());
            let thread = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(_) => return,
                    };
                    let mut request = [0u8; 4096];
                    let _ = stream.read(&mut request);
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    if stream.write_all(headers.as_bytes()).is_ok()
                        && stream.write_all(&body).is_ok()
                    {
                        let _ = stream.shutdown(Shutdown::Both);
                        return;
                    }
                }
            });
            Self {
                template,
                thread: Some(thread),
            }
        }
    }

    impl Drop for FakeLuaServer {
        fn drop(&mut self) {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    #[test]
    fn parse_lua_catalog_keeps_set_appticket() {
        let key = "ab".repeat(32);
        let lua = format!(
            "addappid(42)\naddappid(43,0,\"{key}\")\nsetAppticket(42, \"AaBb\")\nsetETicket(42, \"CcDd\")\n"
        );
        let bundle = parse_lua_catalog(42, &lua).unwrap();
        assert_eq!(
            bundle.app_tickets.get(&42).map(String::as_str),
            Some("aabb")
        );
        assert_eq!(bundle.etickets.get(&42).map(String::as_str), Some("ccdd"));
        assert_eq!(
            bundle.depot_keys.get(&43).map(String::as_str),
            Some(key.as_str())
        );
    }
}
