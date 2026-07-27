//! Catalog 失败分类.

use stt_core::{AppId, DepotId};

/// 单次 Catalog provider 尝试的脱敏结果.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogTraceEntry {
    pub provider: String,
    pub outcome: CatalogTraceOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogTraceOutcome {
    Hit,
    Failed(ProviderErrorKind),
}

/// provider 执行失败的稳定分类.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// 配置或依赖不可用.
    Unavailable,
    /// 请求超过允许时间.
    Timeout,
    /// provider 没有该条目.
    NotFound,
    /// provider 拒绝请求或返回非成功状态.
    Rejected,
}

/// Catalog 获取或校验错误.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// provider 自身失败, 尚未产生可信 bundle.
    #[error("catalog provider {provider} failed ({kind:?}): {detail}")]
    Provider {
        /// provider 稳定标识.
        provider: String,
        /// 失败分类.
        kind: ProviderErrorKind,
        /// 不包含 key/token 正文的诊断.
        detail: String,
    },
    /// provider chain 已全部失败, 只保留脱敏分类.
    #[error("catalog provider chain exhausted")]
    ChainExhausted { trace: Vec<CatalogTraceEntry> },
    /// wire body 超过协议层上限.
    #[error("catalog payload too large: {actual} bytes, limit {limit}")]
    PayloadTooLarge {
        /// 实际字节数.
        actual: usize,
        /// 最大允许字节数.
        limit: usize,
    },
    /// JSON 不是合法的 v1 wire 文档.
    #[error("invalid catalog JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// schema 版本不是当前支持的版本.
    #[error("unsupported catalog schema version {found}, expected {expected}")]
    UnsupportedSchema {
        /// wire 中的版本.
        found: u32,
        /// 当前版本.
        expected: u32,
    },
    /// wire 或 bundle 为空.
    #[error("catalog bundle has no apps")]
    EmptyBundle,
    /// 请求的 AppId 不在结果中.
    #[error("catalog bundle does not contain requested app {0}")]
    RequestedAppMissing(AppId),
    /// AppId 为零.
    #[error("catalog contains zero app id")]
    ZeroAppId,
    /// DepotId 为零.
    #[error("catalog contains zero depot id")]
    ZeroDepotId,
    /// 列表中出现重复 AppId.
    #[error("catalog contains duplicate app {0}")]
    DuplicateApp(AppId),
    /// 同一 app 下出现重复 DepotId.
    #[error("catalog app {app_id} contains duplicate depot {depot_id}")]
    DuplicateDepot {
        /// 所属 AppId.
        app_id: AppId,
        /// 重复 DepotId.
        depot_id: DepotId,
    },
    /// metadata 使用了未声明的 app.
    #[error("catalog metadata references undeclared app {0}")]
    UndeclaredApp(AppId),
    /// metadata 使用了未声明的 depot.
    #[error("catalog metadata references undeclared depot {0}")]
    UndeclaredDepot(DepotId),
    /// 同一个 depot 收到了互相冲突的数据.
    #[error("catalog has conflicting metadata for depot {0}")]
    ConflictingDepot(DepotId),
    /// depot key 不是 64 位 hex.
    #[error("catalog depot {depot_id} has invalid key")]
    InvalidDepotKey {
        /// 出错的 DepotId.
        depot_id: DepotId,
    },
    /// 十进制 u64 字符串无效或为零.
    #[error("catalog field {field} has invalid decimal u64")]
    InvalidDecimalU64 {
        /// 字段路径, 不包含原始敏感值.
        field: String,
    },
    /// 条目数量超过契约上限.
    #[error("catalog {field} count {actual} exceeds limit {limit}")]
    TooManyEntries {
        /// 超限字段.
        field: &'static str,
        /// 实际数量.
        actual: usize,
        /// 最大数量.
        limit: usize,
    },
}

/// Catalog crate 的统一结果.
pub type CatalogResult<T> = std::result::Result<T, CatalogError>;
