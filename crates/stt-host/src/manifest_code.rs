//! Manifest request code 的 host provider 与有界后台 worker.

use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;

use stt_config::{
    ConfigState, LuaHttpClient, LuaHttpErrorKind, LuaHttpMethod, LuaHttpRequest, LuaHttpResponse,
    LuaManifestCodeErrorKind, LuaManifestCodeExecutor, ManifestSection,
};
use stt_steamclient::{
    ManifestCodeFailureKind, ManifestCodeProvider, ManifestCodeProviderResult, ManifestCodeRequest,
    ManifestCodeResolution, ManifestCodeResolverChain, ManifestCodeStage, ManifestCodeTraceEntry,
    ManifestCodeTraceOutcome, ManifestCodeUnresolved,
};

const QUEUE_CAPACITY: usize = 16;
const MAX_HTTP_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_LUA_HTTP_RESPONSE_BYTES: usize = 1024 * 1024;

pub(super) fn spawn_resolver_worker(steam_root: &Path, state: &ConfigState) {
    let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
    if !stt_steamclient::register_manifest_code_worker(sender) {
        super::append_host_log(steam_root, "request_code_worker=already_registered");
        return;
    }

    let root = steam_root.to_path_buf();
    let state = state.clone();
    let spawn = std::thread::Builder::new()
        .name("stt-manifest-code-worker".into())
        .spawn(move || {
            while let Ok(work) = receiver.recv() {
                match resolve_once(&root, &state, work.request) {
                    Ok(resolution) => {
                        let source = resolution.source.clone();
                        let trace = trace_text(&resolution.trace);
                        let outcome = stt_steamclient::complete_manifest_code_work(
                            work,
                            resolution.request_code,
                        );
                        super::append_host_log(
                            &root,
                            &format!(
                                "request_code=resolved source={source} trace={trace} state={outcome:?}"
                            ),
                        );
                    }
                    Err(error) => {
                        stt_steamclient::cancel_manifest_code_work(work);
                        super::append_host_log(
                            &root,
                            &format!("request_code=unresolved trace={}", trace_text(&error.trace)),
                        );
                    }
                }
            }
        });
    match spawn {
        Ok(_) => super::append_host_log(
            steam_root,
            &format!("request_code_worker=ready queue_capacity={QUEUE_CAPACITY}"),
        ),
        Err(error) => super::append_host_log(
            steam_root,
            &format!("request_code_worker=spawn_err {error}"),
        ),
    }
}

fn resolve_once(
    steam_root: &Path,
    state: &ConfigState,
    request: ManifestCodeRequest,
) -> Result<ManifestCodeResolution, ManifestCodeUnresolved> {
    let config = state.host().manifest;
    let lua = load_lua_executor(steam_root, &config);
    let lua_extended = lua.as_ref().map(|executor| LuaManifestProvider {
        executor: Arc::clone(executor),
        stage: ManifestCodeStage::LuaExtended,
    });
    let lua_basic = lua.as_ref().map(|executor| LuaManifestProvider {
        executor: Arc::clone(executor),
        stage: ManifestCodeStage::LuaBasic,
    });
    let http = HttpManifestProvider::for_source(&config);
    ManifestCodeResolverChain::new(
        lua_extended
            .as_ref()
            .map(|provider| provider as &dyn ManifestCodeProvider),
        lua_basic
            .as_ref()
            .map(|provider| provider as &dyn ManifestCodeProvider),
        Some(&http),
    )
    .resolve(request)
}

fn load_lua_executor(
    steam_root: &Path,
    config: &ManifestSection,
) -> Option<Arc<LuaManifestCodeExecutor>> {
    let source =
        std::fs::read_to_string(ConfigState::default_lua_dir(steam_root).join("manifest.lua"))
            .ok()?;
    let client: Arc<dyn LuaHttpClient> = Arc::new(ManifestLuaHttpClient {
        options: request_options(config, MAX_LUA_HTTP_RESPONSE_BYTES),
    });
    LuaManifestCodeExecutor::new(source, Some(client))
        .ok()
        .map(Arc::new)
}

struct LuaManifestProvider {
    executor: Arc<LuaManifestCodeExecutor>,
    stage: ManifestCodeStage,
}

impl ManifestCodeProvider for LuaManifestProvider {
    fn id(&self) -> &str {
        match self.stage {
            ManifestCodeStage::LuaExtended => "lua_ex",
            ManifestCodeStage::LuaBasic => "lua",
            ManifestCodeStage::Http => unreachable!("HTTP has a separate provider"),
        }
    }

    fn resolve(&self, request: ManifestCodeRequest) -> ManifestCodeProviderResult {
        let result = match self.stage {
            ManifestCodeStage::LuaExtended => {
                let (Some(app_id), Some(depot_id)) = (request.app_id, request.depot_id) else {
                    return Ok(None);
                };
                self.executor
                    .resolve_extended(app_id, depot_id, request.manifest_gid)
            }
            ManifestCodeStage::LuaBasic => self.executor.resolve_basic(request.manifest_gid),
            ManifestCodeStage::Http => unreachable!("HTTP has a separate provider"),
        };
        result.map_err(map_lua_error)
    }
}

fn map_lua_error(error: LuaManifestCodeErrorKind) -> ManifestCodeFailureKind {
    match error {
        LuaManifestCodeErrorKind::Unavailable => ManifestCodeFailureKind::Unavailable,
        LuaManifestCodeErrorKind::Rejected => ManifestCodeFailureKind::Rejected,
        LuaManifestCodeErrorKind::InvalidResponse => ManifestCodeFailureKind::InvalidResponse,
        LuaManifestCodeErrorKind::Internal => ManifestCodeFailureKind::Internal,
    }
}

#[derive(Debug, Clone, Copy)]
enum HttpResponseFormat {
    PlainDecimal,
    SteamRunJson,
}

struct HttpManifestProvider {
    id: String,
    url_template: String,
    format: HttpResponseFormat,
    options: stt_platform::WinHttpGetOptions,
}

impl HttpManifestProvider {
    fn for_source(config: &ManifestSection) -> Self {
        let (template, format) = match config.url.as_str() {
            "opensteamtool" => (
                "https://manifest.opensteamtool.com/{manifest_gid}",
                HttpResponseFormat::PlainDecimal,
            ),
            "wudrm" => (
                // 强制 HTTPS: 该 provider 必须自行终止 TLS, 不允许明文回落
                "https://gmrc.wudrm.com/manifest/{manifest_gid}",
                HttpResponseFormat::PlainDecimal,
            ),
            "steamrun" => (
                "https://manifest.steam.run/api/manifest/{manifest_gid}",
                HttpResponseFormat::SteamRunJson,
            ),
            _ => ("", HttpResponseFormat::PlainDecimal),
        };
        Self {
            id: config.url.clone(),
            url_template: template.to_owned(),
            format,
            options: stt_platform::WinHttpGetOptions {
                timeouts: timeouts(config),
                max_body_bytes: MAX_HTTP_RESPONSE_BYTES,
            },
        }
    }

    #[cfg(test)]
    fn new_for_test(
        id: &str,
        url_template: String,
        format: HttpResponseFormat,
        options: stt_platform::WinHttpGetOptions,
    ) -> Self {
        Self {
            id: id.to_owned(),
            url_template,
            format,
            options,
        }
    }
}

impl ManifestCodeProvider for HttpManifestProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn resolve(&self, request: ManifestCodeRequest) -> ManifestCodeProviderResult {
        if self.url_template.is_empty() {
            return Err(ManifestCodeFailureKind::Unavailable);
        }
        let url = self
            .url_template
            .replace("{manifest_gid}", &request.manifest_gid.to_string());
        let response = stt_platform::winhttp_get(&url, self.options).map_err(map_http_error)?;
        if response.status != 200 {
            return Err(ManifestCodeFailureKind::Rejected);
        }
        parse_http_response(self.format, &response.body).map(Some)
    }
}

fn parse_http_response(
    format: HttpResponseFormat,
    body: &[u8],
) -> Result<u64, ManifestCodeFailureKind> {
    match format {
        HttpResponseFormat::PlainDecimal => parse_decimal(body),
        HttpResponseFormat::SteamRunJson => {
            let value: serde_json::Value = serde_json::from_slice(body)
                .map_err(|_| ManifestCodeFailureKind::InvalidResponse)?;
            let content = value
                .get("content")
                .and_then(serde_json::Value::as_str)
                .ok_or(ManifestCodeFailureKind::InvalidResponse)?;
            parse_decimal(content.as_bytes())
        }
    }
}

fn parse_decimal(body: &[u8]) -> Result<u64, ManifestCodeFailureKind> {
    if body.is_empty() || !body.iter().all(u8::is_ascii_digit) {
        return Err(ManifestCodeFailureKind::InvalidResponse);
    }
    let text = std::str::from_utf8(body).map_err(|_| ManifestCodeFailureKind::InvalidResponse)?;
    text.parse::<u64>()
        .ok()
        .filter(|code| *code != 0)
        .ok_or(ManifestCodeFailureKind::InvalidResponse)
}

fn map_http_error(error: stt_platform::HttpError) -> ManifestCodeFailureKind {
    match error {
        stt_platform::HttpError::Timeout { .. } => ManifestCodeFailureKind::Timeout,
        stt_platform::HttpError::ResponseTooLarge { .. } => {
            ManifestCodeFailureKind::InvalidResponse
        }
        stt_platform::HttpError::Windows { .. } => ManifestCodeFailureKind::Unavailable,
        stt_platform::HttpError::InvalidUrl(_)
        | stt_platform::HttpError::InvalidOptions(_)
        | stt_platform::HttpError::RequestTooLarge { .. } => ManifestCodeFailureKind::Internal,
    }
}

struct ManifestLuaHttpClient {
    options: stt_platform::WinHttpRequestOptions,
}

impl LuaHttpClient for ManifestLuaHttpClient {
    fn execute(&self, request: LuaHttpRequest) -> Result<LuaHttpResponse, LuaHttpErrorKind> {
        let method = match request.method {
            LuaHttpMethod::Get => stt_platform::HttpMethod::Get,
            LuaHttpMethod::Post => stt_platform::HttpMethod::Post,
        };
        let response = stt_platform::winhttp_request(
            method,
            &request.url,
            &request.headers,
            &request.body,
            self.options,
        )
        .map_err(map_lua_http_error)?;
        Ok(LuaHttpResponse {
            status: response.status,
            body: response.body,
        })
    }
}

fn map_lua_http_error(error: stt_platform::HttpError) -> LuaHttpErrorKind {
    match error {
        stt_platform::HttpError::Timeout { .. } => LuaHttpErrorKind::Timeout,
        stt_platform::HttpError::RequestTooLarge { .. } => LuaHttpErrorKind::RequestTooLarge,
        stt_platform::HttpError::ResponseTooLarge { .. } => LuaHttpErrorKind::ResponseTooLarge,
        stt_platform::HttpError::InvalidUrl(_) | stt_platform::HttpError::InvalidOptions(_) => {
            LuaHttpErrorKind::InvalidRequest
        }
        stt_platform::HttpError::Windows { .. } => LuaHttpErrorKind::Unavailable,
    }
}

fn request_options(
    config: &ManifestSection,
    max_response_body_bytes: usize,
) -> stt_platform::WinHttpRequestOptions {
    stt_platform::WinHttpRequestOptions {
        timeouts: timeouts(config),
        max_request_body_bytes: 256 * 1024,
        max_response_body_bytes,
    }
}

fn timeouts(config: &ManifestSection) -> stt_platform::WinHttpTimeouts {
    stt_platform::WinHttpTimeouts {
        resolve_ms: config.timeout_resolve_ms,
        connect_ms: config.timeout_connect_ms,
        send_ms: config.timeout_send_ms,
        receive_ms: config.timeout_recv_ms,
    }
}

fn trace_text(trace: &[ManifestCodeTraceEntry]) -> String {
    trace
        .iter()
        .map(|entry| {
            let outcome = match entry.outcome {
                ManifestCodeTraceOutcome::Hit => "hit".to_owned(),
                ManifestCodeTraceOutcome::Miss => "miss".to_owned(),
                ManifestCodeTraceOutcome::Failed(kind) => format!("failed:{kind:?}"),
            };
            format!("{}:{:?}:{outcome}", entry.provider, entry.stage)
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    fn request() -> ManifestCodeRequest {
        ManifestCodeRequest {
            app_id: Some(10),
            depot_id: Some(20),
            manifest_gid: 30,
        }
    }

    #[test]
    fn provider_formats_are_strict() {
        assert_eq!(
            parse_http_response(HttpResponseFormat::PlainDecimal, b"123"),
            Ok(123)
        );
        assert_eq!(
            parse_http_response(HttpResponseFormat::SteamRunJson, br#"{"content":"456"}"#),
            Ok(456)
        );
        assert!(parse_http_response(HttpResponseFormat::PlainDecimal, b"123\n").is_err());
        assert!(
            parse_http_response(HttpResponseFormat::SteamRunJson, br#"{"content":456}"#).is_err()
        );
        assert!(parse_http_response(HttpResponseFormat::PlainDecimal, b"0").is_err());
    }

    #[test]
    fn http_provider_uses_bounded_transport() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let read = stream.read(&mut request).unwrap();
            assert!(String::from_utf8_lossy(&request[..read]).contains("GET /30 "));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n123")
                .unwrap();
        });
        let provider = HttpManifestProvider::new_for_test(
            "test",
            format!("http://127.0.0.1:{port}/{{manifest_gid}}"),
            HttpResponseFormat::PlainDecimal,
            stt_platform::WinHttpGetOptions {
                timeouts: stt_platform::WinHttpTimeouts::default(),
                max_body_bytes: 16,
            },
        );

        assert_eq!(provider.resolve(request()), Ok(Some(123)));
        server.join().unwrap();
    }

    #[test]
    fn lua_extended_then_basic_precedes_http() {
        let root = tempfile::tempdir().unwrap();
        let lua_dir = ConfigState::default_lua_dir(root.path());
        std::fs::create_dir_all(&lua_dir).unwrap();
        std::fs::write(
            lua_dir.join("manifest.lua"),
            r#"
function fetch_manifest_code_ex(app_id, depot_id, gid) return nil end
function fetch_manifest_code(gid) return "789" end
"#,
        )
        .unwrap();
        let state = ConfigState::new();

        let resolution = resolve_once(root.path(), &state, request()).unwrap();

        assert_eq!(resolution.request_code, 789);
        assert_eq!(resolution.source, "lua");
        assert_eq!(resolution.trace.len(), 2);
        assert_eq!(resolution.trace[0].stage, ManifestCodeStage::LuaExtended);
        assert_eq!(resolution.trace[1].stage, ManifestCodeStage::LuaBasic);
    }
}
