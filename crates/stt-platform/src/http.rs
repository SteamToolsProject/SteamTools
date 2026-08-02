//! 受限 WinHTTP transport.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;

use windows::core::{w, Error as WindowsError, HRESULT, PCWSTR};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpCrackUrl, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpSetOption, WinHttpSetTimeouts, ERROR_WINHTTP_TIMEOUT, URL_COMPONENTS,
    WINHTTP_ACCESS_TYPE, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ACCESS_TYPE_NO_PROXY,
    WINHTTP_FLAG_SECURE, WINHTTP_INTERNET_SCHEME_HTTP, WINHTTP_INTERNET_SCHEME_HTTPS,
    WINHTTP_OPEN_REQUEST_FLAGS, WINHTTP_OPTION_RECEIVE_RESPONSE_TIMEOUT,
    WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_LOCATION, WINHTTP_QUERY_STATUS_CODE,
};

const MAX_URL_CHARS: usize = 2048;
const MAX_HEADERS: usize = 32;
const MAX_HEADER_CHARS: usize = 16 * 1024;
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// transport 允许的 HTTP 方法.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
}

/// WinHTTP 四段超时, 单位毫秒.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WinHttpTimeouts {
    pub resolve_ms: u32,
    pub connect_ms: u32,
    pub send_ms: u32,
    pub receive_ms: u32,
}

impl Default for WinHttpTimeouts {
    fn default() -> Self {
        Self {
            resolve_ms: 5_000,
            connect_ms: 5_000,
            send_ms: 10_000,
            receive_ms: 10_000,
        }
    }
}

/// 单次 GET 的资源限制.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WinHttpGetOptions {
    pub timeouts: WinHttpTimeouts,
    pub max_body_bytes: usize,
}

/// 带可选请求体的 HTTP 请求资源限制.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WinHttpRequestOptions {
    pub timeouts: WinHttpTimeouts,
    pub max_request_body_bytes: usize,
    pub max_response_body_bytes: usize,
}

impl Default for WinHttpRequestOptions {
    fn default() -> Self {
        Self {
            timeouts: WinHttpTimeouts::default(),
            max_request_body_bytes: 256 * 1024,
            max_response_body_bytes: 1024 * 1024,
        }
    }
}

impl Default for WinHttpGetOptions {
    fn default() -> Self {
        Self {
            timeouts: WinHttpTimeouts::default(),
            max_body_bytes: 1024 * 1024,
        }
    }
}

/// HTTP 响应. 非 2xx 也会保留状态码并返回这里.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    /// `Location` 响应头 (重定向场景用). 没有该头或为空时为 None.
    ///
    /// WinHTTP 默认会自动跟随重定向; 我们显式禁用 (REDIRECT_POLICY_NEVER),
    /// 所以 3xx 的 Location 由调用方自己决定怎么走.
    pub location: Option<String>,
}

/// WinHTTP transport 错误.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("invalid HTTP URL: {0}")]
    InvalidUrl(&'static str),
    #[error("invalid HTTP options: {0}")]
    InvalidOptions(&'static str),
    #[error("WinHTTP operation {operation} timed out")]
    Timeout { operation: &'static str },
    #[error("WinHTTP operation {operation} failed with {code}")]
    Windows {
        operation: &'static str,
        code: HRESULT,
    },
    #[error("HTTP response exceeds {limit} bytes (at least {actual})")]
    ResponseTooLarge { actual: usize, limit: usize },
    #[error("HTTP request body exceeds {limit} bytes (actual {actual})")]
    RequestTooLarge { actual: usize, limit: usize },
}

struct InternetHandle(*mut c_void);

impl InternetHandle {
    fn new(raw: *mut c_void, operation: &'static str) -> Result<Self, HttpError> {
        if raw.is_null() {
            Err(map_windows_error(operation, WindowsError::from_win32()))
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for InternetHandle {
    fn drop(&mut self) {
        // SAFETY: 句柄由 WinHTTP 创建, 每个 wrapper 只关闭一次.
        let _ = unsafe { WinHttpCloseHandle(self.0) };
    }
}

struct ParsedUrl {
    host: Vec<u16>,
    object: Vec<u16>,
    port: u16,
    secure: bool,
}

/// 执行一次受限 GET.
///
/// 只接受 HTTP/HTTPS, 禁止重定向和 URL 用户信息, 并在读取过程中强制响应上限.
///
/// # Errors
///
/// URL 无效、WinHTTP 失败/超时或响应超过上限时返回 [`HttpError`].
pub fn winhttp_get(url: &str, options: WinHttpGetOptions) -> Result<HttpResponse, HttpError> {
    let options = WinHttpRequestOptions {
        timeouts: options.timeouts,
        max_request_body_bytes: 0,
        max_response_body_bytes: options.max_body_bytes,
    };
    winhttp_request(HttpMethod::Get, url, &[], &[], options)
}

/// 执行一次受限 POST.
///
/// # Errors
///
/// URL/请求头无效、请求或响应超限、WinHTTP 失败/超时时返回 [`HttpError`].
pub fn winhttp_post(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    options: WinHttpRequestOptions,
) -> Result<HttpResponse, HttpError> {
    winhttp_request(HttpMethod::Post, url, headers, body, options)
}

/// 执行一次受限 GET/POST.
///
/// 只接受 HTTP/HTTPS, 禁止重定向和 URL 用户信息, 并限制请求头、请求体和响应体.
///
/// # Errors
///
/// URL/请求头无效、请求或响应超限、WinHTTP 失败/超时时返回 [`HttpError`].
pub fn winhttp_request(
    method: HttpMethod,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    options: WinHttpRequestOptions,
) -> Result<HttpResponse, HttpError> {
    let parsed = parse_url(url)?;
    let timeouts = checked_timeouts(options.timeouts)?;
    let headers = encode_headers(headers)?;
    if body.len() > options.max_request_body_bytes {
        return Err(HttpError::RequestTooLarge {
            actual: body.len(),
            limit: options.max_request_body_bytes,
        });
    }
    if method == HttpMethod::Get && !body.is_empty() {
        return Err(HttpError::InvalidOptions("GET body is not supported"));
    }

    // SAFETY: URL component 在调用期间有效且以 NUL 结尾; header 带显式长度; 句柄由 RAII 管理.
    unsafe {
        let session = InternetHandle::new(
            WinHttpOpen(
                w!("SteamTools/0.1"),
                session_access_type(&parsed.host),
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ),
            "open",
        )?;
        map_result(
            "set_timeouts",
            WinHttpSetTimeouts(session.0, timeouts.0, timeouts.1, timeouts.2, timeouts.3),
        )?;

        let connection = InternetHandle::new(
            WinHttpConnect(session.0, PCWSTR(parsed.host.as_ptr()), parsed.port, 0),
            "connect",
        )?;
        let flags = if parsed.secure {
            WINHTTP_FLAG_SECURE
        } else {
            WINHTTP_OPEN_REQUEST_FLAGS(0)
        };
        let verb = match method {
            HttpMethod::Get => w!("GET"),
            HttpMethod::Post => w!("POST"),
        };
        let request = InternetHandle::new(
            WinHttpOpenRequest(
                connection.0,
                verb,
                PCWSTR(parsed.object.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                ptr::null(),
                flags,
            ),
            "open_request",
        )?;
        map_result(
            "set_request_timeouts",
            WinHttpSetTimeouts(request.0, timeouts.0, timeouts.1, timeouts.2, timeouts.3),
        )?;
        let receive_response_timeout = options.timeouts.receive_ms.to_ne_bytes();
        map_result(
            "set_receive_response_timeout",
            WinHttpSetOption(
                Some(request.0.cast_const()),
                WINHTTP_OPTION_RECEIVE_RESPONSE_TIMEOUT,
                Some(&receive_response_timeout),
            ),
        )?;
        let redirect_policy = WINHTTP_OPTION_REDIRECT_POLICY_NEVER.to_ne_bytes();
        map_result(
            "disable_redirects",
            WinHttpSetOption(
                Some(request.0.cast_const()),
                WINHTTP_OPTION_REDIRECT_POLICY,
                Some(&redirect_policy),
            ),
        )?;
        let body_len = u32::try_from(body.len())
            .map_err(|_| HttpError::InvalidOptions("request body exceeds u32::MAX"))?;
        let body_pointer = (!body.is_empty()).then_some(body.as_ptr().cast());
        map_result(
            "send",
            WinHttpSendRequest(
                request.0,
                (!headers.is_empty()).then_some(headers.as_slice()),
                body_pointer,
                body_len,
                body_len,
                0,
            ),
        )?;
        map_result(
            "receive",
            WinHttpReceiveResponse(request.0, ptr::null_mut()),
        )?;

        let status = query_status(request.0)?;
        let location = query_location(request.0)?;
        let body = read_body(request.0, options.max_response_body_bytes)?;
        Ok(HttpResponse {
            status,
            body,
            location,
        })
    }
}

fn session_access_type(host: &[u16]) -> WINHTTP_ACCESS_TYPE {
    let host = String::from_utf16_lossy(host.strip_suffix(&[0]).unwrap_or(host));
    if matches!(host.as_str(), "127.0.0.1" | "::1") || host.eq_ignore_ascii_case("localhost") {
        // 回环请求不应进入系统代理自动发现, 否则本地 provider 会受外部网络状态影响.
        WINHTTP_ACCESS_TYPE_NO_PROXY
    } else {
        WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY
    }
}

fn encode_headers(headers: &[(String, String)]) -> Result<Vec<u16>, HttpError> {
    if headers.len() > MAX_HEADERS {
        return Err(HttpError::InvalidOptions("too many request headers"));
    }

    let mut encoded = String::new();
    for (name, value) in headers {
        let valid_name = !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        let valid_value = !value.contains(['\0', '\r', '\n']);
        if !valid_name || !valid_value {
            return Err(HttpError::InvalidOptions("invalid request header"));
        }
        encoded.push_str(name);
        encoded.push_str(": ");
        encoded.push_str(value);
        encoded.push_str("\r\n");
    }
    if encoded.chars().count() > MAX_HEADER_CHARS {
        return Err(HttpError::InvalidOptions("request headers are too large"));
    }
    Ok(encoded.encode_utf16().collect())
}

fn checked_timeouts(timeouts: WinHttpTimeouts) -> Result<(i32, i32, i32, i32), HttpError> {
    fn one(value: u32) -> Result<i32, HttpError> {
        i32::try_from(value).map_err(|_| HttpError::InvalidOptions("timeout exceeds i32::MAX"))
    }

    Ok((
        one(timeouts.resolve_ms)?,
        one(timeouts.connect_ms)?,
        one(timeouts.send_ms)?,
        one(timeouts.receive_ms)?,
    ))
}

fn parse_url(url: &str) -> Result<ParsedUrl, HttpError> {
    if url.is_empty() || url.chars().count() > MAX_URL_CHARS {
        return Err(HttpError::InvalidUrl("URL is empty or too long"));
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(HttpError::InvalidUrl("only HTTP and HTTPS are supported"));
    }
    if url.contains('#') {
        return Err(HttpError::InvalidUrl("URL fragments are not allowed"));
    }

    let wide: Vec<u16> = url.encode_utf16().collect();
    let mut parts = URL_COMPONENTS {
        dwStructSize: size_of::<URL_COMPONENTS>() as u32,
        dwSchemeLength: u32::MAX,
        dwHostNameLength: u32::MAX,
        dwUserNameLength: u32::MAX,
        dwPasswordLength: u32::MAX,
        dwUrlPathLength: u32::MAX,
        dwExtraInfoLength: u32::MAX,
        ..URL_COMPONENTS::default()
    };
    // SAFETY: `wide` 在解析和复制各 component 的整个过程中保持有效.
    unsafe { WinHttpCrackUrl(&wide, 0, &mut parts) }
        .map_err(|error| map_windows_error("crack_url", error))?;

    if parts.dwUserNameLength != 0 || parts.dwPasswordLength != 0 {
        return Err(HttpError::InvalidUrl("URL user information is not allowed"));
    }
    let secure = if parts.nScheme == WINHTTP_INTERNET_SCHEME_HTTP {
        false
    } else if parts.nScheme == WINHTTP_INTERNET_SCHEME_HTTPS {
        true
    } else {
        return Err(HttpError::InvalidUrl("only HTTP and HTTPS are supported"));
    };

    let mut host = copy_component(parts.lpszHostName.0, parts.dwHostNameLength)?;
    if host.is_empty() {
        return Err(HttpError::InvalidUrl("host is missing"));
    }
    host.push(0);

    let mut object = copy_component(parts.lpszUrlPath.0, parts.dwUrlPathLength)?;
    if object.is_empty() {
        object.push('/' as u16);
    }
    object.extend(copy_component(
        parts.lpszExtraInfo.0,
        parts.dwExtraInfoLength,
    )?);
    object.push(0);

    Ok(ParsedUrl {
        host,
        object,
        port: parts.nPort,
        secure,
    })
}

fn copy_component(pointer: *mut u16, len: u32) -> Result<Vec<u16>, HttpError> {
    if len == 0 {
        return Ok(Vec::new());
    }
    if pointer.is_null() {
        return Err(HttpError::InvalidUrl("URL component is missing"));
    }
    let len = usize::try_from(len).map_err(|_| HttpError::InvalidUrl("URL is too long"))?;
    // SAFETY: WinHttpCrackUrl 返回指向输入 URL 的 component, `len` 由 WinHTTP 给出.
    Ok(unsafe { std::slice::from_raw_parts(pointer, len) }.to_vec())
}

fn query_status(request: *mut c_void) -> Result<u16, HttpError> {
    let mut status = 0u32;
    let mut bytes = size_of::<u32>() as u32;
    let mut index = 0u32;
    // SAFETY: `request` 在调用期间有效, status/buffer length/index 都指向可写栈变量.
    let result = unsafe {
        WinHttpQueryHeaders(
            request,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(ptr::addr_of_mut!(status).cast()),
            &mut bytes,
            &mut index,
        )
    };
    map_result("query_status", result)?;
    u16::try_from(status).map_err(|_| HttpError::InvalidUrl("invalid HTTP status code"))
}

/// 读 `Location` 响应头; 没有该头时返回 None.
fn query_location(request: *mut c_void) -> Result<Option<String>, HttpError> {
    let mut buf = [0u16; 2048];
    let mut index = 0u32;
    // SAFETY: `request` 在调用期间有效, buf/index 都是可写缓冲.
    let result = unsafe {
        WinHttpQueryHeaders(
            request,
            WINHTTP_QUERY_LOCATION,
            PCWSTR::null(),
            Some(buf.as_mut_ptr().cast()),
            &mut (buf.len() as u32),
            &mut index,
        )
    };
    if result.is_err() {
        return Ok(None);
    }
    let len = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
    if len == 0 {
        return Ok(None);
    }
    Ok(Some(String::from_utf16_lossy(&buf[..len])))
}

fn read_body(request: *mut c_void, limit: usize) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::new();
    loop {
        let remaining_with_sentinel = limit.saturating_sub(body.len()).saturating_add(1);
        let requested = remaining_with_sentinel.clamp(1, READ_CHUNK_BYTES);
        let mut chunk = vec![0u8; requested];
        let mut read = 0u32;
        // SAFETY: `request` 在调用期间有效, chunk/read 都是足够大的可写缓冲.
        let result = unsafe {
            WinHttpReadData(
                request,
                chunk.as_mut_ptr().cast(),
                requested as u32,
                &mut read,
            )
        };
        map_result("read", result)?;
        let read = read as usize;
        if read == 0 {
            return Ok(body);
        }
        let actual = body.len().saturating_add(read);
        if actual > limit {
            return Err(HttpError::ResponseTooLarge { actual, limit });
        }
        chunk.truncate(read);
        body.extend_from_slice(&chunk);
    }
}

fn map_result(operation: &'static str, result: windows::core::Result<()>) -> Result<(), HttpError> {
    result.map_err(|error| map_windows_error(operation, error))
}

fn map_windows_error(operation: &'static str, error: WindowsError) -> HttpError {
    if error.code() == HRESULT::from_win32(ERROR_WINHTTP_TIMEOUT) {
        HttpError::Timeout { operation }
    } else {
        HttpError::Windows {
            operation,
            code: error.code(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn rejects_non_http_scheme() {
        assert!(matches!(
            winhttp_get("file:///C:/x", WinHttpGetOptions::default()),
            Err(HttpError::InvalidUrl(_))
        ));
    }

    #[test]
    fn rejects_url_user_information() {
        assert!(matches!(
            winhttp_get("http://user:pass@127.0.0.1/x", WinHttpGetOptions::default()),
            Err(HttpError::InvalidUrl(_))
        ));
    }

    #[test]
    fn rejects_header_injection() {
        let headers = vec![("X-Test".to_owned(), "ok\r\nInjected: yes".to_owned())];

        let error = winhttp_request(
            HttpMethod::Get,
            "http://127.0.0.1/",
            &headers,
            &[],
            WinHttpRequestOptions::default(),
        )
        .unwrap_err();

        assert!(matches!(error, HttpError::InvalidOptions(_)));
    }

    #[test]
    fn rejects_post_body_over_limit_before_network() {
        let options = WinHttpRequestOptions {
            max_request_body_bytes: 2,
            ..WinHttpRequestOptions::default()
        };

        let error = winhttp_post("http://127.0.0.1/", &[], b"abc", options).unwrap_err();

        assert!(matches!(
            error,
            HttpError::RequestTooLarge {
                actual: 3,
                limit: 2
            }
        ));
    }

    #[test]
    fn loopback_requests_bypass_automatic_proxy() {
        for host in ["127.0.0.1", "::1", "localhost"] {
            let mut host = host.encode_utf16().collect::<Vec<_>>();
            host.push(0);
            assert_eq!(session_access_type(&host), WINHTTP_ACCESS_TYPE_NO_PROXY);
        }
        assert_eq!(
            session_access_type(&"example.com\0".encode_utf16().collect::<Vec<_>>()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY
        );
    }

    #[test]
    fn post_sends_headers_and_body_to_local_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.ends_with(b"\r\n\r\npayload") {
                let read = stream.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            stream
                .write_all(
                    b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                )
                .unwrap();
            request
        });
        let options = WinHttpRequestOptions {
            timeouts: WinHttpTimeouts {
                resolve_ms: 1_000,
                connect_ms: 1_000,
                send_ms: 1_000,
                receive_ms: 1_000,
            },
            ..WinHttpRequestOptions::default()
        };

        let response = winhttp_post(
            &format!("http://127.0.0.1:{port}/submit"),
            &[("X-Test".to_owned(), "yes".to_owned())],
            b"payload",
            options,
        )
        .unwrap();
        let request = String::from_utf8(server.join().unwrap()).unwrap();

        assert!(request.starts_with("POST /submit HTTP/1.1\r\n"));
        assert!(request.contains("X-Test: yes\r\n"));
        assert!(request.ends_with("\r\n\r\npayload"));
        assert_eq!(response.status, 201);
        assert_eq!(response.body, b"ok");
    }
}
