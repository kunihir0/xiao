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

fn is_allowed_token_shaped_string(s: &str, known_hashes: &BTreeSet<&str>) -> bool {
    if s == REDACTED_MARKER {
        return true;
    }
    // Allow uppercase enum/constant identifiers
    if s.chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
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
    {
        return true;
    }
    // Allow URL forms without query/userinfo
    if s.starts_with("https://")
        || s.starts_with("wss://")
        || s.starts_with("tcp+tls://")
        || s.starts_with("fcm://")
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
        let has_alpha = s.chars().any(|c| c.is_ascii_alphabetic());
        let has_digit = s.chars().any(|c| c.is_ascii_digit());
        if has_alpha && has_digit {
            return Err(EvidenceError::Secret(format!(
                "unredacted credential-shaped value detected at {path}"
            )));
        }
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
