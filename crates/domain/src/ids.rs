use std::fmt;

use crate::DomainError;

/// A validated Rust+ server endpoint identity.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ServerId(String);

impl ServerId {
    pub fn from_address(address: &ServerAddress) -> Self {
        let host = address.host();
        let value = if host.contains(':') {
            format!("[{host}]:{}", address.port())
        } else {
            format!("{host}:{}", address.port())
        };
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ServerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("ServerId").field(&self.0).finish()
    }
}

impl fmt::Display for ServerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A host and TCP port used to address a Rust+ server.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct ServerAddress {
    host: String,
    port: u16,
}

impl ServerAddress {
    pub fn new(host: impl Into<String>, port: u16) -> Result<Self, DomainError> {
        let host = host.into();
        let host = host.trim().to_owned();
        if host.is_empty() {
            return Err(DomainError::Invalid { field: "host" });
        }
        if port == 0 {
            return Err(DomainError::Invalid { field: "port" });
        }
        Ok(Self { host, port })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub const fn port(&self) -> u16 {
        self.port
    }
}

impl fmt::Debug for ServerAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerAddress")
            .field("host", &self.host)
            .field("port", &self.port)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PlayerId(u64);

impl PlayerId {
    pub fn new(value: u64) -> Result<Self, DomainError> {
        (value != 0)
            .then_some(Self(value))
            .ok_or(DomainError::Invalid { field: "player_id" })
    }

    pub const fn value(self) -> u64 {
        self.0
    }
}

/// A Steam account identity bound to a Rust+ companion registration.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SteamId(u64);

impl SteamId {
    pub fn new(value: u64) -> Result<Self, DomainError> {
        (value != 0)
            .then_some(Self(value))
            .ok_or(DomainError::Invalid { field: "steam_id" })
    }

    pub const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct EntityId(u32);

impl EntityId {
    pub fn new(value: u32) -> Result<Self, DomainError> {
        (value != 0)
            .then_some(Self(value))
            .ok_or(DomainError::Invalid { field: "entity_id" })
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SessionId(u64);

impl SessionId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct DeliveryId(String);

impl DeliveryId {
    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(DomainError::Invalid {
                field: "delivery_id",
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for DeliveryId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("DeliveryId")
            .field(&"[REDACTED]")
            .finish()
    }
}

impl fmt::Display for DeliveryId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}
