use std::net::IpAddr;

use bot_domain::{
    DeliveryId, EntityId, EntityPairing, Pairing, PlayerCredentials, PlayerId, PlayerToken,
    ServerAddress,
};
use serde::Deserialize;

use crate::ParseError;

#[derive(Clone, Debug)]
pub struct ParseConfig {
    pub max_value_bytes: usize,
    pub max_app_data_items: usize,
}
impl Default for ParseConfig {
    fn default() -> Self {
        Self {
            max_value_bytes: push_receiver::MAX_NOTIFICATION_BYTES,
            max_app_data_items: 256,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum U64Like {
    Number(u64),
    String(String),
}
#[derive(Deserialize)]
#[serde(untagged)]
enum I64Like {
    Number(i64),
    String(String),
}

#[derive(Deserialize)]
struct RawPairing {
    ip: Option<String>,
    port: Option<U64Like>,
    #[serde(rename = "playerId")]
    player_id: Option<U64Like>,
    #[serde(rename = "playerToken")]
    player_token: Option<I64Like>,
    #[serde(rename = "entityId")]
    entity_id: Option<U64Like>,
    #[serde(rename = "entityType")]
    entity_type: Option<String>,
    #[serde(rename = "entityName")]
    entity_name: Option<String>,
    #[serde(rename = "type")]
    pairing_type: Option<String>,
}
pub(crate) fn parse_notification(
    notification: &push_receiver::Notification,
    config: &ParseConfig,
) -> Result<Option<Pairing>, ParseError> {
    let delivery_id = notification
        .persistent_id
        .as_deref()
        .map(|value| {
            DeliveryId::new(value.to_owned()).map_err(|_| ParseError::Invalid("persistent_id"))
        })
        .transpose()?;
    for data in notification.app_data.iter().take(config.max_app_data_items) {
        let value = data.value.trim();
        if value.len() > config.max_value_bytes {
            return Err(ParseError::TooLarge);
        }
        if !value.starts_with('{') {
            continue;
        }
        let raw: RawPairing = serde_json::from_str(value).map_err(|_| ParseError::Malformed)?;
        if raw.ip.is_none()
            && raw.port.is_none()
            && raw.player_id.is_none()
            && raw.player_token.is_none()
        {
            continue;
        }
        let host = raw.ip.ok_or(ParseError::Missing("ip"))?;
        let port = parse_u16(raw.port.ok_or(ParseError::Missing("port"))?, "port")?;
        let player_id = parse_u64(
            raw.player_id.ok_or(ParseError::Missing("playerId"))?,
            "playerId",
        )?;
        let token = parse_i64(
            raw.player_token.ok_or(ParseError::Missing("playerToken"))?,
            "playerToken",
        )?;
        let address = validate_address(host, port)?;
        let player_id = PlayerId::new(player_id).map_err(|_| ParseError::Domain("playerId"))?;
        let credentials = PlayerCredentials::new(
            player_id,
            PlayerToken::new(i32::try_from(token).map_err(|_| ParseError::Numeric("playerToken"))?),
        );
        let entity = match (raw.entity_id, raw.pairing_type.as_deref()) {
            (Some(id), Some("entity")) => {
                let id = EntityId::new(parse_u32(id, "entityId")?)
                    .map_err(|_| ParseError::Domain("entityId"))?;
                Some(
                    EntityPairing::new(id, raw.entity_type, raw.entity_name)
                        .map_err(|_| ParseError::Domain("entity"))?,
                )
            }
            (Some(_), _) => return Err(ParseError::Invalid("type")),
            (None, Some("entity")) => return Err(ParseError::Missing("entityId")),
            (None, Some("server")) | (None, None) => None,
            (None, Some(_)) => None,
        };
        return Ok(Some(Pairing::new(
            address,
            credentials,
            entity,
            delivery_id,
        )));
    }
    Ok(None)
}

fn parse_u64(value: U64Like, field: &'static str) -> Result<u64, ParseError> {
    match value {
        U64Like::Number(value) => Ok(value),
        U64Like::String(value) => value.parse().map_err(|_| ParseError::Numeric(field)),
    }
}
fn parse_i64(value: I64Like, field: &'static str) -> Result<i64, ParseError> {
    match value {
        I64Like::Number(value) => Ok(value),
        I64Like::String(value) => value.parse().map_err(|_| ParseError::Numeric(field)),
    }
}
fn parse_u16(value: U64Like, field: &'static str) -> Result<u16, ParseError> {
    u16::try_from(parse_u64(value, field)?)
        .map_err(|_| ParseError::Numeric(field))
        .and_then(|port| {
            if port == 0 {
                Err(ParseError::Invalid(field))
            } else {
                Ok(port)
            }
        })
}
fn parse_u32(value: U64Like, field: &'static str) -> Result<u32, ParseError> {
    u32::try_from(parse_u64(value, field)?).map_err(|_| ParseError::Numeric(field))
}
fn validate_address(host: String, port: u16) -> Result<ServerAddress, ParseError> {
    let host = host.trim().to_owned();
    if host.is_empty()
        || host
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        || host.contains("//")
        || host.contains('/')
        || host.contains('?')
        || host.contains('#')
    {
        return Err(ParseError::Invalid("ip"));
    }
    if host.contains(':')
        && host
            .parse::<IpAddr>()
            .map_or(true, |address| !address.is_ipv6())
    {
        return Err(ParseError::Invalid("ip"));
    }
    ServerAddress::new(host, port).map_err(|_| ParseError::Domain("ip"))
}

#[cfg(test)]
mod tests {
    use super::parse_notification;
    use crate::{ParseConfig, ParseError};

    #[test]
    fn parses_number_and_string_forms_from_any_app_data_key() -> Result<(), ParseError> {
        let mut notification =
            push_receiver::Notification::new(Vec::new(), Some("delivery-1".to_owned()));
        notification.app_data.push(push_receiver::AppData {
            key: "not-body".to_owned(),
            value: r#"{"ip":"::1","port":"1234","playerId":42,"playerToken":"-7","type":"server"}"#
                .to_owned(),
        });
        let Some(pairing) = parse_notification(&notification, &ParseConfig::default())? else {
            return Err(ParseError::Invalid("pairing"));
        };
        assert_eq!(pairing.address().host(), "::1");
        assert_eq!(pairing.address().port(), 1234);
        assert_eq!(pairing.credentials().player_id().value(), 42);
        assert_eq!(pairing.credentials().player_token().value(), -7);
        assert_eq!(
            pairing.delivery_id().map(|id| id.as_str()),
            Some("delivery-1")
        );
        Ok(())
    }

    #[test]
    fn ignores_unrelated_app_data_and_empty_payload() -> Result<(), ParseError> {
        let mut notification = push_receiver::Notification::new(Vec::new(), None);
        notification.app_data.push(push_receiver::AppData {
            key: "x".to_owned(),
            value: "not-json".to_owned(),
        });
        assert_eq!(
            parse_notification(&notification, &ParseConfig::default())?,
            None
        );
        Ok(())
    }

    #[test]
    fn rejects_out_of_range_secret_without_exposing_body() -> Result<(), ParseError> {
        let mut notification = push_receiver::Notification::new(Vec::new(), None);
        let body = r#"{"ip":"host","port":1234,"playerId":1,"playerToken":999999999999}"#;
        notification.app_data.push(push_receiver::AppData {
            key: "x".to_owned(),
            value: body.to_owned(),
        });
        let error = match parse_notification(&notification, &ParseConfig::default()) {
            Err(error) => error,
            Ok(_) => return Err(ParseError::Invalid("overflow")),
        };
        assert!(!error.to_string().contains(body));
        Ok(())
    }
}
