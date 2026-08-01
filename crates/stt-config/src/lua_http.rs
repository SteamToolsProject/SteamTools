//! Lua HTTP helper 的受限契约.

use std::sync::Arc;

use mlua::{Lua, Table, Value};

const MAX_URL_BYTES: usize = 2048;
const MAX_HEADERS: usize = 32;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_REQUEST_BODY_BYTES: usize = 256 * 1024;
const MAX_RESPONSE_BODY_BYTES: usize = 1024 * 1024;

/// Lua helper 允许的 HTTP 方法.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LuaHttpMethod {
    Get,
    Post,
}

/// 已通过 Lua 边界校验的 HTTP 请求.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaHttpRequest {
    pub method: LuaHttpMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// HTTP client 返回给 Lua 的白名单字段.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaHttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// 不携带远端响应或凭据正文的稳定失败分类.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LuaHttpErrorKind {
    InvalidRequest,
    Timeout,
    RequestTooLarge,
    ResponseTooLarge,
    Unavailable,
}

impl LuaHttpErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::Timeout => "timeout",
            Self::RequestTooLarge => "request_too_large",
            Self::ResponseTooLarge => "response_too_large",
            Self::Unavailable => "unavailable",
        }
    }
}

/// 在调用线程同步执行请求. 调用方必须把运行时放在有界后台 worker.
pub trait LuaHttpClient: Send + Sync {
    fn execute(&self, request: LuaHttpRequest) -> Result<LuaHttpResponse, LuaHttpErrorKind>;
}

pub(crate) fn register_lua_http(lua: &Lua, client: Arc<dyn LuaHttpClient>) -> mlua::Result<()> {
    let get_client = Arc::clone(&client);
    let get = lua.create_function(move |lua, (url, headers): (String, Option<Table>)| {
        let request = build_request(LuaHttpMethod::Get, url, headers, Vec::new());
        lua_http_result(lua, request.and_then(|request| get_client.execute(request)))
    })?;
    lua.globals().set("http_get", get)?;

    let post = lua.create_function(
        move |lua, (url, body, headers): (String, mlua::String, Option<Table>)| {
            let request =
                build_request(LuaHttpMethod::Post, url, headers, body.as_bytes().to_vec());
            lua_http_result(lua, request.and_then(|request| client.execute(request)))
        },
    )?;
    lua.globals().set("http_post", post)?;
    Ok(())
}

/// 从 URL 提取 host: 去掉 scheme 与 path 部分.
fn url_host(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let end = rest.find('/').unwrap_or(rest.len());
    Some(&rest[..end])
}

/// host 是否禁止访问: 回环/链路本地/组播/IPv6/本地主机名.
/// 残余说明: 主机名 DNS 重绑定到回环 IP 的场景本层不覆盖 (脚本作者即信任边界).
fn host_forbidden(host: &str) -> bool {
    // 剥掉 :port 后缀 (仅当后缀全是数字); IPv6 字面量里的 ':' 不匹配数字后缀,
    // 会整体保留并在下一行因含 ':' 被拒.
    let host = match host.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => host,
    };
    // 任何含 ':' 的 host 都是 IPv6 字面量 (::1 / fe80:: / :: / ff00:: 等), 一律拒绝.
    if host.contains(':') {
        return true;
    }
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") {
        return true;
    }
    let Some(octets) = parse_ipv4(&lower) else {
        return false;
    };
    // 127.0.0.0/8 回环, 169.254.0.0/16 链路本地, 0.0.0.0, 224.0.0.0/4 组播.
    octets[0] == 127
        || (octets[0] == 169 && octets[1] == 254)
        || (octets[0] == 0 && octets[1] == 0 && octets[2] == 0 && octets[3] == 0)
        || octets[0] >= 224
}

/// 手写 dotted-quad IPv4 解析, 避免新增依赖.
fn parse_ipv4(host: &str) -> Option<[u8; 4]> {
    let mut parts = host.split('.');
    let mut octets = [0u8; 4];
    for octet in &mut octets {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *octet = part.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(octets)
}

fn build_request(
    method: LuaHttpMethod,
    url: String,
    headers: Option<Table>,
    body: Vec<u8>,
) -> Result<LuaHttpRequest, LuaHttpErrorKind> {
    let valid_url = !url.is_empty()
        && url.len() <= MAX_URL_BYTES
        && (url.starts_with("http://") || url.starts_with("https://"))
        && !url.contains(['\0', '\r', '\n', '#']);
    if !valid_url {
        return Err(LuaHttpErrorKind::InvalidRequest);
    }
    // 合法校验后解析 host, 拒绝回环/链路本地/组播/IPv6/本地主机名, 防 SSRF.
    let host = url_host(&url).ok_or(LuaHttpErrorKind::InvalidRequest)?;
    if host_forbidden(host) {
        return Err(LuaHttpErrorKind::InvalidRequest);
    }
    if body.len() > MAX_REQUEST_BODY_BYTES {
        return Err(LuaHttpErrorKind::RequestTooLarge);
    }

    let headers = parse_headers(headers)?;
    Ok(LuaHttpRequest {
        method,
        url,
        headers,
        body,
    })
}

fn parse_headers(table: Option<Table>) -> Result<Vec<(String, String)>, LuaHttpErrorKind> {
    let Some(table) = table else {
        return Ok(Vec::new());
    };
    let mut headers = Vec::new();
    let mut bytes = 0usize;
    for pair in table.pairs::<String, String>() {
        let (name, value) = pair.map_err(|_| LuaHttpErrorKind::InvalidRequest)?;
        let valid_name = !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        if !valid_name || value.contains(['\0', '\r', '\n']) {
            return Err(LuaHttpErrorKind::InvalidRequest);
        }
        bytes = bytes.saturating_add(name.len()).saturating_add(value.len());
        headers.push((name, value));
        if headers.len() > MAX_HEADERS || bytes > MAX_HEADER_BYTES {
            return Err(LuaHttpErrorKind::InvalidRequest);
        }
    }
    Ok(headers)
}

fn lua_http_result(
    lua: &Lua,
    result: Result<LuaHttpResponse, LuaHttpErrorKind>,
) -> mlua::Result<(Value, Value)> {
    match result {
        Ok(response) if response.body.len() <= MAX_RESPONSE_BODY_BYTES => Ok((
            Value::String(lua.create_string(&response.body)?),
            Value::Integer(i64::from(response.status)),
        )),
        Ok(_) => Ok((
            Value::Nil,
            Value::String(lua.create_string(LuaHttpErrorKind::ResponseTooLarge.as_str())?),
        )),
        Err(kind) => Ok((Value::Nil, Value::String(lua.create_string(kind.as_str())?))),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct StubClient {
        requests: Mutex<Vec<LuaHttpRequest>>,
    }

    impl LuaHttpClient for StubClient {
        fn execute(&self, request: LuaHttpRequest) -> Result<LuaHttpResponse, LuaHttpErrorKind> {
            self.requests.lock().unwrap().push(request);
            Ok(LuaHttpResponse {
                status: 201,
                body: b"response".to_vec(),
            })
        }
    }

    #[test]
    fn helpers_return_only_body_and_status() {
        let lua = Lua::new();
        let client = Arc::new(StubClient::default());
        register_lua_http(&lua, client.clone()).unwrap();

        let (body, status): (String, u16) = lua
            .load(r#"return http_get("https://example.test/data")"#)
            .eval()
            .unwrap();

        assert_eq!(body, "response");
        assert_eq!(status, 201);
        assert_eq!(client.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn post_preserves_binary_body_and_valid_headers() {
        let lua = Lua::new();
        let client = Arc::new(StubClient::default());
        register_lua_http(&lua, client.clone()).unwrap();

        lua.load(
            r#"return http_post("https://example.test/data", "a\0b", { ["X-Test"] = "yes" })"#,
        )
        .exec()
        .unwrap();
        let requests = client.requests.lock().unwrap();

        assert_eq!(requests[0].method, LuaHttpMethod::Post);
        assert_eq!(requests[0].body, b"a\0b");
        assert_eq!(requests[0].headers, vec![("X-Test".into(), "yes".into())]);
    }

    #[test]
    fn invalid_request_returns_stable_category_without_calling_client() {
        let lua = Lua::new();
        let client = Arc::new(StubClient::default());
        register_lua_http(&lua, client.clone()).unwrap();

        let (body, category): (Option<String>, String) = lua
            .load(r#"return http_get("file:///secret")"#)
            .eval()
            .unwrap();

        assert!(body.is_none());
        assert_eq!(category, "invalid_request");
        assert!(client.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn host_forbidden_rejects_loopback_link_local_multicast_and_ipv6() {
        for host in [
            "127.0.0.1",
            "127.8.9.10",
            "169.254.1.1",
            "0.0.0.0",
            "224.0.0.1",
            "239.255.255.255",
            "::1",
            "[::1]",
            "[::1]:8080",
            "::",
            "fe80::1",
            "ff02::1",
            "localhost",
            "foo.localhost",
        ] {
            assert!(host_forbidden(host), "should reject {host}");
        }
    }

    #[test]
    fn host_forbidden_allows_normal_hosts_with_and_without_port() {
        for host in [
            "store.steampowered.com",
            "store.steampowered.com:443",
            "api.example.com",
            "api.example.com:8080",
            "8.8.8.8",
            "203.0.113.5",
            "sub.localhost.test",
        ] {
            assert!(!host_forbidden(host), "should allow {host}");
        }
    }

    #[test]
    fn build_request_rejects_forbidden_hosts() {
        for url in [
            "http://127.0.0.1:8080/secret",
            "http://169.254.169.254/latest/meta-data",
            "http://0.0.0.0/x",
            "http://224.0.0.1/x",
            "http://::1/",
            "http://[::1]/",
            "https://localhost/x",
            "https://steam.localhost/x",
        ] {
            assert_eq!(
                build_request(LuaHttpMethod::Get, url.into(), None, Vec::new()),
                Err(LuaHttpErrorKind::InvalidRequest),
                "should reject {url}"
            );
        }
    }

    #[test]
    fn build_request_allows_public_urls_with_port_and_path() {
        for url in [
            "https://store.steampowered.com/app/730/",
            "http://api.example.com:8080/x?y=1",
        ] {
            let request = build_request(LuaHttpMethod::Get, url.into(), None, Vec::new()).unwrap();
            assert_eq!(request.url, url);
        }
    }
}
