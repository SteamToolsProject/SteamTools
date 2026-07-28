//! Catalog provider 契约, 校验和版本化 wire schema.
//!
//! 该 crate 不执行 HTTP, 也不负责 Lua/TOML 落盘. provider 返回的数据必须先通过
//! [`validate_bundle`], 才能交给配置层持久化.

mod caigamer;
mod chain;
mod community;
mod error;
mod http;
mod mock;
mod snapshot;
mod validate;
mod wire;

pub use caigamer::CaigamerCatalogProvider;
pub use chain::CatalogProviderChain;
pub use community::CommunityCatalogProvider;
pub use error::{
    CatalogError, CatalogResult, CatalogTraceEntry, CatalogTraceOutcome, ProviderErrorKind,
};
pub use http::{validate_url_template, CustomHttpCatalogProvider};
pub use mock::MockCatalogProvider;
pub use snapshot::{ensure_community_snapshots, CommunitySnapshotReport, CommunitySnapshotState};
pub use validate::{validate_bundle, CatalogLimits};
pub use wire::{
    parse_catalog_wire_v1, CatalogAppV1, CatalogDepotV1, CatalogManifestV1, CatalogWireV1,
};

use stt_core::{AppId, CatalogBundle};

#[cfg(test)]
pub(crate) fn http_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Catalog 获取结果及 provider chain 诊断.
#[derive(Debug, Clone)]
pub struct CatalogFetchOutcome {
    pub bundle: CatalogBundle,
    pub source: String,
    pub trace: Vec<CatalogTraceEntry>,
}

/// 按 AppId 获取完整入库元数据的运行时 provider.
///
/// Manifest request code 使用另一套 resolver, 不实现这个 trait.
pub trait CatalogProvider: Send + Sync {
    /// 稳定的 provider 标识, 用于配置, 日志和错误归因.
    fn id(&self) -> &str;

    /// 获取并校验包含 `app_id` 的目录结果.
    ///
    /// # Errors
    ///
    /// provider 不可用, 条目不存在或数据未通过契约校验时返回 [`CatalogError`].
    fn fetch(&self, app_id: AppId) -> CatalogResult<CatalogBundle>;

    /// 获取 Catalog 并报告最终来源. 单 provider 默认生成一条命中记录.
    fn fetch_with_trace(&self, app_id: AppId) -> CatalogResult<CatalogFetchOutcome> {
        Ok(CatalogFetchOutcome {
            bundle: self.fetch(app_id)?,
            source: self.id().to_owned(),
            trace: vec![CatalogTraceEntry {
                provider: self.id().to_owned(),
                outcome: CatalogTraceOutcome::Hit,
            }],
        })
    }
}
