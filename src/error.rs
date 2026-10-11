use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// Errors intentionally omit response bodies, URLs and credentials.
#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid configuration: {0}")]
    Config(&'static str),
    #[error("authentication failed or expired")]
    Auth,
    #[error("provider returned HTTP {0}")]
    Http(u16),
    #[error("Paperless API redirected (HTTP {status}): {guidance}")]
    PaperlessRedirect { status: u16, guidance: &'static str },
    #[error("network request failed or timed out")]
    Network,
    #[error("invalid provider response: {0}")]
    InvalidResponse(&'static str),
    #[error("local filesystem operation failed")]
    Io(#[from] std::io::Error),
    #[error("document store integrity error: {0}")]
    Storage(&'static str),
}

impl Error {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Auth | Self::Http(401 | 403) => 3,
            Self::Storage(_) => 4,
            _ => 1,
        }
    }
}
