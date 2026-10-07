use std::time::Duration;

use bot_domain::{Pairing, ServerId};
use tokio::sync::watch;

use crate::BoxFuture;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceReadiness {
    Starting,
    Ready,
    Stopped,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionReadiness {
    Connecting,
    Ready,
    Reconnecting,
    Closing,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceReadiness {
    Starting,
    Ready,
    Stopping,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEvent {
    Ready,
    Reconnecting,
    Disconnected,
    Terminated { error: Option<SessionError> },
    Lagged { skipped: u64 },
}

pub trait PairingSource: Send {
    fn readiness(&self) -> SourceReadiness;
    fn readiness_watch(&self) -> watch::Receiver<SourceReadiness>;
    fn next<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<Pairing>, PairingSourceError>>;
    fn shutdown<'a>(&'a mut self) -> BoxFuture<'a, Result<(), PairingSourceError>>;
}

pub trait SessionConnector: Send + Sync {
    fn connect<'a>(
        &'a self,
        pairing: &'a Pairing,
    ) -> BoxFuture<'a, Result<Box<dyn SessionHandle>, SessionError>>;
}

pub trait SessionHandle: Send {
    fn readiness(&self) -> SessionReadiness;
    fn next_event<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<SessionEvent>, SessionError>>;
    fn shutdown<'a>(&'a mut self) -> BoxFuture<'a, Result<(), SessionError>>;
}

pub trait PairingStore: Send + Sync {
    fn load<'a>(&'a self) -> BoxFuture<'a, Result<Vec<Pairing>, StoreError>>;
    fn upsert<'a>(&'a self, pairing: &'a Pairing) -> BoxFuture<'a, Result<(), StoreError>>;
    fn remove<'a>(&'a self, server_id: &'a ServerId) -> BoxFuture<'a, Result<(), StoreError>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PairingSourceError {
    Receiver(String),
    Parse(String),
    Failed,
    Closed,
    Shutdown(String),
}

impl std::fmt::Display for PairingSourceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Receiver(message) => write!(formatter, "pairing receiver failure: {message}"),
            Self::Parse(message) => write!(formatter, "pairing parse failure: {message}"),
            Self::Failed => formatter.write_str("pairing source failed"),
            Self::Closed => formatter.write_str("pairing source closed"),
            Self::Shutdown(message) => {
                write!(formatter, "pairing source shutdown failure: {message}")
            }
        }
    }
}
impl std::error::Error for PairingSourceError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreError {
    Unavailable,
    Invalid(String),
    SecretStorageUnavailable,
}
impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("pairing store unavailable"),
            Self::Invalid(message) => {
                write!(formatter, "invalid pairing store operation: {message}")
            }
            Self::SecretStorageUnavailable => formatter.write_str("secret storage unavailable"),
        }
    }
}
impl std::error::Error for StoreError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionError {
    InvalidCredentials,
    ConnectTimeout(Duration),
    Transport,
    Protocol,
    Disconnected,
    EventLagged { skipped: u64 },
    ShutdownTimeout(Duration),
    ShuttingDown,
    Failed,
}
impl std::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCredentials => formatter.write_str("invalid credentials"),
            Self::ConnectTimeout(duration) => {
                write!(formatter, "connection timed out after {duration:?}")
            }
            Self::Transport => formatter.write_str("transport failure"),
            Self::Protocol => formatter.write_str("protocol failure"),
            Self::Disconnected => formatter.write_str("session disconnected"),
            Self::EventLagged { skipped } => {
                write!(formatter, "session event stream lagged by {skipped}")
            }
            Self::ShutdownTimeout(duration) => {
                write!(formatter, "session shutdown timed out after {duration:?}")
            }
            Self::ShuttingDown => formatter.write_str("session is shutting down"),
            Self::Failed => formatter.write_str("session failed"),
        }
    }
}
impl std::error::Error for SessionError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceError {
    Source(PairingSourceError),
    Session {
        server: ServerId,
        source: SessionError,
    },
    Store(StoreError),
    SourceClosed,
    ShutdownTimeout(Duration),
}
impl std::fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Source(error) => write!(formatter, "pairing source failure: {error}"),
            Self::Session { server, source } => {
                write!(formatter, "session {server} failed: {source}")
            }
            Self::Store(error) => write!(formatter, "pairing store failure: {error}"),
            Self::SourceClosed => formatter.write_str("pairing source closed unexpectedly"),
            Self::ShutdownTimeout(duration) => {
                write!(formatter, "service shutdown timed out after {duration:?}")
            }
        }
    }
}
impl std::error::Error for ServiceError {}
