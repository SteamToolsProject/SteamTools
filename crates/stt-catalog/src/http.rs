//! 基于 SteamTools wire v1 的自定义 HTTP Catalog provider.

use stt_core::{AppId, CatalogBundle};
use stt_platform::{winhttp_get, HttpError, WinHttpGetOptions};

use crate::{
    parse_catalog_wire_v1, CatalogError, CatalogProvider, CatalogResult, ProviderErrorKind,
};

const APP_ID_PLACEHOLDER: &str = "{app_id}";
const MAX_TEMPLATE_BYTES: usize = 2048;

/// 从 URL 模板拉取 wire v1 的 provider.
#[derive(Debug, Clone)]
pub struct CustomHttpCatalogProvider {
    url_template: String,
    options: WinHttpGetOptions,
}

impl CustomHttpCatalogProvider {
    /// 创建 provider. 模板必须包含且只包含一个 `{app_id}`.
    ///
    /// # Errors
    ///
    /// 模板为空、过长、scheme 不支持或占位符不唯一时返回配置错误.
    pub fn new(url_template: impl Into<String>, options: WinHttpGetOptions) -> CatalogResult<Self> {
        let url_template = url_template.into();
        validate_url_template(&url_template)?;
        Ok(Self {
            url_template,
            options,
        })
    }

    fn url_for(&self, app_id: AppId) -> String {
        self.url_template
            .replacen(APP_ID_PLACEHOLDER, &app_id.to_string(), 1)
    }
}

impl CatalogProvider for CustomHttpCatalogProvider {
    fn id(&self) -> &str {
        "custom_http"
    }

    fn fetch(&self, app_id: AppId) -> CatalogResult<CatalogBundle> {
        let response = winhttp_get(&self.url_for(app_id), self.options).map_err(map_http_error)?;
        if !(200..300).contains(&response.status) {
            let kind = if response.status == 404 {
                ProviderErrorKind::NotFound
            } else {
                ProviderErrorKind::Rejected
            };
            return Err(provider_error(
                kind,
                format!("HTTP status {}", response.status),
            ));
        }
        parse_catalog_wire_v1(app_id, &response.body)
    }
}

/// 校验 CustomHttp URL 模板, 不执行网络请求.
///
/// # Errors
///
/// 模板不满足受限 HTTP(S) 规则时返回 unavailable 错误.
pub fn validate_url_template(template: &str) -> CatalogResult<()> {
    let valid_scheme = template.starts_with("http://") || template.starts_with("https://");
    let placeholders = template.matches(APP_ID_PLACEHOLDER).count();
    let remainder = template.replacen(APP_ID_PLACEHOLDER, "", 1);
    if template.is_empty()
        || template.len() > MAX_TEMPLATE_BYTES
        || template.contains(['\0', '\n', '\r', '#'])
        || !valid_scheme
        || placeholders != 1
        || remainder.contains(['{', '}'])
    {
        return Err(provider_error(
            ProviderErrorKind::Unavailable,
            "invalid URL template; expected one {app_id} in an HTTP(S) URL",
        ));
    }
    Ok(())
}

/// 页面意图来源的模板校验: 格式校验之外, 拒绝回环/链路本地/组播/IPv6/localhost 主机,
/// 防止被伪造 intent 指向本机或内网服务. 用户手写配置不受此限 (走 `validate_url_template`).
///
/// # Errors
///
/// 模板格式非法或主机被禁止时返回 unavailable 错误.
pub fn validate_url_template_remote(template: &str) -> CatalogResult<()> {
    validate_url_template(template)?;
    let Some(host) = template_url_host(template) else {
        return Err(provider_error(
            ProviderErrorKind::Unavailable,
            "invalid URL template; expected one {app_id} in an HTTP(S) URL",
        ));
    };
    if host_forbidden(host) {
        return Err(provider_error(
            ProviderErrorKind::Unavailable,
            "URL template host is not allowed (loopback/link-local/multicast/IPv6/localhost)",
        ));
    }
    Ok(())
}

/// 提取 URL 模板的 host 段 (不含 scheme 和路径), 失败返回 None.
fn template_url_host(template: &str) -> Option<&str> {
    let rest = template
        .strip_prefix("http://")
        .or_else(|| template.strip_prefix("https://"))?;
    let end = rest.find('/').unwrap_or(rest.len());
    Some(&rest[..end])
}

/// host 是否禁止访问: 回环/链路本地/组播/IPv6/本地主机名.
/// 与 stt-config/src/lua_http.rs 的 host_forbidden 是双胞胎, 改动需两边同步.
/// 残余说明: 主机名被 DNS 重绑定到回环 IP 的场景本层不覆盖 (模板作者即信任边界).
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

fn map_http_error(error: HttpError) -> CatalogError {
    match error {
        HttpError::Timeout { .. } => provider_error(ProviderErrorKind::Timeout, error.to_string()),
        HttpError::ResponseTooLarge { actual, limit } => {
            CatalogError::PayloadTooLarge { actual, limit }
        }
        HttpError::RequestTooLarge { .. }
        | HttpError::InvalidUrl(_)
        | HttpError::InvalidOptions(_)
        | HttpError::Windows { .. } => {
            provider_error(ProviderErrorKind::Unavailable, error.to_string())
        }
    }
}

fn provider_error(kind: ProviderErrorKind, detail: impl Into<String>) -> CatalogError {
    CatalogError::Provider {
        provider: "custom_http".to_owned(),
        kind,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use stt_platform::WinHttpTimeouts;

    use super::*;

    struct FakeHttpServer {
        template: String,
        thread: Option<JoinHandle<()>>,
    }

    impl FakeHttpServer {
        fn spawn(status: u16, body: Vec<u8>, delay: Duration) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let thread = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => return,
                    }
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request);

                let reason = if status == 200 {
                    "OK"
                } else {
                    "Service Unavailable"
                };
                let headers = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(headers.as_bytes());
                std::thread::sleep(delay);
                let _ = stream.write_all(&body);
            });
            Self {
                template: format!("http://127.0.0.1:{port}/catalog/{{app_id}}"),
                thread: Some(thread),
            }
        }

        fn provider(&self, options: WinHttpGetOptions) -> CustomHttpCatalogProvider {
            // 直连回环测试服务器需要跳过模板主机校验; 该路径只测 wire 协议, 模板校验有专门测试.
            CustomHttpCatalogProvider {
                url_template: self.template.clone(),
                options,
            }
        }
    }

    impl Drop for FakeHttpServer {
        fn drop(&mut self) {
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }

    fn valid_body() -> Vec<u8> {
        br#"{"schema_version":1,"apps":[{"app_id":42,"depots":[]}] }"#.to_vec()
    }

    fn options(receive_ms: u32, max_body_bytes: usize) -> WinHttpGetOptions {
        WinHttpGetOptions {
            timeouts: WinHttpTimeouts {
                resolve_ms: 5_000,
                connect_ms: 5_000,
                send_ms: 5_000,
                receive_ms,
            },
            max_body_bytes,
        }
    }

    #[test]
    fn template_requires_exactly_one_app_id() {
        assert!(CustomHttpCatalogProvider::new(
            "http://127.0.0.1/catalog.json",
            WinHttpGetOptions::default()
        )
        .is_err());
        assert!(CustomHttpCatalogProvider::new(
            "http://127.0.0.1/{app_id}/{app_id}",
            WinHttpGetOptions::default()
        )
        .is_err());
    }

    #[test]
    fn rejects_loopback_link_local_multicast_and_ipv6_hosts() {
        for template in [
            "http://127.0.0.1/catalog/{app_id}",
            "http://127.255.1.1/catalog/{app_id}",
            "https://127.0.0.1:8443/catalog/{app_id}",
            "http://169.254.169.254/latest/{app_id}",
            "http://0.0.0.0/catalog/{app_id}",
            "http://224.0.0.1/catalog/{app_id}",
            "http://239.255.255.250/catalog/{app_id}",
            "http://[::1]/catalog/{app_id}",
            "http://fe80::1/catalog/{app_id}",
            "http://localhost/catalog/{app_id}",
            "http://foo.localhost/catalog/{app_id}",
            "https://steam.localhost:8443/catalog/{app_id}",
        ] {
            assert!(
                validate_url_template_remote(template).is_err(),
                "should reject {template}"
            );
        }
    }

    #[test]
    fn accepts_external_hosts() {
        for template in [
            "https://store.steampowered.com/app/{app_id}",
            "https://api.steampowered.com/catalog/{app_id}",
            "http://cdn.example.com/catalog/{app_id}",
            "https://sub.domain.test:8443/catalog/{app_id}",
            "http://1.2.3.4/catalog/{app_id}",
            "https://github.com/SteamToolsProject/SteamTools/raw/{app_id}",
        ] {
            assert!(
                validate_url_template_remote(template).is_ok(),
                "should accept {template}"
            );
        }
    }

    #[test]
    fn local_host_templates_are_allowed_in_written_config() {
        // 用户手写配置允许本地自建 catalog 服务; 页面意图路径才拒绝 (见 remote 校验).
        assert!(validate_url_template("http://127.0.0.1:8081/catalog/{app_id}").is_ok());
    }

    #[test]
    fn fetches_wire_v1_from_local_http() {
        let _guard = crate::http_test_guard();
        let server = FakeHttpServer::spawn(200, valid_body(), Duration::ZERO);

        let bundle = server.provider(options(1_000, 1024)).fetch(42).unwrap();

        assert_eq!(bundle.apps, vec![42]);
    }

    #[test]
    fn non_success_status_is_rejected_without_parsing_body() {
        let _guard = crate::http_test_guard();
        let server = FakeHttpServer::spawn(503, b"not-json".to_vec(), Duration::ZERO);

        let error = server.provider(options(1_000, 1024)).fetch(42).unwrap_err();

        assert!(matches!(
            error,
            CatalogError::Provider {
                kind: ProviderErrorKind::Rejected,
                ..
            }
        ));
    }

    #[test]
    fn receive_timeout_has_stable_classification() {
        let _guard = crate::http_test_guard();
        let server = FakeHttpServer::spawn(200, valid_body(), Duration::from_millis(6_000));

        let error = server.provider(options(1_000, 1024)).fetch(42).unwrap_err();

        assert!(matches!(
            error,
            CatalogError::Provider {
                kind: ProviderErrorKind::Timeout,
                ..
            }
        ));
    }

    #[test]
    fn response_limit_is_enforced_while_reading() {
        let _guard = crate::http_test_guard();
        let server = FakeHttpServer::spawn(200, vec![b'x'; 256], Duration::ZERO);

        let error = server.provider(options(1_000, 32)).fetch(42).unwrap_err();

        assert!(matches!(
            error,
            CatalogError::PayloadTooLarge { limit: 32, .. }
        ));
    }

    #[test]
    fn malformed_json_is_not_persistable_catalog() {
        let _guard = crate::http_test_guard();
        let server = FakeHttpServer::spawn(200, b"not-json".to_vec(), Duration::ZERO);

        let error = server.provider(options(1_000, 1024)).fetch(42).unwrap_err();

        assert!(matches!(error, CatalogError::Json(_)));
    }

    #[test]
    fn semantically_invalid_catalog_is_rejected() {
        let _guard = crate::http_test_guard();
        let body = br#"{"schema_version":1,"apps":[{"app_id":7}]}"#.to_vec();
        let server = FakeHttpServer::spawn(200, body, Duration::ZERO);

        let error = server.provider(options(1_000, 1024)).fetch(42).unwrap_err();

        assert!(matches!(error, CatalogError::RequestedAppMissing(42)));
    }
}
