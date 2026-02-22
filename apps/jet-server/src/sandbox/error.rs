use thiserror::Error;

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("Hakoniwa error: {0}")]
    Hakoniwa(#[from] hakoniwa::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Execution failed: {0}")]
    ExecutionFailed(String),
}

pub type SandboxResult<T> = Result<T, SandboxError>;
