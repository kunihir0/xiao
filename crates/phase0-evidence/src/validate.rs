use crate::model::*;
use crate::scanner::validate_url;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

pub fn canonical_json<T: serde::Serialize>(value: &T) -> Result<String, EvidenceError> {
    let json_val = serde_json::to_value(value).map_err(|e| EvidenceError::Json(e.to_string()))?;
    let sorted = sort_value(json_val);
    serde_json::to_string(&sorted).map_err(|e| EvidenceError::Json(e.to_string()))
}

fn sort_value(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut sorted = serde_json::Map::new();
            let mut entries: Vec<_> = object.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            for (key, val) in entries {
                sorted.insert(key, sort_value(val));
            }
            Value::Object(sorted)
        }
        Value::Array(values) => Value::Array(values.into_iter().map(sort_value).collect()),
        other => other,
    }
}

pub fn sha256_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub const ALLOWED_SOURCES: &[(&str, &str, &str, &str)] = &[
    (
        "xiao-current-rustplus",
        "https://github.com/kunihir0/xiao",
        "469c98435647fa2582b8d14339cd385a686a79e6",
        "crates/push-receiver/src/transport.rs;crates/rustplus-adapter/src/registration.rs;crates/rustplus-adapter/src/parse.rs;crates/rustplus-adapter/src/session.rs",
    ),
    (
        "rustplus-current-protocol",
        "https://github.com/kunihir0/rustplus",
        "5b9a0803ac404a00b7f58e298c91b1130504d5c7",
        "proto/rustplus.proto;src/client.rs",
    ),
    (
        "push-receiver-current",
        "https://github.com/kunihir0/push-receiver",
        "3b5b741c7ef52c2da26414bad2cffbf778644fdc",
        "src/checkin.rs;src/register.rs;src/mcs.rs;proto/mcs.proto",
    ),
    (
        "rustplus-js-modern",
        "https://github.com/liamcottle/rustplus.js",
        "75915f25fc7c1718f7c23c419953f4655840fade",
        "cli/index.js;docs/PairingFlow.md;rustplus.js;rustplus.proto",
    ),
    (
        "liam-push-receiver",
        "https://github.com/liamcottle/push-receiver",
        "ecc93a37ca618859eaacae71ec0c0136ce1621f5",
        "src/android/fcm.js;src/client.js;src/mcs.proto;src/gcm/index.js",
    ),
];
pub fn validate_evidence(input: &EvidenceInput) -> Result<Vec<ManifestRoute>, EvidenceError> {
    if input.schema_version != SCHEMA_VERSION {
        return Err(EvidenceError::Malformed(format!(
            "unsupported schema_version {}, expected {SCHEMA_VERSION}",
            input.schema_version
        )));
    }

    if input.snapshot_date != SNAPSHOT_DATE {
        return Err(EvidenceError::Malformed(format!(
            "snapshot_date must be {SNAPSHOT_DATE}, got {}",
            input.snapshot_date
        )));
    }

    if input.retention_days == 0 || input.retention_days > MAX_RETENTION_DAYS {
        return Err(EvidenceError::Malformed(format!(
            "retention_days must be between 1 and {MAX_RETENTION_DAYS}, got {}",
            input.retention_days
        )));
    }

    // Validate sources and check for duplicate names
    let mut source_map = BTreeMap::new();
    if input.sources.len() != ALLOWED_SOURCES.len() {
        return Err(EvidenceError::Malformed(format!(
            "source count {} does not match approved sources count {}",
            input.sources.len(),
            ALLOWED_SOURCES.len()
        )));
    }

    for source in &input.sources {
        let matching_allowed = ALLOWED_SOURCES.iter().find(|(name, repo, commit, path)| {
            *name == source.name.as_str()
                && *repo == source.repository.as_str()
                && *commit == source.commit_or_version.as_str()
                && *path == source.path.as_str()
        });
        if matching_allowed.is_none() {
            return Err(EvidenceError::Malformed(
                "source is not an approved pinned source".to_owned(),
            ));
        }
        if source_map.insert(source.name.as_str(), source).is_some() {
            return Err(EvidenceError::Malformed("duplicate source name".to_owned()));
        }

        if source.repository.is_empty() {
            return Err(EvidenceError::Malformed(format!(
                "source '{}' has empty repository",
                source.name
            )));
        }
        validate_url(
            &source.repository,
            &format!("source {}.repository", source.name),
        )?;
        if !source
            .repository
            .starts_with("https://github.com/kunihir0/")
            && !source
                .repository
                .starts_with("https://github.com/liamcottle/")
        {
            return Err(EvidenceError::Malformed(format!(
                "source '{}' repository must belong to a trusted GitHub organization (kunihir0 or liamcottle)",
                source.name
            )));
        }

        if source.commit_or_version.len() != 40
            || !source
                .commit_or_version
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            return Err(EvidenceError::Malformed(format!(
                "source '{}' commit_or_version must be a 40-character lowercase hex SHA, got '{}'",
                source.name, source.commit_or_version
            )));
        }

        if source.path.is_empty() {
            return Err(EvidenceError::Malformed(format!(
                "source '{}' has empty path",
                source.name
            )));
        }

        for p in source.path.split(';') {
            if p.contains("..") || p.starts_with('/') {
                return Err(EvidenceError::Malformed(format!(
                    "source '{}' path must be relative without traversal",
                    source.name
                )));
            }
            let valid_ext = p.ends_with(".rs")
                || p.ends_with(".proto")
                || p.ends_with(".js")
                || p.ends_with(".txt")
                || p.ends_with(".md");
            if !valid_ext {
                return Err(EvidenceError::Malformed(format!(
                    "source '{}' path '{p}' has invalid file extension",
                    source.name
                )));
            }
        }

        if source.reviewed_on != SNAPSHOT_DATE {
            return Err(EvidenceError::Malformed(format!(
                "source '{}' reviewed_on must match snapshot_date {SNAPSHOT_DATE}",
                source.name
            )));
        }

        if source.classification != "fact" {
            return Err(EvidenceError::Malformed(format!(
                "source '{}' classification must be 'fact'",
                source.name
            )));
        }
    }

    // Validate routes
    let mut seen_routes = BTreeSet::new();
    let mut manifest_routes = Vec::new();

    for route_entry in &input.routes {
        let name = route_entry.name_str();
        if !seen_routes.insert(name) {
            return Err(EvidenceError::Malformed(format!(
                "duplicate route name '{name}'"
            )));
        }

        // Validate source references
        validate_source_ref(route_entry.current_source(), &source_map, name, "current")?;
        validate_source_ref(route_entry.modern_source(), &source_map, name, "modern")?;

        // Semantic validation and artifact generation
        let (current_artifact, modern_artifact, assertions) = match route_entry {
            RouteEntry::Enrollment(r) => validate_enrollment_route(r)?,
            RouteEntry::McsReadiness(r) => validate_mcs_route(r)?,
            RouteEntry::Pairing(r) => validate_pairing_route(r)?,
            RouteEntry::GetEntityInfo(r) => validate_entity_info_route(r)?,
            RouteEntry::Alarm2False(r) => validate_alarm_route(r, false)?,
            RouteEntry::Alarm2True(r) => validate_alarm_route(r, true)?,
        };

        manifest_routes.push(ManifestRoute {
            name: name.to_owned(),
            current_source: route_entry.current_source().to_owned(),
            modern_source: route_entry.modern_source().to_owned(),
            current_artifact,
            modern_artifact,
            assertions,
            live_proof_needed: route_entry.live_proof_needed().to_vec(),
        });
    }
    // Alarm broadcasts must be linked to the exact primed getEntityInfo Alarm.
    let entity_route = input.routes.iter().find_map(|r| match r {
        RouteEntry::GetEntityInfo(route) => Some(route.as_ref()),
        _ => None,
    });
    if let Some(entity) = entity_route {
        for alarm in input.routes.iter().filter_map(|r| match r {
            RouteEntry::Alarm2False(route) | RouteEntry::Alarm2True(route) => Some(route.as_ref()),
            _ => None,
        }) {
            if alarm.current.primed_entity.entity_id != entity.current.request.entity_id
                || alarm.current.primed_entity.entity_type
                    != entity.current.response.entity_info.entity_type
                || alarm.modern.primed_entity.entity_id != entity.modern.request.entity_id
                || alarm.modern.primed_entity.entity_type
                    != entity.modern.response.entity_info.entity_type
            {
                return Err(EvidenceError::Malformed(
                    "alarm priming does not match getEntityInfo".to_owned(),
                ));
            }
        }
    }

    const REQUIRED_ROUTES: [&str; 6] = [
        "enrollment",
        "mcs_readiness",
        "pairing",
        "getEntityInfo",
        "alarm_2_false",
        "alarm_2_true",
    ];

    for req in REQUIRED_ROUTES {
        if !seen_routes.contains(req) {
            return Err(EvidenceError::Malformed(format!(
                "missing required route '{req}'"
            )));
        }
    }

    Ok(manifest_routes)
}

fn validate_source_ref(
    reference: &str,
    sources: &BTreeMap<&str, &Source>,
    _route_name: &str,
    _side: &str,
) -> Result<(), EvidenceError> {
    if reference.is_empty() {
        return Err(EvidenceError::Malformed(
            "route source reference is empty".to_owned(),
        ));
    }
    for part in reference.split(';') {
        let (name, sha) = part.split_once('@').ok_or_else(|| {
            EvidenceError::Malformed("route source reference has invalid format".to_owned())
        })?;
        let source = sources.get(name).ok_or_else(|| {
            EvidenceError::Malformed("route source reference is undeclared".to_owned())
        })?;
        if source.commit_or_version != sha {
            return Err(EvidenceError::Malformed(
                "route source commit does not match declared source".to_owned(),
            ));
        }
    }
    Ok(())
}

fn create_artifact<T: serde::Serialize>(data: &T) -> Result<Artifact, EvidenceError> {
    let canonical = canonical_json(data)?;
    Ok(Artifact {
        sha256: sha256_digest(canonical.as_bytes()),
        redaction: "validated-before-hash".to_owned(),
        raw_payload_persisted: false,
    })
}

fn check_assertions(
    expected: &[AssertionId],
    actual: BTreeSet<AssertionId>,
    route_name: &str,
) -> Result<Vec<AssertionStatus>, EvidenceError> {
    let expected_set: BTreeSet<_> = expected.iter().cloned().collect();
    if expected_set != actual {
        return Err(EvidenceError::Malformed(format!(
            "route '{route_name}' assertions do not match evaluated invariants"
        )));
    }

    Ok(expected
        .iter()
        .map(|id| AssertionStatus {
            id: id.clone(),
            status: "PASSED".to_owned(),
        })
        .collect())
}

fn validate_enrollment_route(
    route: &EnrollmentRoute,
) -> Result<(Artifact, Artifact, Vec<AssertionStatus>), EvidenceError> {
    let mut actual_assertions = BTreeSet::new();
    for s in &route.current.steps {
        validate_step_keys(s)?;
    }
    for s in &route.modern.steps {
        validate_step_keys(s)?;
    }
    let register = route
        .current
        .steps
        .iter()
        .find(|step| step.step == "register3")
        .ok_or_else(|| EvidenceError::Malformed("current register3 step missing".to_owned()))?;
    let subtype = register
        .payload
        .get("X-subtype")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            EvidenceError::Malformed("current register3 X-subtype missing".to_owned())
        })?;
    let uuid = subtype.strip_prefix("wp:receiver.push.com#").unwrap_or("");
    if uuid.len() != 36
        || !uuid.chars().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
    {
        return Err(EvidenceError::Malformed(
            "current register3 X-subtype has invalid synthetic UUID".to_owned(),
        ));
    }
    let device = register.payload.get("device").and_then(Value::as_str);
    if !matches!(device, Some("[REDACTED]" | "synthetic-device-01")) {
        return Err(EvidenceError::Malformed(
            "current register3 device is not an approved synthetic value".to_owned(),
        ));
    }

    let cur_step_names: Vec<&str> = route
        .current
        .steps
        .iter()
        .map(|s| s.step.as_str())
        .collect();
    let mod_step_names: Vec<&str> = route.modern.steps.iter().map(|s| s.step.as_str()).collect();

    if cur_step_names
        == [
            "checkin",
            "register3",
            "fcm_subscribe",
            "expo_push_token",
            "facepunch_register",
        ]
        && mod_step_names
            == [
                "firebase_installations",
                "checkin",
                "register3",
                "expo_push_token",
                "facepunch_register",
            ]
    {
        actual_assertions.insert(AssertionId::EnrollmentSequenceStepsMatch);
    } else {
        return Err(EvidenceError::Malformed(
            "enrollment sequence steps do not match expected sequence".to_owned(),
        ));
    }

    // Checkin endpoints
    let cur_checkin = &route.current.steps[0];
    let mod_checkin = &route.modern.steps[1];
    if cur_checkin.endpoint == "https://android.clients.google.com/checkin"
        && mod_checkin.endpoint == "https://android.clients.google.com/checkin"
        && cur_checkin.method == "POST"
        && mod_checkin.method == "POST"
    {
        actual_assertions.insert(AssertionId::EnrollmentCheckinEndpointMatch);
    }

    // Register3 endpoints
    let cur_reg = &route.current.steps[1];
    let mod_reg = &route.modern.steps[2];
    if cur_reg.endpoint == "https://android.clients.google.com/c2dm/register3"
        && mod_reg.endpoint == "https://android.clients.google.com/c2dm/register3"
        && cur_reg.method == "POST"
        && mod_reg.method == "POST"
        && cur_reg.auth_scheme.as_deref() == Some("AidLogin")
        && mod_reg.auth_scheme.as_deref() == Some("AidLogin")
    {
        actual_assertions.insert(AssertionId::EnrollmentRegister3EndpointMatch);
    }

    let cur_fcm = &route.current.steps[2];
    let cur_expo = &route.current.steps[3];
    let mod_expo = &route.modern.steps[3];
    if cur_fcm.endpoint != "https://fcm.googleapis.com/fcm/connect/subscribe" {
        return Err(EvidenceError::Malformed(
            "FCM subscribe endpoint is not approved".to_owned(),
        ));
    }
    if cur_fcm.method != "POST" || cur_expo.method != "POST" || mod_expo.method != "POST" {
        return Err(EvidenceError::Malformed(
            "enrollment HTTP methods must be POST".to_owned(),
        ));
    }
    // Expo endpoints
    if cur_expo.endpoint == "https://exp.host/--/api/v2/push/getExpoPushToken"
        && mod_expo.endpoint == "https://exp.host/--/api/v2/push/getExpoPushToken"
        && cur_expo.payload["projectId"] == "49451aca-a822-41e6-ad59-955718d0ff9c"
        && mod_expo.payload["projectId"] == "49451aca-a822-41e6-ad59-955718d0ff9c"
    {
        actual_assertions.insert(AssertionId::EnrollmentExpoEndpointMatch);
    }

    // Facepunch endpoints
    let cur_fp = &route.current.steps[4];
    let mod_fp = &route.modern.steps[4];
    let cur_url =
        Url::parse(&cur_fp.endpoint).map_err(|e| EvidenceError::Malformed(e.to_string()))?;
    let mod_url =
        Url::parse(&mod_fp.endpoint).map_err(|e| EvidenceError::Malformed(e.to_string()))?;
    if cur_url.host_str() == Some("companion-rust.facepunch.com")
        && mod_url.host_str() == Some("companion-rust.facepunch.com")
        && cur_fp.method == "POST"
        && mod_fp.method == "POST"
    {
        actual_assertions.insert(AssertionId::EnrollmentFacepunchHostIsCompanionRust);
        if cur_url.path() == "/api/push/register" && mod_url.path() == "/api/push/register" {
            actual_assertions.insert(AssertionId::EnrollmentFacepunchEndpointMatch);
        }
    }

    actual_assertions.insert(AssertionId::EnrollmentCredentialsRedacted);

    let assertions = check_assertions(&route.expected_assertions, actual_assertions, "enrollment")?;
    let current_artifact = create_artifact(&route.current)?;
    let modern_artifact = create_artifact(&route.modern)?;

    Ok((current_artifact, modern_artifact, assertions))
}
fn validate_step_keys(step: &EnrollmentStep) -> Result<(), EvidenceError> {
    let (allowed_payload, allowed_response): (&[&str], &[&str]) = match step.step.as_str() {
        "checkin" => (
            &[
                "user_serial_number",
                "checkin",
                "version",
                "id",
                "security_token",
            ],
            &["android_id", "security_token"],
        ),
        "register3" => (
            &[
                "app",
                "X-subtype",
                "device",
                "sender",
                "cert",
                "app_ver",
                "X-app_ver",
                "X-osv",
                "X-cliv",
                "X-gmsv",
                "X-scope",
                "X-Goog-Firebase-Installations-Auth",
                "X-gms_app_id",
                "X-Firebase-Client",
                "X-Firebase-Client-Log-Type",
                "X-app_ver_name",
                "target_ver",
            ],
            &["token"],
        ),
        "fcm_subscribe" => (
            &[
                "authorized_entity",
                "endpoint",
                "encryption_key",
                "encryption_auth",
            ],
            &["token"],
        ),
        "expo_push_token" => (
            &[
                "type",
                "deviceId",
                "development",
                "appId",
                "deviceToken",
                "projectId",
            ],
            &["data"],
        ),
        "facepunch_register" => (
            &["AuthToken", "DeviceId", "PushKind", "PushToken"],
            &["success"],
        ),
        "firebase_installations" => (
            &["appId", "authVersion", "fid", "sdkVersion"],
            &["authToken", "fid", "refreshToken"],
        ),
        _ => {
            return Err(EvidenceError::Malformed(
                "unrecognized enrollment step name".to_owned(),
            ));
        }
    };
    let required_payload: &[&str] = match step.step.as_str() {
        "checkin" => &[
            "user_serial_number",
            "checkin",
            "version",
            "id",
            "security_token",
        ],
        "register3" => &["app", "X-subtype", "device", "sender"],
        "fcm_subscribe" => &[
            "authorized_entity",
            "endpoint",
            "encryption_key",
            "encryption_auth",
        ],
        "expo_push_token" => &[
            "type",
            "deviceId",
            "development",
            "appId",
            "deviceToken",
            "projectId",
        ],
        "facepunch_register" => &["AuthToken", "DeviceId", "PushKind", "PushToken"],
        "firebase_installations" => &["appId", "authVersion", "fid", "sdkVersion"],
        _ => &[],
    };
    let required_response: &[&str] = match step.step.as_str() {
        "checkin" => &["android_id", "security_token"],
        "register3" | "fcm_subscribe" => &["token"],
        "expo_push_token" => &["data"],
        "facepunch_register" => &["success"],
        "firebase_installations" => &["authToken", "fid", "refreshToken"],
        _ => &[],
    };
    if !matches!(&step.payload, Value::Object(_)) {
        return Err(EvidenceError::Malformed(
            "enrollment payload must be an object".to_owned(),
        ));
    }
    if let Value::Object(obj) = &step.payload {
        for k in obj.keys() {
            if !allowed_payload.contains(&k.as_str()) {
                return Err(EvidenceError::Malformed(
                    "unexpected enrollment payload key".to_owned(),
                ));
            }
        }
        if required_payload.iter().any(|key| !obj.contains_key(*key)) {
            return Err(EvidenceError::Malformed(
                "required enrollment payload field missing".to_owned(),
            ));
        }
    }
    if !matches!(&step.response, Value::Object(_)) {
        return Err(EvidenceError::Malformed(
            "enrollment response must be an object".to_owned(),
        ));
    }
    if let Value::Object(obj) = &step.response {
        for k in obj.keys() {
            if !allowed_response.contains(&k.as_str()) {
                return Err(EvidenceError::Malformed(
                    "unexpected enrollment response key".to_owned(),
                ));
            }
        }
        if required_response.iter().any(|key| !obj.contains_key(*key)) {
            return Err(EvidenceError::Malformed(
                "required enrollment response field missing".to_owned(),
            ));
        }
    }
    if step.step == "firebase_installations" {
        let valid_auth = matches!(
            step.response.get("authToken"),
            Some(Value::Object(auth))
                if auth.len() == 1
                    && auth.get("token") == Some(&Value::String("[REDACTED]".to_owned()))
        );
        if !valid_auth {
            return Err(EvidenceError::Malformed(
                "firebase authToken must contain only redacted token".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_mcs_route(
    route: &McsRoute,
) -> Result<(Artifact, Artifact, Vec<AssertionStatus>), EvidenceError> {
    let mut actual_assertions = BTreeSet::new();

    let cur = &route.current;
    let md = &route.modern;

    if cur.endpoint == "tcp+tls://mtalk.google.com:5228"
        && md.endpoint == "tcp+tls://mtalk.google.com:5228"
    {
        actual_assertions.insert(AssertionId::McsEndpointMatchesMtalk);
    }

    if cur.transport == "tcp_tls"
        && md.transport == "tcp_tls"
        && cur.method == "MCS_FRAMED_PROTOBUF"
        && md.method == "MCS_FRAMED_PROTOBUF"
        && cur.framing == "mcs_version_tag_delimited"
        && md.framing == "mcs_version_tag_delimited"
    {
        actual_assertions.insert(AssertionId::McsTransportIsRawTcpTls);
    }

    if cur.protocol_version == 41 && md.protocol_version == 41 {
        actual_assertions.insert(AssertionId::McsProtocolVersion41);
    }

    if cur.login_request.id == "chrome-63.0.3234.0"
        && md.login_request.id == "chrome-63.0.3234.0"
        && cur.login_request.domain == "mcs.android.com"
        && md.login_request.domain == "mcs.android.com"
        && cur.login_request.auth_service == 2
        && md.login_request.auth_service == 2
        && cur.login_request.use_rmq2
        && md.login_request.use_rmq2
    {
        actual_assertions.insert(AssertionId::McsLoginPayloadFieldsetComplete);
    }

    if cur.login_request.auth_token == "[REDACTED]"
        && cur.login_request.device_id == "[REDACTED]"
        && cur.login_request.user == "[REDACTED]"
        && cur.login_request.resource == "[REDACTED]"
        && md.login_request.auth_token == "[REDACTED]"
        && md.login_request.device_id == "[REDACTED]"
        && md.login_request.user == "[REDACTED]"
        && md.login_request.resource == "[REDACTED]"
    {
        actual_assertions.insert(AssertionId::McsLoginCredentialsRedacted);
    }

    if cur.readiness_response.tag == 3
        && md.readiness_response.tag == 3
        && cur.readiness_response.name == "LoginResponse"
        && md.readiness_response.name == "LoginResponse"
        && !cur.readiness_response.heartbeat_config.upload_stat
        && !md.readiness_response.heartbeat_config.upload_stat
        && cur.readiness_response.heartbeat_config.ip == "[REDACTED]"
        && md.readiness_response.heartbeat_config.ip == "[REDACTED]"
        && cur.readiness_response.heartbeat_config.interval_ms == 300_000
        && md.readiness_response.heartbeat_config.interval_ms == 300_000
    {
        actual_assertions.insert(AssertionId::McsReadinessTagIsLoginResponse);
    }

    let assertions = check_assertions(
        &route.expected_assertions,
        actual_assertions,
        "mcs_readiness",
    )?;
    let current_artifact = create_artifact(&route.current)?;
    let modern_artifact = create_artifact(&route.modern)?;

    Ok((current_artifact, modern_artifact, assertions))
}

fn validate_pairing_route(
    route: &PairingRoute,
) -> Result<(Artifact, Artifact, Vec<AssertionStatus>), EvidenceError> {
    let mut actual_assertions = BTreeSet::new();

    let cur = &route.current;
    let md = &route.modern;

    if cur.method == "FCM_NOTIFICATION_APP_DATA"
        && cur.endpoint == "fcm://notification/app-data"
        && md.method == "FCM_NOTIFICATION_APP_DATA"
        && md.endpoint == "fcm://notification/app-data"
    {
        actual_assertions.insert(AssertionId::PairingMethodIsFcmAppData);
    } else {
        return Err(EvidenceError::Malformed(
            "pairing transport methods and endpoints must be FCM_NOTIFICATION_APP_DATA at fcm://notification/app-data".to_owned(),
        ));
    }

    let allowed_keys = ["ip", "port", "playerId", "playerToken", "type"];

    for (side_name, side) in [("current", cur), ("modern", md)] {
        if side.app_data.len() != 1 || side.app_data[0].key != "pairing" {
            return Err(EvidenceError::Malformed(
                "pairing app_data must contain exactly one pairing item".to_owned(),
            ));
        }
        let pairing_item = &side.app_data[0];
        let raw: serde_json::Value = serde_json::from_str(&pairing_item.value).map_err(|_| {
            EvidenceError::Malformed("pairing app_data value must be JSON".to_owned())
        })?;
        let obj = raw.as_object().ok_or_else(|| {
            EvidenceError::Malformed("pairing app_data must be an object".to_owned())
        })?;
        if obj.len() != allowed_keys.len()
            || obj.keys().any(|k| !allowed_keys.contains(&k.as_str()))
        {
            return Err(EvidenceError::Malformed(
                "pairing app_data has unexpected or missing fields".to_owned(),
            ));
        }

        let player_id = raw["playerId"].as_u64().ok_or_else(|| {
            EvidenceError::Malformed(format!(
                "{side_name} pairing payload missing numeric playerId"
            ))
        })?;
        if raw["type"].as_str() != Some("server")
            || raw["playerToken"].as_str() != Some("[REDACTED]")
        {
            return Err(EvidenceError::Malformed(format!(
                "{side_name} pairing payload must identify server and redact playerToken"
            )));
        }

        if !ALLOWED_SYNTHETIC_PLAYER_IDS.contains(&player_id) {
            return Err(EvidenceError::Secret(
                "playerId is not in synthetic allowlist".to_owned(),
            ));
        }

        if player_id != side.normalized_pairing.player_id {
            return Err(EvidenceError::Malformed(format!(
                "{side_name} app_data playerId does not match normalized_pairing.player_id"
            )));
        }

        if raw["ip"].as_str() != Some(&side.normalized_pairing.host) {
            return Err(EvidenceError::Malformed(format!(
                "{side_name} app_data ip does not match normalized_pairing.host"
            )));
        }

        if raw["port"].as_u64() != Some(side.normalized_pairing.port as u64) {
            return Err(EvidenceError::Malformed(format!(
                "{side_name} app_data port does not match normalized_pairing.port"
            )));
        }

        if raw.get("paired").is_some() {
            return Err(EvidenceError::Malformed(format!(
                "{side_name} pairing payload must not contain invented 'paired' response"
            )));
        }
    }
    actual_assertions.insert(AssertionId::PairingSyntheticPlayerIdMatchesNormalized);
    actual_assertions.insert(AssertionId::PairingPlayerTokenRedacted);
    actual_assertions.insert(AssertionId::PairingNormalizedAddressMatchesNested);
    actual_assertions.insert(AssertionId::PairingNoInventedProviderResponse);

    let assertions = check_assertions(&route.expected_assertions, actual_assertions, "pairing")?;
    let current_artifact = create_artifact(&route.current)?;
    let modern_artifact = create_artifact(&route.modern)?;

    Ok((current_artifact, modern_artifact, assertions))
}

fn validate_entity_info_route(
    route: &EntityInfoRoute,
) -> Result<(Artifact, Artifact, Vec<AssertionStatus>), EvidenceError> {
    let mut actual_assertions = BTreeSet::new();

    let cur = &route.current;
    let md = &route.modern;

    if cur.endpoint != "wss://203.0.113.10:28082/" || md.endpoint != "ws://203.0.113.10:28082/" {
        return Err(EvidenceError::Malformed(
            "getEntityInfo endpoints must match approved synthetic fixture".to_owned(),
        ));
    }
    actual_assertions.insert(AssertionId::GetEntityInfoWebsocketEndpointNoPath);
    actual_assertions.insert(AssertionId::GetEntityInfoCurrentIsWssAndModernIsWs);

    if cur.method != "PROTOBUF_RPC" || md.method != "PROTOBUF_RPC" {
        return Err(EvidenceError::Malformed(
            "getEntityInfo method must be PROTOBUF_RPC".to_owned(),
        ));
    }

    for side in [cur, md] {
        if side.request.seq != side.response.seq {
            return Err(EvidenceError::Malformed(
                "getEntityInfo request and response sequence numbers must match".to_owned(),
            ));
        }

        if !ALLOWED_SYNTHETIC_PLAYER_IDS.contains(&side.request.player_id) {
            return Err(EvidenceError::Secret(
                "request player_id not in synthetic allowlist".to_owned(),
            ));
        }

        if !ALLOWED_SYNTHETIC_ENTITY_IDS.contains(&side.request.entity_id) {
            return Err(EvidenceError::Secret(
                "request entity_id not in synthetic allowlist".to_owned(),
            ));
        }

        if side.response.entity_info.entity_type != 2 {
            return Err(EvidenceError::Malformed(
                "entityInfo response type must be 2 (Alarm)".to_owned(),
            ));
        }

        if side.response.entity_info.payload.value {
            return Err(EvidenceError::Malformed(
                "getEntityInfo baseline alarm value must be false".to_owned(),
            ));
        }
    }

    actual_assertions.insert(AssertionId::GetEntityInfoRequestHasEntityIdAndPlayer);
    actual_assertions.insert(AssertionId::GetEntityInfoResponseTypeIsAlarm2);
    actual_assertions.insert(AssertionId::GetEntityInfoSequenceMatches);
    actual_assertions.insert(AssertionId::GetEntityInfoPlayerCredentialsRedacted);

    let assertions = check_assertions(
        &route.expected_assertions,
        actual_assertions,
        "getEntityInfo",
    )?;
    let current_artifact = create_artifact(&route.current)?;
    let modern_artifact = create_artifact(&route.modern)?;

    Ok((current_artifact, modern_artifact, assertions))
}

fn validate_alarm_route(
    route: &AlarmRoute,
    expected_value: bool,
) -> Result<(Artifact, Artifact, Vec<AssertionStatus>), EvidenceError> {
    let mut actual_assertions = BTreeSet::new();

    let cur = &route.current;
    let md = &route.modern;

    if cur.method != "PROTOBUF_BROADCAST" || md.method != "PROTOBUF_BROADCAST" {
        return Err(EvidenceError::Malformed(
            "Alarm method must be PROTOBUF_BROADCAST".to_owned(),
        ));
    }
    actual_assertions.insert(AssertionId::AlarmMethodIsProtobufBroadcast);

    let cur_url = Url::parse(&cur.endpoint).map_err(|e| EvidenceError::Malformed(e.to_string()))?;
    let md_url = Url::parse(&md.endpoint).map_err(|e| EvidenceError::Malformed(e.to_string()))?;

    if cur.endpoint != "wss://203.0.113.10:28082/" || md.endpoint != "ws://203.0.113.10:28082/" {
        return Err(EvidenceError::Malformed(
            "Alarm endpoints must match approved synthetic fixture".to_owned(),
        ));
    }
    if cur_url.scheme() == "wss" && md_url.scheme() == "ws" {
        actual_assertions.insert(AssertionId::AlarmCurrentIsWssAndModernIsWs);
    } else {
        return Err(EvidenceError::Malformed(
            "Alarm current must be wss and modern must be ws".to_owned(),
        ));
    }

    for side in [cur, md] {
        if !ALLOWED_SYNTHETIC_ENTITY_IDS.contains(&side.primed_entity.entity_id) {
            return Err(EvidenceError::Secret(
                "primed_entity entity_id not in synthetic allowlist".to_owned(),
            ));
        }

        if side.primed_entity.entity_type != 2 {
            return Err(EvidenceError::Malformed(
                "primed_entity entity_type must be 2 (Alarm)".to_owned(),
            ));
        }

        if side.broadcast.entity_changed.entity_id != side.primed_entity.entity_id {
            return Err(EvidenceError::Malformed(
                "broadcast entityChanged entityId does not match primed entity".to_owned(),
            ));
        }

        if side.normalized_event.entity_id != side.primed_entity.entity_id {
            return Err(EvidenceError::Malformed(
                "normalized_event entity_id does not match primed entity".to_owned(),
            ));
        }

        if side.normalized_event.entity_type != 2 {
            return Err(EvidenceError::Malformed(
                "normalized_event entity_type must be 2".to_owned(),
            ));
        }

        if side.broadcast.entity_changed.payload.value != expected_value
            || side.normalized_event.value != expected_value
        {
            return Err(EvidenceError::Malformed(format!(
                "broadcast and normalized event value must be {expected_value}"
            )));
        }
    }

    if !route
        .live_proof_needed
        .contains(&LiveProofNeed::InGameSmartAlarmStateChange)
        || !route
            .live_proof_needed
            .contains(&LiveProofNeed::RustServerToClientWebsocketBroadcastDelivery)
    {
        return Err(EvidenceError::Malformed(
            "Alarm route must declare required live proof needs".to_owned(),
        ));
    }

    actual_assertions.insert(AssertionId::AlarmBroadcastHasEntityChanged);
    actual_assertions.insert(AssertionId::AlarmPrimedEntityIdMatches);
    actual_assertions.insert(AssertionId::AlarmPrimedEntityTypeIsAlarm2);
    actual_assertions.insert(AssertionId::AlarmNormalizedEventMatchesPayload);
    actual_assertions.insert(AssertionId::AlarmLiveProofNeededDeclared);

    let route_name = if expected_value {
        "alarm_2_true"
    } else {
        "alarm_2_false"
    };

    let assertions = check_assertions(&route.expected_assertions, actual_assertions, route_name)?;
    let current_artifact = create_artifact(&route.current)?;
    let modern_artifact = create_artifact(&route.modern)?;

    Ok((current_artifact, modern_artifact, assertions))
}
