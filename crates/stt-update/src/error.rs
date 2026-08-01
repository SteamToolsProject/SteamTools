//! 自更新的错误类型.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("http error on {url}: {source}")]
    Http {
        url: String,
        #[source]
        source: stt_platform::HttpError,
    },
    #[error("unexpected HTTP status {status} on {url}")]
    BadStatus { url: String, status: u16 },
    #[error("latest redirect has no tag: {0}")]
    InvalidTag(String),
    #[error("release tag missing from Location header")]
    MissingTag,
    #[error("no checksums.sha256 in release")]
    NoChecksums,
    #[error("checksums.sha256 has no entry for {0}")]
    MissingEntry(String),
    #[error("checksum mismatch for {name}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        name: String,
        expected: String,
        actual: String,
    },
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
