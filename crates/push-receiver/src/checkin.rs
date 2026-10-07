use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use p256::PublicKey;
use prost::Message;
use serde::Deserialize;
use url::Url;

use crate::{Error, Result, proto};

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CheckinCredentials {
    android_id: u64,
    security_token: u64,
}

impl std::fmt::Debug for CheckinCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CheckinCredentials")
            .field("android_id", &self.android_id)
            .field("security_token", &"<redacted>")
            .finish()
    }
}

impl CheckinCredentials {
    pub fn new(android_id: u64, security_token: u64) -> Result<Self> {
        if android_id == 0 {
            return Err(Error::MissingCredential("android_id"));
        }
        if security_token == 0 {
            return Err(Error::MissingCredential("security_token"));
        }
        Ok(Self {
            android_id,
            security_token,
        })
    }
    #[must_use]
    pub const fn android_id(self) -> u64 {
        self.android_id
    }
    #[must_use]
    pub const fn security_token(self) -> u64 {
        self.security_token
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckinResult {
    pub credentials: CheckinCredentials,
}

pub fn build_checkin_request(previous: Option<CheckinCredentials>) -> Vec<u8> {
    let credentials = previous.map(|value| value.android_id() as i64);
    let security = previous.map(CheckinCredentials::security_token);
    let request = proto::AndroidCheckinRequest {
        user_serial_number: Some(0),
        checkin: proto::AndroidCheckinProto {
            r#type: Some(3),
            chrome_build: Some(proto::ChromeBuildProto {
                platform: Some(2),
                chrome_version: Some("63.0.3234.0".to_owned()),
                channel: Some(1),
            }),
            ..Default::default()
        },
        version: Some(3),
        id: credentials,
        security_token: security,
        ..Default::default()
    };
    request.encode_to_vec()
}

pub fn parse_checkin_response(bytes: &[u8]) -> Result<CheckinResult> {
    let response = proto::AndroidCheckinResponse::decode(bytes)
        .map_err(|error| Error::Decode(error.to_string()))?;
    if !response.stats_ok {
        return Err(Error::InvalidRegistration(
            "check-in response did not accept statistics".into(),
        ));
    }
    let android_id = response
        .android_id
        .ok_or(Error::MissingCredential("android_id"))?;
    let security_token = response
        .security_token
        .ok_or(Error::MissingCredential("security_token"))?;
    Ok(CheckinResult {
        credentials: CheckinCredentials::new(android_id, security_token)?,
    })
}

#[derive(Clone, PartialEq, Eq)]
pub struct RegistrationToken(String);
impl std::fmt::Debug for RegistrationToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RegistrationToken(<redacted>)")
    }
}
impl RegistrationToken {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 4096
            || value
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err(Error::InvalidRegistration(
                "empty, oversized, or malformed token".into(),
            ));
        }
        Ok(Self(value))
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Parses the legacy `token=...` registration response without accepting error bodies or extra fields.
pub fn parse_registration_response(body: &str) -> Result<RegistrationToken> {
    let body = body.trim();
    let Some(value) = body.strip_prefix("token=") else {
        return Err(Error::InvalidRegistration("missing token prefix".into()));
    };
    RegistrationToken::new(value)
}

#[derive(Deserialize)]
pub struct SubscriptionResponse {
    pub endpoint: Option<String>,
    pub keys: Option<SubscriptionKeys>,
}
#[derive(Deserialize)]
pub struct SubscriptionKeys {
    pub p256dh: Option<String>,
    pub auth: Option<String>,
}

impl std::fmt::Debug for SubscriptionResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubscriptionResponse")
            .field("endpoint", &self.endpoint.as_ref().map(|_| "<redacted>"))
            .field("keys", &self.keys.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}
impl std::fmt::Debug for SubscriptionKeys {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SubscriptionKeys(<redacted>)")
    }
}

pub fn validate_subscription_response(value: &SubscriptionResponse) -> Result<()> {
    let endpoint = value
        .endpoint
        .as_deref()
        .ok_or_else(|| Error::InvalidRegistration("missing subscription endpoint".into()))?;
    let endpoint = Url::parse(endpoint)
        .map_err(|_| Error::InvalidRegistration("invalid subscription endpoint".into()))?;
    if endpoint.scheme() != "https" || endpoint.host_str().is_none() {
        return Err(Error::InvalidRegistration(
            "subscription endpoint must be HTTPS".into(),
        ));
    }
    let keys = value
        .keys
        .as_ref()
        .ok_or_else(|| Error::InvalidRegistration("missing subscription keys".into()))?;
    let p256dh = keys
        .p256dh
        .as_deref()
        .ok_or_else(|| Error::InvalidRegistration("missing p256dh key".into()))?;
    let auth = keys
        .auth
        .as_deref()
        .ok_or_else(|| Error::InvalidRegistration("missing auth key".into()))?;
    let p256dh = URL_SAFE_NO_PAD
        .decode(p256dh)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(p256dh))
        .map_err(|error| Error::InvalidRegistration(format!("invalid p256dh key: {error}")))?;
    if p256dh.len() != 65 {
        return Err(Error::InvalidRegistration(
            "p256dh key must be 65 bytes".into(),
        ));
    }
    PublicKey::from_sec1_bytes(&p256dh)
        .map_err(|error| Error::InvalidRegistration(format!("invalid p256dh point: {error}")))?;
    let auth = URL_SAFE_NO_PAD
        .decode(auth)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(auth))
        .map_err(|error| Error::InvalidRegistration(format!("invalid auth key: {error}")))?;
    if auth.len() != 16 {
        return Err(Error::InvalidRegistration(
            "auth key must be 16 bytes".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_zero_checkin_credentials() {
        assert!(matches!(
            CheckinCredentials::new(0, 1),
            Err(Error::MissingCredential("android_id"))
        ));
        assert!(matches!(
            CheckinCredentials::new(1, 0),
            Err(Error::MissingCredential("security_token"))
        ));
    }
    #[test]
    fn rejects_missing_or_false_checkin_status() {
        let bytes = proto::AndroidCheckinResponse {
            stats_ok: false,
            ..Default::default()
        }
        .encode_to_vec();
        assert!(matches!(
            parse_checkin_response(&bytes),
            Err(Error::InvalidRegistration(_))
        ));
    }
    #[test]
    fn validates_subscription_shape_and_key_lengths() {
        let invalid = SubscriptionResponse {
            endpoint: Some("http://example.test".into()),
            keys: Some(SubscriptionKeys {
                p256dh: Some("AQ".into()),
                auth: Some("Ag".into()),
            }),
        };
        assert!(validate_subscription_response(&invalid).is_err());
        let malformed_point = SubscriptionResponse {
            endpoint: Some("https://push.example.test/send".into()),
            keys: Some(SubscriptionKeys {
                p256dh: Some(URL_SAFE_NO_PAD.encode([0_u8; 65])),
                auth: Some(URL_SAFE_NO_PAD.encode([2_u8; 16])),
            }),
        };
        assert!(validate_subscription_response(&malformed_point).is_err());
    }
    #[test]
    fn parses_reference_registration_response() {
        let token = parse_registration_response("token=abc.def");
        assert!(token.is_ok());
        assert_eq!(token.ok().map(|value| value.0), Some("abc.def".to_owned()));
        assert!(parse_registration_response("Error=INVALID").is_err());
        assert!(parse_registration_response("token=").is_err());
    }

    #[test]
    fn consumes_independent_checkin_and_registration_fixtures() {
        let bytes = include_str!("../fixtures/checkin_response.hex")
            .split_whitespace()
            .filter_map(|pair| u8::from_str_radix(pair, 16).ok())
            .collect::<Vec<_>>();
        let result = parse_checkin_response(&bytes);
        assert!(result.is_ok());
        let registration = include_str!("../fixtures/registration_response.txt").trim();
        assert_eq!(
            parse_registration_response(registration)
                .ok()
                .map(|token| token.as_str().to_owned()),
            Some("APA91bFcmLegacyReferenceRegistrationToken-7f3a".to_owned())
        );
    }
}
