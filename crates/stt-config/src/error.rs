use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(transparent)]
    Catalog(#[from] stt_catalog::CatalogError),
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("lua error: {0}")]
    Lua(String),
    #[error("invalid value: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, ConfigError>;
