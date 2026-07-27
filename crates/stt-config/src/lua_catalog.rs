//! SteamTools 扩展的 Lua Catalog provider.

use std::sync::Arc;

use mlua::{Function, Lua, Value};
use stt_catalog::{
    parse_catalog_wire_v1, CatalogError, CatalogProvider, CatalogResult, ProviderErrorKind,
};
use stt_core::{AppId, CatalogBundle};

use crate::lua_http::{register_lua_http, LuaHttpClient};

const MAX_SOURCE_BYTES: usize = 1024 * 1024;

/// 执行 `fetch_catalog(app_id)` 并解析 wire v1 JSON 的 provider.
pub struct LuaCatalogProvider {
    source: String,
    http_client: Option<Arc<dyn LuaHttpClient>>,
}

impl LuaCatalogProvider {
    /// 创建 provider. Lua VM 会在每次 `fetch` 的调用线程内创建.
    ///
    /// # Errors
    ///
    /// 脚本为空或超过大小上限时返回 unavailable.
    pub fn new(
        source: impl Into<String>,
        http_client: Option<Arc<dyn LuaHttpClient>>,
    ) -> CatalogResult<Self> {
        let source = source.into();
        if source.is_empty() || source.len() > MAX_SOURCE_BYTES {
            return Err(provider_error(
                ProviderErrorKind::Unavailable,
                "Lua Catalog source is empty or too large",
            ));
        }
        Ok(Self {
            source,
            http_client,
        })
    }
}

impl CatalogProvider for LuaCatalogProvider {
    fn id(&self) -> &str {
        "lua"
    }

    fn fetch(&self, app_id: AppId) -> CatalogResult<CatalogBundle> {
        let lua = Lua::new();
        if let Some(client) = &self.http_client {
            register_lua_http(&lua, Arc::clone(client)).map_err(|_| {
                provider_error(
                    ProviderErrorKind::Unavailable,
                    "Lua HTTP helpers could not be registered",
                )
            })?;
        }
        lua.load(&self.source).exec().map_err(|_| {
            provider_error(
                ProviderErrorKind::Rejected,
                "Lua Catalog source failed to load",
            )
        })?;
        let function: Function = lua.globals().get("fetch_catalog").map_err(|_| {
            provider_error(
                ProviderErrorKind::Unavailable,
                "fetch_catalog(app_id) is not defined",
            )
        })?;
        let value: Value = function.call(app_id).map_err(|_| {
            provider_error(ProviderErrorKind::Rejected, "fetch_catalog(app_id) failed")
        })?;
        match value {
            Value::String(body) => parse_catalog_wire_v1(app_id, &body.as_bytes()),
            Value::Nil => Err(provider_error(
                ProviderErrorKind::NotFound,
                "fetch_catalog(app_id) returned nil",
            )),
            _ => Err(provider_error(
                ProviderErrorKind::Rejected,
                "fetch_catalog(app_id) must return wire v1 JSON string or nil",
            )),
        }
    }
}

fn provider_error(kind: ProviderErrorKind, detail: impl Into<String>) -> CatalogError {
    CatalogError::Provider {
        provider: "lua".to_owned(),
        kind,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crate::{LuaHttpErrorKind, LuaHttpRequest, LuaHttpResponse};

    use super::*;

    #[derive(Default)]
    struct StubHttpClient {
        requests: Mutex<Vec<LuaHttpRequest>>,
    }

    impl LuaHttpClient for StubHttpClient {
        fn execute(&self, request: LuaHttpRequest) -> Result<LuaHttpResponse, LuaHttpErrorKind> {
            self.requests.lock().unwrap().push(request);
            Ok(LuaHttpResponse {
                status: 200,
                body: br#"{"schema_version":1,"apps":[{"app_id":42}]}"#.to_vec(),
            })
        }
    }

    #[test]
    fn fetch_catalog_returns_independent_wire_v1() {
        let provider = LuaCatalogProvider::new(
            r#"
function fetch_catalog(app_id)
  return '{"schema_version":1,"apps":[{"app_id":' .. app_id .. '}]}'
end
"#,
            None,
        )
        .unwrap();

        let bundle = provider.fetch(42).unwrap();

        assert_eq!(bundle.apps, vec![42]);
    }

    #[test]
    fn fetch_catalog_can_use_injected_http_on_caller_thread() {
        let client = Arc::new(StubHttpClient::default());
        let provider = LuaCatalogProvider::new(
            r#"
function fetch_catalog(app_id)
  local body, status = http_get("https://example.test/catalog/" .. app_id)
  if status ~= 200 then return nil end
  return body
end
"#,
            Some(client.clone()),
        )
        .unwrap();

        let bundle = provider.fetch(42).unwrap();

        assert_eq!(bundle.apps, vec![42]);
        assert_eq!(client.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn missing_fetch_catalog_is_explicitly_unavailable() {
        let provider = LuaCatalogProvider::new("return true", None).unwrap();

        let error = provider.fetch(42).unwrap_err();

        assert!(matches!(
            error,
            CatalogError::Provider {
                kind: ProviderErrorKind::Unavailable,
                ..
            }
        ));
    }
}
