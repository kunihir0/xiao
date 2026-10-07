use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("websocket transport failed")]
    Transport,
    #[error("protobuf decode failed")]
    Decode,
    #[error("protobuf encode failed")]
    Encode,
    #[error("I/O failed")]
    Io,
    #[error("server rejected request")]
    Api,
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    #[error("client is disconnected")]
    Disconnected,
    #[error("client is shutting down")]
    ShuttingDown,
    #[error("outbound queue is full")]
    QueueFull,
    #[error("invalid camera payload: {0}")]
    InvalidCamera(&'static str),
    #[error("invalid configuration: {0}")]
    InvalidConfig(&'static str),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
