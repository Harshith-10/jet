use std::path::PathBuf;

use thiserror::Error;

pub type JetPackResult<T> = Result<T, JetPackError>;

#[derive(Debug, Error)]
pub enum JetPackError {
    #[error("io error on {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("manifest parse failed for {path:?}: {source}")]
    ManifestParse {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },

    #[error("unsupported archive type for {path:?}")]
    UnsupportedArchive { path: PathBuf },

    #[error("http download failed for {url}: {source}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("manifest for language={language} version={version} has no archive for arch={arch}")]
    MissingArchive {
        language: String,
        version: String,
        arch: String,
    },

    #[error("invalid version in manifest: {value}")]
    InvalidVersion { value: String },

    #[error("serialization error: {message}")]
    Serialization { message: String },
}
