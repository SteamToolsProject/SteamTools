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

fn map_http_error(error: HttpError) -> CatalogError {
    match error {
        HttpError::Timeout { .. } => provider_error(ProviderErrorKind::Timeout, error.to_string()),
        HttpError::ResponseTooLarge { actual, limit } => {
            CatalogError::PayloadTooLarge { actual, limit }
        }
        HttpError::InvalidUrl(_) | HttpError::InvalidOptions(_) | HttpError::Windows { .. } => {
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
            CustomHttpCatalogProvider::new(self.template.clone(), options).unwrap()
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
                resolve_ms: 1_000,
                connect_ms: 1_000,
                send_ms: 1_000,
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
    fn fetches_wire_v1_from_local_http() {
        let server = FakeHttpServer::spawn(200, valid_body(), Duration::ZERO);

        let bundle = server.provider(options(1_000, 1024)).fetch(42).unwrap();

        assert_eq!(bundle.apps, vec![42]);
    }

    #[test]
    fn non_success_status_is_rejected_without_parsing_body() {
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
        let server = FakeHttpServer::spawn(200, vec![b'x'; 256], Duration::ZERO);

        let error = server.provider(options(1_000, 32)).fetch(42).unwrap_err();

        assert!(matches!(
            error,
            CatalogError::PayloadTooLarge { limit: 32, .. }
        ));
    }

    #[test]
    fn malformed_json_is_not_persistable_catalog() {
        let server = FakeHttpServer::spawn(200, b"not-json".to_vec(), Duration::ZERO);

        let error = server.provider(options(1_000, 1024)).fetch(42).unwrap_err();

        assert!(matches!(error, CatalogError::Json(_)));
    }

    #[test]
    fn semantically_invalid_catalog_is_rejected() {
        let body = br#"{"schema_version":1,"apps":[{"app_id":7}]}"#.to_vec();
        let server = FakeHttpServer::spawn(200, body, Duration::ZERO);

        let error = server.provider(options(1_000, 1024)).fetch(42).unwrap_err();

        assert!(matches!(error, CatalogError::RequestedAppMissing(42)));
    }
}
