//! Catalog 补全器: 在已有 bundle 上只补缺 key / token / manifest 字节.
//!
//! 与 [`crate::CatalogProvider`] 完整源区分: enricher 失败只记 trace, 不否决入库.
//! 私有加密源应实现本 trait (或同时实现 CatalogProvider), 解密逻辑留在源模块内.

use stt_core::{AppId, CatalogBundle};

use crate::{CatalogTraceEntry, ManifestBlob};

/// 一次补全调用的可变上下文 (编排器持有, enricher 只补缺).
pub struct EnrichContext<'a> {
    pub app_id: AppId,
    pub bundle: &'a mut CatalogBundle,
    pub manifest_blobs: &'a mut Vec<ManifestBlob>,
    pub trace: &'a mut Vec<CatalogTraceEntry>,
}

/// 在已有 CatalogBundle 上尽力补全下载数据.
///
/// 约定:
/// - 不覆盖已有非空 depot key (与现 CaiGamer 补全一致)
/// - 失败只写 [`CatalogTraceEntry`], 不返回 Err 打断整链
/// - `id()` 用于 trace, 应保持稳定字符串
pub trait CatalogEnricher: Send + Sync {
    fn id(&self) -> &str;
    fn enrich(&self, ctx: &mut EnrichContext<'_>);
}
