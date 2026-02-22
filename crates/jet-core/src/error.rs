use std::path::PathBuf;

use thiserror::Error;

pub type JetResult<T> = Result<T, JetError>;

#[derive(Debug, Error)]
pub enum JetError {
    #[error("configuration error: {message}")]
    Config { message: String },

    #[error("failed to parse config file {path:?}: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("I/O error on {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("sandbox error: {message}")]
    Sandbox { message: String },
}

impl From<toml::ser::Error> for JetError {
    fn from(value: toml::ser::Error) -> Self {
        Self::Serialization(value.to_string())
    }
}

impl From<toml::de::Error> for JetError {
    fn from(value: toml::de::Error) -> Self {
        Self::Serialization(value.to_string())
    }
}

impl JetError {
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config {
            message: message.into(),
        }
    }

    pub fn sandbox(message: impl Into<String>) -> Self {
        Self::Sandbox {
            message: message.into(),
        }
    }
}
