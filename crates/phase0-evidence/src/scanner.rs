use crate::model::EvidenceError;
use serde_json::Value;
use std::collections::BTreeSet;
use url::Url;

pub const MAX_INPUT_BYTES: usize = 64 * 1024; // 64 KiB
pub const MAX_NESTING_DEPTH: usize = 32;
pub const MAX_STRING_LENGTH: usize = 2048;
pub const MAX_COLLECTION_SIZE: usize = 64;
pub const REDACTED_MARKER: &str = "[REDACTED]";

pub fn validate_raw_limits(raw: &str) -> Result<(), EvidenceError> {
    if raw.len() > MAX_INPUT_BYTES {
        return Err(EvidenceError::LimitExceeded(format!(
            "input size {} bytes exceeds maximum allowed {}",
            raw.len(),
            MAX_INPUT_BYTES
        )));
    }

    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaping = false;

    for b in raw.bytes() {
        if in_string {
            if escaping {
                escaping = false;
            } else if b == b'\\' {
                escaping = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }

        match b {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > MAX_NESTING_DEPTH {
                    return Err(EvidenceError::LimitExceeded(format!(
                        "nesting depth exceeds limit of {MAX_NESTING_DEPTH}"
                    )));
                }
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    Ok(())
}

pub fn scan_value(
    value: &Value,
    depth: usize,
    path: &str,
    known_hashes: &BTreeSet<&str>,
) -> Result<(), EvidenceError> {
    if depth > MAX_NESTING_DEPTH {
        return Err(EvidenceError::LimitExceeded(format!(
            "nesting depth at {path} exceeds {MAX_NESTING_DEPTH}"
        )));
    }

    match value {
        Value::Object(map) => {
            if map.len() > MAX_COLLECTION_SIZE {
                return Err(EvidenceError::LimitExceeded(format!(
                    "object at {path} exceeds {MAX_COLLECTION_SIZE} keys"
                )));
            }
            for (k, v) in map {
                let sub_path = format!("{path}.{k}");
                validate_key_and_value(k, v, &sub_path)?;
                scan_value(v, depth + 1, &sub_path, known_hashes)?;
            }
        }
        Value::Array(items) => {
            if items.len() > MAX_COLLECTION_SIZE {
                return Err(EvidenceError::LimitExceeded(format!(
                    "array at {path} exceeds {MAX_COLLECTION_SIZE} items"
                )));
            }
            for (idx, item) in items.iter().enumerate() {
                scan_value(item, depth + 1, &format!("{path}[{idx}]"), known_hashes)?;
            }
        }
        Value::String(s) => {
            validate_string_content(s, path, known_hashes)?;
            // Check for nested JSON string
            let trimmed = s.trim();
            let is_json_delimited = (trimmed.starts_with('{') && trimmed.ends_with('}'))
                || (trimmed.starts_with('[') && trimmed.ends_with(']'));
            if is_json_delimited {
                #[allow(clippy::collapsible_if)]
                if let Ok(nested) = serde_json::from_str::<Value>(trimmed) {
                    scan_value(&nested, depth + 1, &format!("{path}::json"), known_hashes)?;
                }
            }
        }
        Value::Number(n) => {
            if let Some(num) = n.as_u64() {
                validate_numeric_id(num, path)?;
            }
        }
        Value::Bool(_) | Value::Null => {}
    }
    Ok(())
}

fn is_sensitive_key(k: &str) -> bool {
    let normalized = k.to_ascii_lowercase().replace(['-', '_', ':'], "");
    if normalized == "authscheme" || normalized == "authservice" || normalized == "authversion" {
        return false;
    }
    const SENSITIVE_PATTERNS: &[&str] = &[
        "token",
        "password",
        "cookie",
        "auth",
        "secret",
        "private",
        "credential",
        "apikey",
        "refresh",
        "encryptionkey",
        "security",
        "androidid",
    ];

    for pat in SENSITIVE_PATTERNS {
        if normalized.contains(pat) {
            return true;
        }
    }
    if normalized.ends_with("key") && normalized != "key" {
        return true;
    }
    false
}

fn validate_key_and_value(key: &str, value: &Value, path: &str) -> Result<(), EvidenceError> {
    if is_sensitive_key(key) {
        match value {
            Value::String(s) if s == REDACTED_MARKER => {}
            _ => {
                return Err(EvidenceError::Secret(format!(
                    "key '{key}' at {path} must have value '{REDACTED_MARKER}'"
                )));
            }
        }
    } else if key == "key" {
        // AppData key exception
        match value {
            Value::String(s) if s == "pairing" => {}
            Value::String(s) if s == REDACTED_MARKER => {}
            _ => {
                return Err(EvidenceError::Secret(format!(
                    "key '{key}' at {path} has unexpected value"
                )));
            }
        }
    }
    Ok(())
}

const ALLOWED_ASSERTIONS_AND_PROOFS: &[&str] = &[
    "ENROLLMENT_SEQUENCE_STEPS_MATCH",
    "ENROLLMENT_CHECKIN_ENDPOINT_MATCH",
    "ENROLLMENT_REGISTER3_ENDPOINT_MATCH",
    "ENROLLMENT_EXPO_ENDPOINT_MATCH",
    "ENROLLMENT_FACEPUNCH_ENDPOINT_MATCH",
    "ENROLLMENT_FACEPUNCH_HOST_IS_COMPANION_RUST",
    "ENROLLMENT_CREDENTIALS_REDACTED",
    "MCS_ENDPOINT_MATCHES_MTALK",
    "MCS_TRANSPORT_IS_RAW_TCP_TLS",
    "MCS_PROTOCOL_VERSION_41",
    "MCS_LOGIN_PAYLOAD_FIELDSET_COMPLETE",
    "MCS_LOGIN_CREDENTIALS_REDACTED",
    "MCS_READINESS_TAG_IS_LOGIN_RESPONSE",
    "PAIRING_CURRENT_IS_FCM_APP_DATA",
    "PAIRING_MODERN_IS_EXPO_NOTIFICATION",
    "PAIRING_SYNTHETIC_PLAYER_ID_MATCHES_NORMALIZED",
    "PAIRING_PLAYER_TOKEN_REDACTED",
    "PAIRING_NORMALIZED_ADDRESS_MATCHES_NESTED",
    "PAIRING_NO_INVENTED_PROVIDER_RESPONSE",
    "GET_ENTITY_INFO_WEBSOCKET_ENDPOINT_NO_PATH",
    "GET_ENTITY_INFO_CURRENT_IS_WSS_AND_MODERN_IS_WS",
    "GET_ENTITY_INFO_REQUEST_HAS_ENTITY_ID_AND_PLAYER",
    "GET_ENTITY_INFO_RESPONSE_TYPE_IS_ALARM_2",
    "GET_ENTITY_INFO_SEQUENCE_MATCHES",
    "GET_ENTITY_INFO_PLAYER_CREDENTIALS_REDACTED",
    "ALARM_METHOD_IS_PROTOBUF_BROADCAST",
    "ALARM_CURRENT_IS_WSS_AND_MODERN_IS_WS",
    "ALARM_BROADCAST_HAS_ENTITY_CHANGED",
    "ALARM_PRIMED_ENTITY_ID_MATCHES",
    "ALARM_PRIMED_ENTITY_TYPE_IS_ALARM_2",
    "ALARM_NORMALIZED_EVENT_MATCHES_PAYLOAD",
    "ALARM_LIVE_PROOF_NEEDED_DECLARED",
    "IN_GAME_SMART_ALARM_STATE_CHANGE",
    "RUST_SERVER_TO_CLIENT_WEBSOCKET_BROADCAST_DELIVERY",
    "STEAM_ACCOUNT_COMPANION_AUTHENTICATION",
    "LIVE_PROVIDER_PUSH_DELIVERY",
];
fn is_allowed_token_shaped_string(s: &str, known_hashes: &BTreeSet<&str>) -> bool {
    if s == REDACTED_MARKER {
        return true;
    }
    // Allow known assertion and live proof identifiers exactly
    if ALLOWED_ASSERTIONS_AND_PROOFS.contains(&s) {
        return true;
    }
    // Allow JSON-delimited strings (their parsed structure is scanned separately)
    let trimmed = s.trim();
    if (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'))
    {
        return true;
    }
    if known_hashes.contains(s) {
        return true;
    }
    if s.starts_with("synthetic-") {
        return true;
    }
    if s == "49451aca-a822-41e6-ad59-955718d0ff9c" {
        return true;
    }
    if s == "com.facepunch.rust.companion"
        || s == "org.chromium.linux"
        || s == "org.mozilla.firefox"
        || s == "chrome-63.0.3234.0"
        || s == "mcs.android.com"
        || s == "1:976529667804:android:d6f1ddeb4403b338fea619"
        || s == "2026-10-07"
        || s.starts_with("application/")
        || s == "mcs_version_tag_delimited"
        || s == "facepunch_register"
        || s == "firebase_installations"
        || s == "FCM_NOTIFICATION_APP_DATA"
        || s == "EXPO_PUSH_NOTIFICATION"
        || s == "MCS_FRAMED_PROTOBUF"
        || s == "PROTOBUF_BROADCAST"
        || s == "PROTOBUF_RPC"
        || s == "xiao-current-rustplus"
        || s == "rustplus-current-protocol"
        || s == "push-receiver-current"
        || s == "rustplus-js-modern"
        || s == "liam-push-receiver"
    {
        return true;
    }
    // Allow URL forms without query/userinfo
    if s.starts_with("https://")
        || s.starts_with("wss://")
        || s.starts_with("ws://")
        || s.starts_with("tcp+tls://")
        || s.starts_with("fcm://")
        || s.starts_with("expo://")
    {
        return true;
    }
    // Allow source reference strings, e.g. name@commit
    if s.contains('@')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '-' | '_' | ';' | '.'))
    {
        return true;
    }
    // Allow file paths
    if s.split(';').all(|part| {
        part.ends_with(".rs")
            || part.ends_with(".proto")
            || part.ends_with(".js")
            || part.ends_with(".txt")
            || part.ends_with(".md")
    }) {
        return true;
    }
    false
}

fn validate_string_content(
    s: &str,
    path: &str,
    known_hashes: &BTreeSet<&str>,
) -> Result<(), EvidenceError> {
    if s.len() > MAX_STRING_LENGTH {
        return Err(EvidenceError::LimitExceeded(format!(
            "string length at {path} exceeds {MAX_STRING_LENGTH}"
        )));
    }

    if s.chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t' && c != '\r')
    {
        return Err(EvidenceError::Malformed(format!(
            "control characters detected at {path}"
        )));
    }

    let lower = s.to_ascii_lowercase();

    if lower.contains("-----begin") {
        return Err(EvidenceError::Secret(format!(
            "PEM header detected at {path}"
        )));
    }

    if lower.contains("bearer ") {
        return Err(EvidenceError::Secret(format!(
            "Bearer token detected at {path}"
        )));
    }

    const SECRET_ASSIGNMENTS: &[&str] = &[
        "token=",
        "secret=",
        "api_key=",
        "apikey=",
        "password=",
        "passwd=",
        "session=",
        "auth=",
        "access_token=",
    ];

    for assign in SECRET_ASSIGNMENTS {
        if lower.contains(assign) {
            return Err(EvidenceError::Secret(format!(
                "secret assignment pattern detected at {path}"
            )));
        }
    }

    // URL validation
    if s.contains("://") {
        validate_url(s, path)?;
    }

    // SteamID64 production-shaped checks
    if s.starts_with("7656119") && s.len() == 17 && s.chars().all(|c| c.is_ascii_digit()) {
        return Err(EvidenceError::Secret(format!(
            "production-shaped SteamID64 detected at {path}"
        )));
    }

    // Known token prefixes
    const SENSITIVE_PREFIXES: &[&str] = &["ghp_", "gho_", "aiza", "akia"];
    for prefix in SENSITIVE_PREFIXES {
        if lower.starts_with(prefix) {
            return Err(EvidenceError::Secret(format!(
                "token prefix detected at {path}"
            )));
        }
    }

    // Hex string check: reject long hex unless it is in known_hashes (or allowed UUID/commit)
    if s.len() >= 32 && s.chars().all(|c| c.is_ascii_hexdigit()) && !known_hashes.contains(s) {
        return Err(EvidenceError::Secret(format!(
            "unredacted hex secret or hash detected at {path}"
        )));
    }

    // Check for random high-entropy token-shaped string under any key
    if s.len() >= 16 && !s.contains(' ') && !is_allowed_token_shaped_string(s, known_hashes) {
        return Err(EvidenceError::Secret(format!(
            "unredacted credential-shaped value detected at {path}"
        )));
    }

    Ok(())
}

fn validate_numeric_id(id: u64, path: &str) -> Result<(), EvidenceError> {
    // SteamID64 range starts around 76561197960265728
    if id >= 76_560_000_000_000_000 {
        return Err(EvidenceError::Secret(format!(
            "production-shaped numeric SteamID64 detected at {path}"
        )));
    }
    Ok(())
}

pub fn validate_url(s: &str, path: &str) -> Result<(), EvidenceError> {
    if s.contains('?') || s.contains('#') || s.contains('@') {
        return Err(EvidenceError::Malformed(format!(
            "URL at {path} contains forbidden query, fragment, or userinfo"
        )));
    }

    if s.split_once("://")
        .is_some_and(|(_, rest)| rest.starts_with('/'))
    {
        return Err(EvidenceError::Malformed(format!(
            "URL at {path} has empty host"
        )));
    }

    let parsed =
        Url::parse(s).map_err(|_| EvidenceError::Malformed(format!("invalid URL at {path}")))?;

    if parsed.host_str().is_none() || parsed.host_str() == Some("") {
        return Err(EvidenceError::Malformed(format!(
            "URL at {path} lacks host"
        )));
    }

    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(EvidenceError::Secret(format!(
            "URL at {path} contains userinfo"
        )));
    }

    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(EvidenceError::Malformed(format!(
            "URL at {path} contains query or fragment"
        )));
    }

    if s.starts_with("https://github.com/") {
        let path_segments: Vec<&str> = parsed
            .path_segments()
            .map(|c| c.collect())
            .unwrap_or_default();
        if path_segments.len() != 2 || path_segments[0].is_empty() || path_segments[1].is_empty() {
            return Err(EvidenceError::Malformed(format!(
                "repository URL at {path} must be 'https://github.com/owner/repo'"
            )));
        }
    }

    Ok(())
}
