use prost::Message;

use crate::{Error, Notification, Result, checkin::CheckinCredentials, proto};

pub const MAX_PERSISTENT_ID_BYTES: usize = crate::dedupe::MAX_PERSISTENT_ID_BYTES;
pub const MAX_APP_DATA_ITEMS: usize = 128;
pub const MAX_APP_DATA_BYTES: usize = 64 * 1024;
pub const MAX_RAW_DATA_BYTES: usize = crate::MAX_NOTIFICATION_BYTES;

#[derive(Clone, PartialEq, Eq)]
pub struct LoginParameters {
    pub credentials: CheckinCredentials,
    pub persistent_ids: Vec<String>,
}

impl std::fmt::Debug for LoginParameters {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoginParameters")
            .field("credentials", &self.credentials)
            .field(
                "persistent_ids",
                &format_args!("<{} ids>", self.persistent_ids.len()),
            )
            .finish()
    }
}

pub fn encode_login(parameters: &LoginParameters) -> Result<Vec<u8>> {
    let credentials = parameters.credentials;
    let request = proto::LoginRequest {
        id: "chrome-63.0.3234.0".into(),
        domain: "mcs.android.com".into(),
        user: credentials.android_id().to_string(),
        resource: credentials.android_id().to_string(),
        auth_token: credentials.security_token().to_string(),
        device_id: Some(format!("android-{:x}", credentials.android_id())),
        adaptive_heartbeat: Some(false),
        auth_service: Some(2),
        network_type: Some(1),
        use_rmq2: Some(true),
        setting: vec![proto::Setting {
            name: "new_vc".into(),
            value: "1".into(),
        }],
        received_persistent_id: parameters.persistent_ids.clone(),
        ..Default::default()
    };
    Ok(request.encode_to_vec())
}

pub fn decode_login_response(payload: &[u8]) -> Result<DurationConfig> {
    if payload.len() > crate::MAX_FRAME_SIZE {
        return Err(Error::FrameTooLarge {
            actual: payload.len(),
            max: crate::MAX_FRAME_SIZE,
        });
    }
    let response =
        proto::LoginResponse::decode(payload).map_err(|error| Error::Decode(error.to_string()))?;
    if response.id.is_empty() {
        return Err(Error::InvalidRegistration(
            "MCS login response missing id".into(),
        ));
    }
    if let Some(error) = response.error.filter(|error| error.code != 0) {
        return Err(Error::InvalidRegistration(format!(
            "MCS login {}",
            error.code
        )));
    }
    let millis = response
        .heartbeat_config
        .and_then(|config| config.interval_ms)
        .map(i64::from)
        .filter(|value| *value > 0)
        .unwrap_or(300_000);
    Ok(DurationConfig {
        heartbeat_millis: millis.clamp(30_000, 900_000) as u64,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurationConfig {
    pub heartbeat_millis: u64,
}

pub(crate) struct DecodedDataMessage {
    pub notification: Notification,
    pub immediate_ack: bool,
    pub stream_id: Option<i32>,
}

pub(crate) fn decode_data_message_with_metadata(payload: &[u8]) -> Result<DecodedDataMessage> {
    if payload.len() > crate::MAX_FRAME_SIZE {
        return Err(Error::FrameTooLarge {
            actual: payload.len(),
            max: crate::MAX_FRAME_SIZE,
        });
    }
    let message = proto::DataMessageStanza::decode(payload)
        .map_err(|error| Error::Decode(error.to_string()))?;
    if message.from.is_empty() || message.category.is_empty() {
        return Err(Error::InvalidRegistration(
            "MCS data message missing required fields".into(),
        ));
    }
    if message.app_data.len() > MAX_APP_DATA_ITEMS {
        return Err(Error::NotificationTooLarge);
    }
    let app_data_bytes = message
        .app_data
        .iter()
        .map(|entry| entry.key.len().saturating_add(entry.value.len()))
        .sum::<usize>();
    if app_data_bytes > MAX_APP_DATA_BYTES {
        return Err(Error::NotificationTooLarge);
    }
    let persistent_id = message.persistent_id;
    if persistent_id
        .as_ref()
        .is_some_and(|id| id.is_empty() || id.len() > MAX_PERSISTENT_ID_BYTES)
    {
        return Err(Error::InvalidRegistration(
            "MCS persistent ID is empty or oversized".into(),
        ));
    }
    let payload = message.raw_data.unwrap_or_default();
    if payload.len() > MAX_RAW_DATA_BYTES {
        return Err(Error::NotificationTooLarge);
    }
    let app_data = message
        .app_data
        .into_iter()
        .map(|entry| crate::AppData {
            key: entry.key,
            value: entry.value,
        })
        .collect();
    Ok(DecodedDataMessage {
        notification: Notification {
            payload,
            persistent_id,
            app_data,
            sent: message.sent,
        },
        immediate_ack: message.immediate_ack.unwrap_or(false),
        stream_id: message.stream_id,
    })
}

pub fn decode_data_message(payload: &[u8]) -> Result<Notification> {
    Ok(decode_data_message_with_metadata(payload)?.notification)
}

pub fn encode_heartbeat_ack(last_stream_id_received: Option<i32>) -> Result<Vec<u8>> {
    Ok(proto::HeartbeatAck {
        last_stream_id_received,
        ..Default::default()
    }
    .encode_to_vec())
}
pub(crate) fn encode_heartbeat_ping(last_stream_id_received: Option<i32>) -> Result<Vec<u8>> {
    Ok(proto::HeartbeatPing {
        last_stream_id_received,
        ..Default::default()
    }
    .encode_to_vec())
}

pub(crate) fn encode_stream_ack(last_stream_id_received: i32) -> Result<Vec<u8>> {
    Ok(proto::IqStanza {
        r#type: proto::iq_stanza::IqType::Set as i32,
        id: String::new(),
        extension: Some(proto::Extension {
            id: 13,
            data: Vec::new(),
        }),
        last_stream_id_received: Some(last_stream_id_received),
        ..Default::default()
    }
    .encode_to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consumes_independent_login_and_data_fixtures() {
        let login = include_str!("../fixtures/login_response.hex")
            .split_whitespace()
            .filter_map(|pair| u8::from_str_radix(pair, 16).ok())
            .collect::<Vec<_>>();
        assert!(decode_login_response(&login).is_ok());
        let data = include_str!("../fixtures/data_message.hex")
            .split_whitespace()
            .filter_map(|pair| u8::from_str_radix(pair, 16).ok())
            .collect::<Vec<_>>();
        let notification = decode_data_message(&data);
        assert_eq!(
            notification.ok().map(|value| value.payload),
            Some(b"hello".to_vec())
        );
    }
}
