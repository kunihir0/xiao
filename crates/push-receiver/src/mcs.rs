use prost::Message;

use crate::{Error, Notification, Result, checkin::CheckinCredentials, proto};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginParameters {
    pub credentials: CheckinCredentials,
    pub persistent_ids: Vec<String>,
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

pub fn decode_data_message(payload: &[u8]) -> Result<Notification> {
    let message = proto::DataMessageStanza::decode(payload)
        .map_err(|error| Error::Decode(error.to_string()))?;
    if message.from.is_empty() || message.category.is_empty() {
        return Err(Error::InvalidRegistration(
            "MCS data message missing required fields".into(),
        ));
    }
    let app_data = message
        .app_data
        .into_iter()
        .map(|entry| crate::AppData {
            key: entry.key,
            value: entry.value,
        })
        .collect();
    Ok(Notification {
        payload: message.raw_data.unwrap_or_default(),
        persistent_id: message.persistent_id,
        app_data,
        sent: message.sent,
    })
}

pub fn encode_heartbeat_ack(last_stream_id_received: Option<i32>) -> Result<Vec<u8>> {
    Ok(proto::HeartbeatAck {
        last_stream_id_received,
        ..Default::default()
    }
    .encode_to_vec())
}
