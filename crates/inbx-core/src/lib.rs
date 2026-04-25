#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config: {0}")]
    Config(String),
    #[error("store: {0}")]
    Store(String),
    #[error("net: {0}")]
    Net(String),
}

pub type Result<T> = std::result::Result<T, Error>;
