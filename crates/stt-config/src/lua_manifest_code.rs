//! 上游兼容的 manifest request code Lua 执行器.

use std::sync::Arc;

use mlua::{Function, Lua, Value};

use crate::lua_http::{register_lua_http, LuaHttpClient};

const MAX_SOURCE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LuaManifestCodeErrorKind {
    Unavailable,
    Rejected,
    InvalidResponse,
    Internal,
}

pub type LuaManifestCodeResult = Result<Option<u64>, LuaManifestCodeErrorKind>;

/// 每次调用都在当前后台 worker 创建独立 Lua VM.
pub struct LuaManifestCodeExecutor {
    source: String,
    http_client: Option<Arc<dyn LuaHttpClient>>,
}

impl LuaManifestCodeExecutor {
    pub fn new(
        source: impl Into<String>,
        http_client: Option<Arc<dyn LuaHttpClient>>,
    ) -> Result<Self, LuaManifestCodeErrorKind> {
        let source = source.into();
        if source.is_empty() || source.len() > MAX_SOURCE_BYTES {
            return Err(LuaManifestCodeErrorKind::Unavailable);
        }
        Ok(Self {
            source,
            http_client,
        })
    }

    /// 调用 `fetch_manifest_code_ex(app_id, depot_id, manifest_gid)`.
    pub fn resolve_extended(
        &self,
        app_id: u32,
        depot_id: u32,
        manifest_gid: u64,
    ) -> LuaManifestCodeResult {
        let gid =
            i64::try_from(manifest_gid).map_err(|_| LuaManifestCodeErrorKind::InvalidResponse)?;
        self.with_lua("fetch_manifest_code_ex", |function| {
            function.call::<Value>((app_id, depot_id, gid))
        })
    }

    /// 调用 `fetch_manifest_code(manifest_gid)`.
    pub fn resolve_basic(&self, manifest_gid: u64) -> LuaManifestCodeResult {
        let gid =
            i64::try_from(manifest_gid).map_err(|_| LuaManifestCodeErrorKind::InvalidResponse)?;
        self.with_lua("fetch_manifest_code", |function| {
            function.call::<Value>(gid)
        })
    }

    fn with_lua(
        &self,
        function_name: &str,
        call: impl FnOnce(Function) -> mlua::Result<Value>,
    ) -> LuaManifestCodeResult {
        let lua = Lua::new();
        if let Some(client) = &self.http_client {
            register_lua_http(&lua, Arc::clone(client))
                .map_err(|_| LuaManifestCodeErrorKind::Internal)?;
        }
        lua.load(&self.source)
            .exec()
            .map_err(|_| LuaManifestCodeErrorKind::Rejected)?;
        let function: Function = lua
            .globals()
            .get(function_name)
            .map_err(|_| LuaManifestCodeErrorKind::Unavailable)?;
        let value = call(function).map_err(|_| LuaManifestCodeErrorKind::Rejected)?;
        parse_code(value)
    }
}

fn parse_code(value: Value) -> LuaManifestCodeResult {
    let code = match value {
        Value::Nil => return Ok(None),
        Value::Integer(value) => u64::try_from(value).ok(),
        Value::String(value) => {
            let bytes = value.as_bytes();
            if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
                None
            } else {
                std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|text| text.parse().ok())
            }
        }
        _ => None,
    }
    .filter(|code| *code != 0)
    .ok_or(LuaManifestCodeErrorKind::InvalidResponse)?;
    Ok(Some(code))
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
                body: b"123".to_vec(),
            })
        }
    }

    #[test]
    fn extended_receives_all_ids_and_keeps_decimal_precision() {
        let executor = LuaManifestCodeExecutor::new(
            r#"
function fetch_manifest_code_ex(app_id, depot_id, gid)
  if app_id == 10 and depot_id == 20 and gid == 30 then
    return "18446744073709551615"
  end
end
"#,
            None,
        )
        .unwrap();

        assert_eq!(executor.resolve_extended(10, 20, 30), Ok(Some(u64::MAX)));
    }

    #[test]
    fn basic_nil_is_a_normal_miss() {
        let executor =
            LuaManifestCodeExecutor::new("function fetch_manifest_code(gid) return nil end", None)
                .unwrap();

        assert_eq!(executor.resolve_basic(30), Ok(None));
    }

    #[test]
    fn invalid_or_zero_values_are_rejected() {
        let invalid = LuaManifestCodeExecutor::new(
            "function fetch_manifest_code(gid) return '12x' end",
            None,
        )
        .unwrap();
        let zero =
            LuaManifestCodeExecutor::new("function fetch_manifest_code(gid) return 0 end", None)
                .unwrap();

        assert_eq!(
            invalid.resolve_basic(30),
            Err(LuaManifestCodeErrorKind::InvalidResponse)
        );
        assert_eq!(
            zero.resolve_basic(30),
            Err(LuaManifestCodeErrorKind::InvalidResponse)
        );
    }

    #[test]
    fn injected_http_is_only_called_from_executor_thread() {
        let client = Arc::new(StubHttpClient::default());
        let executor = LuaManifestCodeExecutor::new(
            r#"
function fetch_manifest_code(gid)
  local body, status = http_get("https://example.test/" .. gid)
  if status ~= 200 then return nil end
  return body
end
"#,
            Some(client.clone()),
        )
        .unwrap();

        assert_eq!(executor.resolve_basic(30), Ok(Some(123)));
        assert_eq!(client.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn missing_function_is_unavailable() {
        let executor = LuaManifestCodeExecutor::new("return true", None).unwrap();

        assert_eq!(
            executor.resolve_basic(30),
            Err(LuaManifestCodeErrorKind::Unavailable)
        );
    }
}
