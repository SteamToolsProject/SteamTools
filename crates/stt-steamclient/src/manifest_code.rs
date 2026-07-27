//! Manifest request code 的纯逻辑 provider chain.

use stt_core::{AppId, DepotId};

/// 一次 manifest request code 查询.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestCodeRequest {
    pub app_id: Option<AppId>,
    pub depot_id: Option<DepotId>,
    pub manifest_gid: u64,
}

/// provider 在固定回退顺序中的职责.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestCodeStage {
    LuaExtended,
    LuaBasic,
    Http,
}

/// 可安全进入诊断记录的失败分类.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestCodeFailureKind {
    Unavailable,
    Timeout,
    Rejected,
    InvalidResponse,
    Internal,
}

/// 单个 provider 的结果. `None` 表示正常未命中, 应继续回退.
pub type ManifestCodeProviderResult = Result<Option<u64>, ManifestCodeFailureKind>;

/// Manifest request code provider.
pub trait ManifestCodeProvider: Send + Sync {
    /// 稳定标识, 仅用于诊断和最终来源.
    fn id(&self) -> &str;

    /// 尝试解析 request code.
    fn resolve(&self, request: ManifestCodeRequest) -> ManifestCodeProviderResult;
}

/// 一次 provider 尝试的脱敏结果.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestCodeTraceEntry {
    pub provider: String,
    pub stage: ManifestCodeStage,
    pub outcome: ManifestCodeTraceOutcome,
}

/// trace 不携带 provider 错误正文或响应正文.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestCodeTraceOutcome {
    Hit,
    Miss,
    Failed(ManifestCodeFailureKind),
}

/// 成功解析的 request code 及其来源.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestCodeResolution {
    pub request_code: u64,
    pub source: String,
    pub trace: Vec<ManifestCodeTraceEntry>,
}

/// 所有可用 provider 都未命中时的诊断结果.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("manifest request code was not resolved")]
pub struct ManifestCodeUnresolved {
    pub trace: Vec<ManifestCodeTraceEntry>,
}

/// 固定执行 Lua extended, Lua basic, HTTP 的解析链.
pub struct ManifestCodeResolverChain<'a> {
    lua_extended: Option<&'a dyn ManifestCodeProvider>,
    lua_basic: Option<&'a dyn ManifestCodeProvider>,
    http: Option<&'a dyn ManifestCodeProvider>,
}

impl<'a> ManifestCodeResolverChain<'a> {
    pub fn new(
        lua_extended: Option<&'a dyn ManifestCodeProvider>,
        lua_basic: Option<&'a dyn ManifestCodeProvider>,
        http: Option<&'a dyn ManifestCodeProvider>,
    ) -> Self {
        Self {
            lua_extended,
            lua_basic,
            http,
        }
    }

    /// 按上游兼容顺序执行 provider. 缺少 app/depot 时跳过 extended.
    pub fn resolve(
        &self,
        request: ManifestCodeRequest,
    ) -> Result<ManifestCodeResolution, ManifestCodeUnresolved> {
        let mut trace = Vec::with_capacity(3);

        if request.app_id.is_some() && request.depot_id.is_some() {
            if let Some(provider) = self.lua_extended {
                if let Some(resolution) = try_provider(
                    provider,
                    ManifestCodeStage::LuaExtended,
                    request,
                    &mut trace,
                ) {
                    return Ok(resolution);
                }
            }
        }

        if let Some(provider) = self.lua_basic {
            if let Some(resolution) =
                try_provider(provider, ManifestCodeStage::LuaBasic, request, &mut trace)
            {
                return Ok(resolution);
            }
        }

        if let Some(provider) = self.http {
            if let Some(resolution) =
                try_provider(provider, ManifestCodeStage::Http, request, &mut trace)
            {
                return Ok(resolution);
            }
        }

        Err(ManifestCodeUnresolved { trace })
    }
}

fn try_provider(
    provider: &dyn ManifestCodeProvider,
    stage: ManifestCodeStage,
    request: ManifestCodeRequest,
    trace: &mut Vec<ManifestCodeTraceEntry>,
) -> Option<ManifestCodeResolution> {
    let (outcome, request_code) = match provider.resolve(request) {
        Ok(Some(request_code)) => (ManifestCodeTraceOutcome::Hit, Some(request_code)),
        Ok(None) => (ManifestCodeTraceOutcome::Miss, None),
        Err(kind) => (ManifestCodeTraceOutcome::Failed(kind), None),
    };
    trace.push(ManifestCodeTraceEntry {
        provider: provider.id().to_owned(),
        stage,
        outcome,
    });

    let request_code = request_code?;
    Some(ManifestCodeResolution {
        request_code,
        source: provider.id().to_owned(),
        trace: std::mem::take(trace),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct StubProvider {
        id: &'static str,
        result: ManifestCodeProviderResult,
        calls: Mutex<Vec<ManifestCodeRequest>>,
    }

    impl StubProvider {
        fn new(id: &'static str, result: ManifestCodeProviderResult) -> Self {
            Self {
                id,
                result,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl ManifestCodeProvider for StubProvider {
        fn id(&self) -> &str {
            self.id
        }

        fn resolve(&self, request: ManifestCodeRequest) -> ManifestCodeProviderResult {
            self.calls.lock().unwrap().push(request);
            self.result
        }
    }

    fn request() -> ManifestCodeRequest {
        ManifestCodeRequest {
            app_id: Some(10),
            depot_id: Some(20),
            manifest_gid: 30,
        }
    }

    #[test]
    fn extended_hit_stops_fallback_chain() {
        let extended = StubProvider::new("lua_ex", Ok(Some(41)));
        let basic = StubProvider::new("lua", Ok(Some(42)));
        let http = StubProvider::new("http", Ok(Some(43)));
        let chain = ManifestCodeResolverChain::new(Some(&extended), Some(&basic), Some(&http));

        let resolved = chain.resolve(request()).unwrap();

        assert_eq!(resolved.request_code, 41);
        assert_eq!(resolved.source, "lua_ex");
        assert_eq!(extended.call_count(), 1);
        assert_eq!(basic.call_count(), 0);
        assert_eq!(http.call_count(), 0);
    }

    #[test]
    fn miss_and_failure_fall_back_in_upstream_order() {
        let extended = StubProvider::new("lua_ex", Ok(None));
        let basic = StubProvider::new("lua", Err(ManifestCodeFailureKind::InvalidResponse));
        let http = StubProvider::new("http", Ok(Some(43)));
        let chain = ManifestCodeResolverChain::new(Some(&extended), Some(&basic), Some(&http));

        let resolved = chain.resolve(request()).unwrap();

        assert_eq!(resolved.request_code, 43);
        assert_eq!(
            resolved
                .trace
                .iter()
                .map(|entry| (entry.stage, entry.outcome))
                .collect::<Vec<_>>(),
            vec![
                (
                    ManifestCodeStage::LuaExtended,
                    ManifestCodeTraceOutcome::Miss
                ),
                (
                    ManifestCodeStage::LuaBasic,
                    ManifestCodeTraceOutcome::Failed(ManifestCodeFailureKind::InvalidResponse)
                ),
                (ManifestCodeStage::Http, ManifestCodeTraceOutcome::Hit),
            ]
        );
    }

    #[test]
    fn extended_is_skipped_without_app_and_depot_context() {
        let extended = StubProvider::new("lua_ex", Ok(Some(41)));
        let basic = StubProvider::new("lua", Ok(Some(42)));
        let chain = ManifestCodeResolverChain::new(Some(&extended), Some(&basic), None);
        let request = ManifestCodeRequest {
            app_id: None,
            depot_id: None,
            manifest_gid: 30,
        };

        let resolved = chain.resolve(request).unwrap();

        assert_eq!(resolved.request_code, 42);
        assert_eq!(extended.call_count(), 0);
    }

    #[test]
    fn unresolved_keeps_only_classified_trace() {
        let basic = StubProvider::new("lua", Ok(None));
        let http = StubProvider::new("http", Err(ManifestCodeFailureKind::Timeout));
        let chain = ManifestCodeResolverChain::new(None, Some(&basic), Some(&http));

        let error = chain.resolve(request()).unwrap_err();

        assert_eq!(error.to_string(), "manifest request code was not resolved");
        assert_eq!(error.trace.len(), 2);
        assert_eq!(
            error.trace[1].outcome,
            ManifestCodeTraceOutcome::Failed(ManifestCodeFailureKind::Timeout)
        );
    }
}
