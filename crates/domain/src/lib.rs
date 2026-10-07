//! Business concepts and invariants.

mod credentials;
mod ids;
mod pairing;

use std::fmt;

pub use credentials::{PlayerCredentials, PlayerToken};
pub use ids::{DeliveryId, EntityId, PlayerId, ServerAddress, ServerId, SessionId, SteamId};
pub use pairing::{EntityPairing, MAX_ENTITY_KIND_BYTES, MAX_ENTITY_NAME_BYTES, Pairing};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DomainError {
    Invalid { field: &'static str },
}

impl fmt::Display for DomainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { field } => write!(formatter, "invalid {field}"),
        }
    }
}

impl std::error::Error for DomainError {}

#[cfg(test)]
mod tests {
    use super::{DeliveryId, DomainError, PlayerId, PlayerToken, ServerAddress, ServerId};

    #[test]
    fn identifiers_validate_nonzero_values() {
        assert!(matches!(
            PlayerId::new(0),
            Err(DomainError::Invalid { field: "player_id" })
        ));
        assert!(matches!(
            super::EntityId::new(0),
            Err(DomainError::Invalid { field: "entity_id" })
        ));
        assert!(matches!(
            ServerAddress::new("host", 0),
            Err(DomainError::Invalid { field: "port" })
        ));
        assert!(matches!(
            DeliveryId::new("  "),
            Err(DomainError::Invalid {
                field: "delivery_id"
            })
        ));
    }

    #[test]
    fn server_identity_canonicalizes_ipv6() -> Result<(), DomainError> {
        let address = ServerAddress::new("::1", 28082)?;
        assert_eq!(ServerId::from_address(&address).as_str(), "[::1]:28082");
        Ok(())
    }

    #[test]
    fn credentials_and_pairing_redact_token() -> Result<(), DomainError> {
        let token = PlayerToken::new(123_456_789);
        assert_eq!(format!("{token:?}"), "[REDACTED]");
        let credentials = super::PlayerCredentials::new(PlayerId::new(1)?, token);
        assert!(!format!("{credentials:?}").contains("123456789"));
        let pairing =
            super::Pairing::new(ServerAddress::new("host", 28082)?, credentials, None, None);
        assert!(!format!("{pairing:?}").contains("123456789"));
        Ok(())
    }

    #[test]
    fn entity_metadata_has_bounded_bytes_and_rejects_controls() -> Result<(), DomainError> {
        let id = super::EntityId::new(1)?;
        assert!(
            super::EntityPairing::new(
                id,
                Some("k".repeat(super::MAX_ENTITY_KIND_BYTES)),
                Some("n".repeat(super::MAX_ENTITY_NAME_BYTES)),
            )
            .is_ok()
        );
        assert!(matches!(
            super::EntityPairing::new(id, Some("k".repeat(super::MAX_ENTITY_KIND_BYTES + 1)), None,),
            Err(DomainError::Invalid {
                field: "entity_type"
            })
        ));
        assert!(matches!(
            super::EntityPairing::new(id, None, Some("n".repeat(super::MAX_ENTITY_NAME_BYTES + 1)),),
            Err(DomainError::Invalid {
                field: "entity_name"
            })
        ));
        assert!(matches!(
            super::EntityPairing::new(id, Some("kind\n".to_owned()), None),
            Err(DomainError::Invalid {
                field: "entity_type"
            })
        ));
        assert!(matches!(
            super::EntityPairing::new(id, None, Some("name\u{7f}".to_owned())),
            Err(DomainError::Invalid {
                field: "entity_name"
            })
        ));
        Ok(())
    }
}
