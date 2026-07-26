//! Pattern 元数据: FNV 键, TOML 子集, 特征码扫描.

mod error;
mod fnv;
mod pattern;
mod store;

pub use error::{MetadataError, Result};
pub use fnv::{fnv1a32, fnv1a32_str};
pub use pattern::{
    resolve_in_image, resolve_rva, ByteSig, PatternEntry, PatternMap,
};
pub use store::{PathBufLoad, PatternStore};
