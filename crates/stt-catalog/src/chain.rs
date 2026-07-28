//! Catalog provider chain 与脱敏诊断.

use stt_core::{AppId, CatalogBundle};

use crate::{
    CatalogError, CatalogFetchOutcome, CatalogProvider, CatalogResult, CatalogTraceEntry,
    CatalogTraceOutcome, ProviderErrorKind,
};

/// 按配置顺序执行的 Catalog provider chain.
#[derive(Default)]
pub struct CatalogProviderChain {
    providers: Vec<Box<dyn CatalogProvider>>,
}

impl CatalogProviderChain {
    pub fn new(providers: Vec<Box<dyn CatalogProvider>>) -> Self {
        Self { providers }
    }

    pub fn push(&mut self, provider: Box<dyn CatalogProvider>) {
        self.providers.push(provider);
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

impl CatalogProvider for CatalogProviderChain {
    fn id(&self) -> &str {
        "chain"
    }

    fn fetch(&self, app_id: AppId) -> CatalogResult<CatalogBundle> {
        self.fetch_with_trace(app_id).map(|outcome| outcome.bundle)
    }

    fn fetch_with_trace(&self, app_id: AppId) -> CatalogResult<CatalogFetchOutcome> {
        let mut trace = Vec::with_capacity(self.providers.len());
        for provider in &self.providers {
            match provider.fetch_with_trace(app_id) {
                Ok(mut outcome) => {
                    trace.append(&mut outcome.trace);
                    outcome.trace = trace;
                    return Ok(outcome);
                }
                Err(CatalogError::ChainExhausted { trace: mut nested }) => {
                    trace.append(&mut nested);
                }
                Err(error) => trace.push(CatalogTraceEntry {
                    provider: provider.id().to_owned(),
                    outcome: CatalogTraceOutcome::Failed(classify_error(&error)),
                }),
            }
        }
        Err(CatalogError::ChainExhausted { trace })
    }
}

pub(crate) fn classify_error(error: &CatalogError) -> ProviderErrorKind {
    match error {
        CatalogError::Provider { kind, .. } => *kind,
        CatalogError::RequestedAppMissing(_) => ProviderErrorKind::NotFound,
        CatalogError::ChainExhausted { .. }
        | CatalogError::PayloadTooLarge { .. }
        | CatalogError::Json(_)
        | CatalogError::UnsupportedSchema { .. }
        | CatalogError::EmptyBundle
        | CatalogError::ZeroAppId
        | CatalogError::ZeroDepotId
        | CatalogError::DuplicateApp(_)
        | CatalogError::DuplicateDepot { .. }
        | CatalogError::UndeclaredApp(_)
        | CatalogError::UndeclaredDepot(_)
        | CatalogError::ConflictingDepot(_)
        | CatalogError::InvalidDepotKey { .. }
        | CatalogError::InvalidDecimalU64 { .. }
        | CatalogError::TooManyEntries { .. } => ProviderErrorKind::Rejected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MockCatalogProvider;

    struct Unavailable;

    impl CatalogProvider for Unavailable {
        fn id(&self) -> &str {
            "unavailable"
        }

        fn fetch(&self, _app_id: AppId) -> CatalogResult<CatalogBundle> {
            Err(CatalogError::Provider {
                provider: self.id().to_owned(),
                kind: ProviderErrorKind::Timeout,
                detail: "classified detail".to_owned(),
            })
        }
    }

    #[test]
    fn chain_records_failures_and_final_source() {
        let chain = CatalogProviderChain::new(vec![
            Box::new(Unavailable),
            Box::new(MockCatalogProvider::new().with_simple_app(42, 43, &"ab".repeat(32), 44)),
        ]);

        let outcome = chain.fetch_with_trace(42).unwrap();

        assert_eq!(outcome.source, "mock");
        assert_eq!(outcome.trace.len(), 2);
        assert_eq!(
            outcome.trace[0].outcome,
            CatalogTraceOutcome::Failed(ProviderErrorKind::Timeout)
        );
        assert_eq!(outcome.trace[1].outcome, CatalogTraceOutcome::Hit);
    }

    #[test]
    fn exhausted_error_does_not_keep_provider_detail() {
        let chain = CatalogProviderChain::new(vec![Box::new(Unavailable)]);

        let error = chain.fetch(42).unwrap_err();

        let CatalogError::ChainExhausted { trace } = error else {
            panic!("unexpected error")
        };
        assert_eq!(trace[0].provider, "unavailable");
        assert_eq!(
            trace[0].outcome,
            CatalogTraceOutcome::Failed(ProviderErrorKind::Timeout)
        );
    }
}
