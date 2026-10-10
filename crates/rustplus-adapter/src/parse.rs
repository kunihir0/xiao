use std::net::{IpAddr, Ipv4Addr};

use bot_domain::{
    DeliveryId, DomainError, EntityId, EntityPairing, Pairing, PlayerCredentials, PlayerId,
    PlayerToken, ServerAddress,
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
/// Parses one push notification into a typed pairing using the same bounded
/// logic as [`crate::PushPairingSource`]. Returns `Ok(None)` when the
/// notification carries no pairing payload.
pub fn parse_pairing_notification(
    notification: &push_receiver::Notification,
    config: &ParseConfig,
) -> Result<Option<Pairing>, ParseError> {
    parse_notification(notification, config)
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
                    EntityPairing::new(id, raw.entity_type, raw.entity_name).map_err(|error| {
                        match error {
                            DomainError::Invalid {
                                field: "entity_type",
                            } => ParseError::Domain("entityType"),
                            DomainError::Invalid {
                                field: "entity_name",
                            } => ParseError::Domain("entityName"),
                            DomainError::Invalid { .. } => ParseError::Domain("entity"),
                        }
                    })?,
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
    let address = parse_public_ip(&host)?;
    ServerAddress::new(address.to_string(), port).map_err(|_| ParseError::Domain("ip"))
}

pub(crate) fn parse_public_ip(host: &str) -> Result<IpAddr, ParseError> {
    if host.is_empty()
        || host.chars().any(|character| {
            character.is_control()
                || character.is_whitespace()
                || matches!(character, '@' | '/' | '\\' | '?' | '#' | '%')
        })
    {
        return Err(ParseError::Invalid("ip"));
    }
    let literal = host
        .strip_prefix('[')
        .map_or(host, |value| value.strip_suffix(']').unwrap_or(""));
    if literal.is_empty() || host.starts_with('[') != host.ends_with(']') {
        return Err(ParseError::HostNotIpLiteral);
    }
    let address = literal
        .parse::<IpAddr>()
        .map_err(|_| ParseError::HostNotIpLiteral)?;
    if host.starts_with('[') && !address.is_ipv6() {
        return Err(ParseError::HostNotIpLiteral);
    }
    if is_special_use(address) {
        return Err(ParseError::SpecialUseIp);
    }
    Ok(address)
}

fn is_special_use(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_special_v4(address),
        IpAddr::V6(address) => {
            let segments = address.segments();
            // Any IPv6 form that embeds or translates an IPv4 address
            // (IPv4-mapped/compatible ::/96, NAT64 64:ff9b::/96, 6to4
            // 2002::/16, Teredo 2001:0::/32) is rejected outright: these can
            // reach IPv4 destinations whose policy was never evaluated, and
            // Rust+ servers publish plain IPv4 or native IPv6 addresses.
            let embeds_ipv4 = address.to_ipv4().is_some()
                || (segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2..6] == [0; 4])
                || segments[0] == 0x2002
                || (segments[0] == 0x2001 && segments[1] == 0);
            embeds_ipv4
                || address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || (segments[0] & 0xfe00) == 0xfc00
                || (segments[0] & 0xffc0) == 0xfe80
                || (u128::from(address) >> 96) == u128::from(0x2001_0db8_u32)
                || (segments[0] == 0x2001 && segments[1] == 0x0002 && segments[2] == 0)
        }
    }
}

fn is_special_v4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_multicast()
        || octets[0] == 0
        || octets[0] >= 240
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        || (octets[0] == 198 && (octets[1] & 0xfe) == 18)
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
}

#[cfg(test)]
mod tests {
    use super::{parse_notification, parse_public_ip};
    use crate::{ParseConfig, ParseError};
    use bot_domain::{MAX_ENTITY_KIND_BYTES, MAX_ENTITY_NAME_BYTES};
    use proptest::prelude::*;

    #[test]
    fn parses_number_and_string_forms_from_any_app_data_key() -> Result<(), ParseError> {
        let mut notification =
            push_receiver::Notification::new(Vec::new(), Some("delivery-1".to_owned()));
        notification.app_data.push(push_receiver::AppData {
            key: "not-body".to_owned(),
            value: r#"{"ip":"[2001:4860:4860::8888]","port":"1234","playerId":42,"playerToken":"-7","type":"server"}"#
                .to_owned(),
        });
        let Some(pairing) = parse_notification(&notification, &ParseConfig::default())? else {
            return Err(ParseError::Invalid("pairing"));
        };
        assert_eq!(pairing.address().host(), "2001:4860:4860::8888");
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
        let body = r#"{"ip":"8.8.8.8","port":1234,"playerId":1,"playerToken":999999999999}"#;
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

    fn notification(body: &str) -> push_receiver::Notification {
        let mut notification =
            push_receiver::Notification::new(Vec::new(), Some("delivery-2".to_owned()));
        notification.app_data.push(push_receiver::AppData {
            key: "payload".to_owned(),
            value: body.to_owned(),
        });
        notification
    }

    fn server_body(host: &str, port: &str, player_id: &str, token: &str) -> String {
        format!(
            r#"{{"ip":{host},"port":{port},"playerId":{player_id},"playerToken":{token},"type":"server"}}"#
        )
    }

    #[test]
    fn public_wrapper_matches_internal_parser() -> Result<(), ParseError> {
        let config = ParseConfig::default();
        let valid = notification(&server_body(r#""8.8.8.8""#, "28015", "42", "7"));
        assert!(super::parse_pairing_notification(&valid, &config)?.is_some());
        let invalid = notification(&server_body(r#""8.8.8.8""#, "0", "42", "7"));
        assert_eq!(
            super::parse_pairing_notification(&invalid, &config).err(),
            parse_notification(&invalid, &config).err()
        );
        let empty = push_receiver::Notification::new(Vec::new(), None);
        assert!(super::parse_pairing_notification(&empty, &config)?.is_none());
        Ok(())
    }

    #[test]
    fn chooses_the_first_valid_body_after_unrelated_data() -> Result<(), ParseError> {
        let mut notification = push_receiver::Notification::new(Vec::new(), None);
        notification.app_data.extend([
            push_receiver::AppData {
                key: "title".to_owned(),
                value: "not a pairing".to_owned(),
            },
            push_receiver::AppData {
                key: "metadata".to_owned(),
                value: r#"{"unrelated":true}"#.to_owned(),
            },
            push_receiver::AppData {
                key: "pairing".to_owned(),
                value: server_body(r#""8.8.8.8""#, "28015", "42", "-7"),
            },
            push_receiver::AppData {
                key: "pairing".to_owned(),
                value: server_body(r#""1.1.1.1""#, "28016", "43", "-8"),
            },
        ]);
        let pairing = parse_notification(&notification, &ParseConfig::default())?
            .ok_or(ParseError::Missing("pairing"))?;
        assert_eq!(pairing.address().host(), "8.8.8.8");
        assert_eq!(pairing.address().port(), 28015);
        Ok(())
    }

    #[test]
    fn rejects_port_and_token_values_outside_their_domains() {
        for (field, body) in [
            ("port", server_body(r#""8.8.8.8""#, "0", "42", "7")),
            ("port", server_body(r#""8.8.8.8""#, "-1", "42", "7")),
            ("port", server_body(r#""8.8.8.8""#, "65536", "42", "7")),
            (
                "port",
                server_body(r#""8.8.8.8""#, "18446744073709551616", "42", "7"),
            ),
            (
                "playerToken",
                server_body(r#""8.8.8.8""#, "28015", "42", "2147483648"),
            ),
            (
                "playerToken",
                server_body(r#""8.8.8.8""#, "28015", "42", "-2147483649"),
            ),
        ] {
            let result = parse_notification(&notification(&body), &ParseConfig::default());
            assert!(
                matches!(
                    result,
                    Err(ParseError::Invalid(_))
                        | Err(ParseError::Numeric(_))
                        | Err(ParseError::Malformed)
                ),
                "{field}"
            );
        }
    }

    #[test]
    fn rejects_zero_player_id() {
        let body = server_body(r#""8.8.8.8""#, "28015", "0", "7");
        assert!(matches!(
            parse_notification(&notification(&body), &ParseConfig::default()),
            Err(ParseError::Domain("playerId"))
        ));
    }

    #[test]
    fn rejects_scheme_path_control_and_malformed_ipv6_hosts() {
        for host in [
            r#""https://example.com""#,
            r#""example.com/path""#,
            r#""example.com\u0000""#,
            r#""  example.com""#,
            r#""[2001:db8::1""#,
            r#""2001:db8:::1""#,
        ] {
            let body = server_body(host, "28015", "42", "7");
            assert!(matches!(
                parse_notification(&notification(&body), &ParseConfig::default()),
                Err(ParseError::Invalid(_))
                    | Err(ParseError::Domain(_))
                    | Err(ParseError::HostNotIpLiteral)
                    | Err(ParseError::SpecialUseIp)
                    | Err(ParseError::Numeric(_))
                    | Err(ParseError::Malformed)
            ));
        }
    }

    #[test]
    fn oversized_body_is_rejected_before_deserialization() {
        let body = server_body(r#""8.8.8.8""#, "28015", "42", "7");
        let config = ParseConfig {
            max_value_bytes: body.len() - 1,
            ..ParseConfig::default()
        };
        assert_eq!(
            parse_notification(&notification(&body), &config),
            Err(ParseError::TooLarge)
        );
    }

    #[test]
    fn token_never_appears_in_error_debug_or_display() {
        let secret = "2147483648";
        let body = server_body(r#""8.8.8.8""#, "28015", "42", secret);
        let result = parse_notification(&notification(&body), &ParseConfig::default());
        assert!(result.is_err());
        if let Err(error) = result {
            assert!(!format!("{error:?}").contains(secret));
            assert!(!error.to_string().contains(secret));
        }
    }

    #[test]
    fn accepts_public_literals_and_rejects_special_or_hostname_hosts() {
        for host in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.1",
            "224.0.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:10.0.0.1",
            "::ffff:8.8.8.8",
            "::8.8.8.8",
            "64:ff9b::808:808",
            "2002:0a00:0001::1",
            "2002:0808:0808::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "0.1.2.3",
            "192.0.0.8",
            "198.18.0.1",
            "198.19.255.1",
            "240.0.0.1",
            "2001:2::1",
        ] {
            assert_eq!(
                parse_public_ip(host),
                Err(ParseError::SpecialUseIp),
                "{host}"
            );
        }
        for host in ["example.com", "8.8.8.8@evil", "8.8.8.8/path", "8.8.8.8%25"] {
            assert!(
                matches!(
                    parse_public_ip(host),
                    Err(ParseError::HostNotIpLiteral | ParseError::Invalid("ip"))
                ),
                "{host}"
            );
        }
        assert!(parse_public_ip("8.8.8.8").is_ok());
        assert!(parse_public_ip("2001:4860:4860::8888").is_ok());
        assert!(parse_public_ip("[2001:4860:4860::8888]").is_ok());
    }

    fn entity_body(kind: &str, name: &str) -> String {
        format!(
            r#"{{"ip":"8.8.8.8","port":28015,"playerId":42,"playerToken":7,"entityId":1,"entityType":{kind:?},"entityName":{name:?},"type":"entity"}}"#
        )
    }

    #[test]
    fn entity_metadata_accepts_exact_limits_and_rejects_overflow() {
        let exact = entity_body(
            &"k".repeat(MAX_ENTITY_KIND_BYTES),
            &"n".repeat(MAX_ENTITY_NAME_BYTES),
        );
        assert!(matches!(
            parse_notification(&notification(&exact), &ParseConfig::default()),
            Ok(Some(_))
        ));

        let kind_overflow = entity_body(
            &"k".repeat(MAX_ENTITY_KIND_BYTES + 1),
            &"n".repeat(MAX_ENTITY_NAME_BYTES),
        );
        assert_eq!(
            parse_notification(&notification(&kind_overflow), &ParseConfig::default()),
            Err(ParseError::Domain("entityType"))
        );

        let name_overflow = entity_body(
            &"k".repeat(MAX_ENTITY_KIND_BYTES),
            &"n".repeat(MAX_ENTITY_NAME_BYTES + 1),
        );
        assert_eq!(
            parse_notification(&notification(&name_overflow), &ParseConfig::default()),
            Err(ParseError::Domain("entityName"))
        );
    }

    #[test]
    fn entity_metadata_rejects_control_characters_without_values() {
        for (kind, name, field) in [
            ("kind\n", "name", "entityType"),
            ("kind", "name\t", "entityName"),
        ] {
            let body = entity_body(kind, name);
            let result = parse_notification(&notification(&body), &ParseConfig::default());
            assert_eq!(result, Err(ParseError::Domain(field)));
            assert!(!format!("{result:?}").contains(kind));
            assert!(!format!("{result:?}").contains(name));
        }
    }

    proptest! {
        #[test]
        fn arbitrary_push_values_are_panic_free_and_size_bounded(
            bytes in prop::collection::vec(any::<u8>(), 0..2048)
        ) {
            let value = String::from_utf8_lossy(&bytes).into_owned();
            let mut push = push_receiver::Notification::new(Vec::new(), None);
            push.app_data.push(push_receiver::AppData {
                key: "property".to_owned(),
                value: value.clone(),
            });
            let config = ParseConfig {
                max_value_bytes: 512,
                ..ParseConfig::default()
            };
            let result = parse_notification(&push, &config);
            // The parser bounds the trimmed app-data value; surrounding
            // whitespace is ignored before admission.
            if value.trim().len() > config.max_value_bytes {
                prop_assert_eq!(result, Err(ParseError::TooLarge));
            }
        }
    }
}
