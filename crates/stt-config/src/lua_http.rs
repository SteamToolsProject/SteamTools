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
}
