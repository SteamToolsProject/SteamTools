//! 受限 WinHTTP GET transport.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;

use windows::core::{w, Error as WindowsError, HRESULT, PCWSTR};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpCrackUrl, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpSetOption, WinHttpSetTimeouts, ERROR_WINHTTP_TIMEOUT, URL_COMPONENTS,
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_INTERNET_SCHEME_HTTP,
    WINHTTP_INTERNET_SCHEME_HTTPS, WINHTTP_OPEN_REQUEST_FLAGS,
    WINHTTP_OPTION_RECEIVE_RESPONSE_TIMEOUT, WINHTTP_OPTION_REDIRECT_POLICY,
    WINHTTP_OPTION_REDIRECT_POLICY_NEVER, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};

const MAX_URL_CHARS: usize = 2048;
const READ_CHUNK_BYTES: usize = 64 * 1024;

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

impl Default for WinHttpGetOptions {
    fn default() -> Self {
        Self {
            timeouts: WinHttpTimeouts::default(),
            max_body_bytes: 1024 * 1024,
        }
    }
}

/// GET 响应. 非 2xx 也会保留状态码并返回这里.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
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
    let parsed = parse_url(url)?;
    let timeouts = checked_timeouts(options.timeouts)?;

    // SAFETY: 所有字符串在调用期间有效且以 NUL 结尾; 句柄由 RAII wrapper 管理.
    unsafe {
        let session = InternetHandle::new(
            WinHttpOpen(
                w!("SteamTools/0.1"),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
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
        let request = InternetHandle::new(
            WinHttpOpenRequest(
                connection.0,
                w!("GET"),
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
        map_result("send", WinHttpSendRequest(request.0, None, None, 0, 0, 0))?;
        map_result(
            "receive",
            WinHttpReceiveResponse(request.0, ptr::null_mut()),
        )?;

        let status = query_status(request.0)?;
        let body = read_body(request.0, options.max_body_bytes)?;
        Ok(HttpResponse { status, body })
    }
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
}
