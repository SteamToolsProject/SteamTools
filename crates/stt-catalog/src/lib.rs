//! Catalog provider 契约, 校验和版本化 wire schema.
//!
//! 该 crate 不执行 HTTP, 也不负责 Lua/TOML 落盘. provider 返回的数据必须先通过
//! [`validate_bundle`], 才能交给配置层持久化.

mod error;
mod http;
mod mock;
mod validate;
mod wire;

pub use error::{CatalogError, CatalogResult, ProviderErrorKind};
pub use http::{validate_url_template, CustomHttpCatalogProvider};
pub use mock::MockCatalogProvider;
pub use validate::{validate_bundle, CatalogLimits};
pub use wire::{
    parse_catalog_wire_v1, CatalogAppV1, CatalogDepotV1, CatalogManifestV1, CatalogWireV1,
};

use stt_core::{AppId, CatalogBundle};

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
}
