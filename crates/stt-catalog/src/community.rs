//! Community Catalog adapter 占位.

use stt_core::{AppId, CatalogBundle};

use crate::{CatalogError, CatalogProvider, CatalogResult, ProviderErrorKind};

/// 社区 Catalog adapter. 当前没有可用协议实现.
#[derive(Debug, Clone, Copy, Default)]
pub struct CommunityCatalogProvider;

impl CatalogProvider for CommunityCatalogProvider {
    fn id(&self) -> &str {
        "community"
    }

    fn fetch(&self, _app_id: AppId) -> CatalogResult<CatalogBundle> {
        Err(CatalogError::Provider {
            provider: self.id().to_owned(),
            kind: ProviderErrorKind::Unavailable,
            detail: "community catalog adapter is not implemented".to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn community_adapter_is_explicitly_unavailable() {
        let error = CommunityCatalogProvider.fetch(42).unwrap_err();

        assert!(matches!(
            error,
            CatalogError::Provider {
                kind: ProviderErrorKind::Unavailable,
                ..
            }
        ));
    }
}
