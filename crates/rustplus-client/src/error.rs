use std::time::Duration;

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum Error {
    #[error("websocket transport failed")]
    Transport,
    #[error("protobuf decode failed")]
    Decode,
    #[error("protobuf encode failed")]
    Encode,
    #[error("I/O failed")]
    Io,
    #[error("server rejected request: {0}")]
    Api(String),
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    #[error("client is disconnected")]
    Disconnected,
    #[error("client is shutting down")]
    ShuttingDown,
    #[error("outbound queue is full")]
    QueueFull,
    #[error("too many pending requests")]
    PendingFull,
    #[error("request sequence space exhausted")]
    SequenceExhausted,
    #[error("connection endpoint must use wss://")]
    InsecureEndpoint,
    #[error("invalid endpoint")]
    InvalidEndpoint,
    #[error("invalid configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("shutdown timed out after {0:?}")]
    ShutdownTimeout(Duration),
    #[error("invalid camera payload: {0}")]
    InvalidCamera(&'static str),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
