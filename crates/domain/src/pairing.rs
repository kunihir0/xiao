use std::fmt;

use crate::{DeliveryId, DomainError, EntityId, PlayerCredentials, ServerAddress, ServerId};

#[derive(Clone, Eq, PartialEq)]
pub struct EntityPairing {
    id: EntityId,
    kind: Option<String>,
    name: Option<String>,
}

impl EntityPairing {
    pub fn new(
        id: EntityId,
        kind: Option<String>,
        name: Option<String>,
    ) -> Result<Self, DomainError> {
        if kind.as_deref().is_some_and(|value| value.trim().is_empty()) {
            return Err(DomainError::Invalid {
                field: "entity_type",
            });
        }
        if name.as_deref().is_some_and(|value| value.trim().is_empty()) {
            return Err(DomainError::Invalid {
                field: "entity_name",
            });
        }
        Ok(Self { id, kind, name })
    }

    pub const fn id(&self) -> EntityId {
        self.id
    }

    pub fn kind(&self) -> Option<&str> {
        self.kind.as_deref()
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

impl fmt::Debug for EntityPairing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EntityPairing")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("name", &self.name)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct Pairing {
    server_id: ServerId,
    address: ServerAddress,
    credentials: PlayerCredentials,
    entity: Option<EntityPairing>,
    delivery_id: Option<DeliveryId>,
}

impl Pairing {
    pub fn new(
        address: ServerAddress,
        credentials: PlayerCredentials,
        entity: Option<EntityPairing>,
        delivery_id: Option<DeliveryId>,
    ) -> Self {
        let server_id = ServerId::from_address(&address);
        Self {
            server_id,
            address,
            credentials,
            entity,
            delivery_id,
        }
    }

    pub fn same_server(&self, other: &Self) -> bool {
        self.server_id == other.server_id
    }

    pub fn same_credentials(&self, other: &Self) -> bool {
        self.credentials == other.credentials
    }

    pub fn server_id(&self) -> &ServerId {
        &self.server_id
    }

    pub fn address(&self) -> &ServerAddress {
        &self.address
    }

    pub fn credentials(&self) -> &PlayerCredentials {
        &self.credentials
    }

    pub fn entity(&self) -> Option<&EntityPairing> {
        self.entity.as_ref()
    }

    pub fn delivery_id(&self) -> Option<&DeliveryId> {
        self.delivery_id.as_ref()
    }
}

impl fmt::Debug for Pairing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Pairing")
            .field("server_id", &self.server_id)
            .field("address", &self.address)
            .field("credentials", &"[REDACTED]")
            .field("entity", &self.entity)
            .field("delivery_id", &"[REDACTED]")
            .finish()
    }
}
