use std::num::TryFromIntError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid protocol state: {0}")]
    InvalidState(&'static str),
    #[error("protocol failure: {0}")]
    Protocol(String),
    #[error("operation timed out: {0}")]
    Timeout(&'static str),
    #[error("heartbeat liveness deadline exceeded")]
    LivenessTimeout,
    #[error("transport closed")]
    TransportClosed,
    #[error("TLS transport failed: {0}")]
    Tls(String),
    #[error("HTTP transport failed: {0}")]
    Http(String),
    #[error("frame version {0} is not supported")]
    UnsupportedVersion(u8),
    #[error("receiver queue byte budget exceeded")]
    QueueByteBudget,
    #[error("notification exceeds byte budget")]
    NotificationTooLarge,
    #[error("frame tag {0} is not supported")]
    UnsupportedTag(u8),
    #[error("frame payload is too large ({actual} bytes, max {max})")]
    FrameTooLarge { actual: usize, max: usize },
    #[error("inbound MCS stream counter exhausted")]
    StreamIdExhausted,
    #[error("truncated frame")]
    TruncatedFrame,
    #[error("invalid or overlong varint")]
    InvalidVarint,
    #[error("protobuf decode failed: {0}")]
    Decode(String),
    #[error("protobuf encode failed: {0}")]
    Encode(String),
    #[error("invalid registration response: {0}")]
    InvalidRegistration(String),
    #[error("missing check-in credential: {0}")]
    MissingCredential(&'static str),
    #[error("invalid check-in credential: {0}")]
    InvalidCredential(&'static str),
    #[error("base64 decoding failed: {0}")]
    InvalidBase64(String),
    #[error("invalid cryptographic key: {0}")]
    InvalidKey(String),
    #[error("web push authentication failed")]
    DecryptionFailed,
    #[error("invalid Web Push padding")]
    InvalidPadding,
    #[error("I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("receiver channel is closed")]
    ChannelClosed,
    #[error("receiver is already stopped")]
    Stopped,
    #[error("receiver task failed: {0}")]
    Task(String),
    #[error("invalid numeric value")]
    Numeric(#[from] TryFromIntError),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
