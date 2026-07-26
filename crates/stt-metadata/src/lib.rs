//! Pattern metadata: FNV keys, TOML subset, signature scan.

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
