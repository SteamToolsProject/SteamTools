use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid metadata: {0}")]
    Invalid(String),
    #[error("pattern set unavailable for module")]
    ModuleDisabled,
}

pub type Result<T> = std::result::Result<T, MetadataError>;
