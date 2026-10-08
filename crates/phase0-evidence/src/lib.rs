//! Deterministic, offline, redacted Phase 0 Rust+ contract evidence.
//!
//! This crate only consumes the checked-in fixture below. It never reads process
//! environment, filesystem, keyrings, databases, network, or provider state.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fmt};

const FIXTURE: &str = include_str!("../fixtures/evidence.json");
const REDACTED: &str = "[REDACTED]";
const REQUIRED_ROUTES: [&str; 6] = [
    "enrollment",
    "mcs_readiness",
    "pairing",
    "getEntityInfo",
    "alarm_2_false",
    "alarm_2_true",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceError {
    Json(String),
    Malformed(String),
    Secret(String),
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(message) => write!(f, "invalid evidence JSON: {message}"),
            Self::Malformed(message) => write!(f, "malformed evidence: {message}"),
            Self::Secret(message) => write!(f, "unredacted evidence: {message}"),
        }
    }
}

impl std::error::Error for EvidenceError {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    schema_version: u32,
    snapshot_date: String,
    retention_days: u32,
    sources: Vec<Source>,
    routes: Vec<Route>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Source {
    name: String,
    repository: String,
    commit_or_version: String,
    reviewed_on: String,
    classification: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Route {
    name: String,
    current_source: String,
    modern_source: String,
    current: Contract,
    modern: Contract,
    expected_assertions: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Contract {
    endpoint: String,
    method: String,
    headers: Map<String, Value>,
    payload: Value,
    response: Value,
}

#[derive(Debug, Serialize)]
struct Manifest {
    schema_version: u32,
    snapshot_date: String,
    offline: bool,
    sources: Vec<Source>,
    routes: Vec<ManifestRoute>,
    classification: Classification,
    retention: Retention,
    manifest_sha256: String,
}

#[derive(Debug, Serialize)]
struct ManifestRoute {
    name: String,
    current_source: String,
    modern_source: String,
    current: Artifact,
    modern: Artifact,
    expected_assertions: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Artifact {
    sha256: String,
    redaction: &'static str,
    raw_payload_persisted: bool,
}

#[derive(Debug, Serialize)]
struct Classification {
    facts: Vec<&'static str>,
    inferences: Vec<&'static str>,
    live_proof: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct Retention {
    max_days: u32,
    raw_payloads_persisted: bool,
    manifest_retained: bool,
    redaction_before_hash: bool,
}

/// Render the checked-in Phase 0 evidence manifest.
pub fn render_embedded() -> Result<String, EvidenceError> {
    render(FIXTURE)
}

/// Parse, validate, redact-check, hash, and render an evidence input.
///
/// The input is expected to already contain redacted fixtures. This function
/// never writes a raw fixture or attempts to redact one after the fact.
pub fn render(input: &str) -> Result<String, EvidenceError> {
    let parsed: Input =
        serde_json::from_str(input).map_err(|error| EvidenceError::Json(error.to_string()))?;
    validate_input(&parsed)?;

    let mut routes = Vec::with_capacity(parsed.routes.len());
    for route in parsed.routes {
        let current = artifact(&route.current)?;
        let modern = artifact(&route.modern)?;
        routes.push(ManifestRoute {
            name: route.name,
            current_source: route.current_source,
            modern_source: route.modern_source,
            current,
            modern,
            expected_assertions: route.expected_assertions,
        });
    }

    let mut manifest = Manifest {
        schema_version: parsed.schema_version,
        snapshot_date: parsed.snapshot_date,
        offline: true,
        sources: parsed.sources,
        routes,
        classification: Classification {
            facts: vec![
                "fixture source references and route contracts are pinned in this repository",
                "current and modern payloads passed structural and redaction validation",
                "artifact hashes are SHA-256 over canonical redacted JSON",
            ],
            inferences: vec![
                "modern route shape may differ from the legacy current route",
                "live provider acceptance and delivery remain unverified",
            ],
            live_proof: Vec::new(),
        },
        retention: Retention {
            max_days: parsed.retention_days,
            raw_payloads_persisted: false,
            manifest_retained: true,
            redaction_before_hash: true,
        },
        manifest_sha256: String::new(),
    };
    let without_hash = canonical_json(&manifest)?;
    manifest.manifest_sha256 = sha256(without_hash.as_bytes());
    canonical_json(&manifest)
}

fn validate_input(input: &Input) -> Result<(), EvidenceError> {
    if input.schema_version != 1 {
        return Err(EvidenceError::Malformed(
            "unsupported schema_version".to_owned(),
        ));
    }
    if input.snapshot_date != "2026-10-07" {
        return Err(EvidenceError::Malformed(
            "snapshot_date must be the pinned review date".to_owned(),
        ));
    }
    if input.retention_days == 0 || input.retention_days > 365 {
        return Err(EvidenceError::Malformed(
            "retention_days must be between 1 and 365".to_owned(),
        ));
    }
    if input.sources.is_empty() {
        return Err(EvidenceError::Malformed(
            "at least one source is required".to_owned(),
        ));
    }
    for source in &input.sources {
        if source.repository.is_empty()
            || source.commit_or_version.is_empty()
            || source.reviewed_on != input.snapshot_date
            || source.classification != "fact"
        {
            return Err(EvidenceError::Malformed(format!(
                "invalid source {}",
                source.name
            )));
        }
        scan_value(
            &serde_json::to_value(source).map_err(|e| EvidenceError::Json(e.to_string()))?,
            "source",
        )?;
    }

    let names: BTreeSet<_> = input
        .routes
        .iter()
        .map(|route| route.name.as_str())
        .collect();
    if names.len() != input.routes.len() {
        return Err(EvidenceError::Malformed(
            "route names must be unique".to_owned(),
        ));
    }
    for required in REQUIRED_ROUTES {
        if !names.contains(required) {
            return Err(EvidenceError::Malformed(format!(
                "missing route {required}"
            )));
        }
    }
    for route in &input.routes {
        if route.name.is_empty()
            || route.current_source.is_empty()
            || route.modern_source.is_empty()
            || route.expected_assertions.is_empty()
        {
            return Err(EvidenceError::Malformed(
                "route names, provenance, and assertions are required".to_owned(),
            ));
        }
        scan_value(
            &Value::String(route.current_source.clone()),
            &format!("{}.current_source", route.name),
        )?;
        scan_value(
            &Value::String(route.modern_source.clone()),
            &format!("{}.modern_source", route.name),
        )?;
        for assertion in &route.expected_assertions {
            scan_value(
                &Value::String(assertion.clone()),
                &format!("{}.expected_assertion", route.name),
            )?;
        }
        validate_contract(&route.current, &route.name)?;
        validate_contract(&route.modern, &route.name)?;
        validate_route_semantics(route)?;
    }
    Ok(())
}

fn validate_contract(contract: &Contract, route: &str) -> Result<(), EvidenceError> {
    if !matches!(
        contract.method.as_str(),
        "GET" | "POST" | "PUT" | "TCP+TLS" | "APP_DATA" | "PROTOBUF"
    ) || !valid_endpoint(&contract.endpoint)
    {
        return Err(EvidenceError::Malformed(format!(
            "invalid endpoint or method for {route}"
        )));
    }
    if contract.headers.is_empty() || !contract.headers.contains_key("content-type") {
        return Err(EvidenceError::Malformed(format!(
            "headers missing content-type for {route}"
        )));
    }
    if contract.payload.is_null() || contract.response.is_null() {
        return Err(EvidenceError::Malformed(format!(
            "payload and response are required for {route}"
        )));
    }
    scan_value(&Value::Object(contract.headers.clone()), route)?;
    scan_value(&contract.payload, route)?;
    scan_value(&contract.response, route)?;
    Ok(())
}

fn valid_endpoint(endpoint: &str) -> bool {
    !endpoint.is_empty()
        && endpoint.chars().all(|character| !character.is_control())
        && !endpoint.contains(['?', '#', '@', ' '])
        && (endpoint.starts_with("https://")
            || endpoint.starts_with("wss://")
            || endpoint.starts_with("tcp+tls://")
            || endpoint.starts_with("fcm://"))
        && endpoint
            .split_once("://")
            .is_some_and(|(_, authority)| !authority.is_empty())
}

fn validate_route_semantics(route: &Route) -> Result<(), EvidenceError> {
    let current = &route.current;
    let modern = &route.modern;
    match route.name.as_str() {
        "enrollment" => {
            if !current.endpoint.ends_with("/c2dm/register3")
                || current.method != "POST"
                || !modern
                    .endpoint
                    .contains("firebaseinstallations.googleapis.com")
            {
                return Err(EvidenceError::Malformed(
                    "enrollment route shape is not source-grounded".to_owned(),
                ));
            }
        }
        "mcs_readiness" => {
            for contract in [current, modern] {
                if contract.endpoint != "tcp+tls://mtalk.google.com:5228"
                    || contract.method != "TCP+TLS"
                    || contract.payload["protocol_version"] != 41
                    || contract.response["ready"] != true
                {
                    return Err(EvidenceError::Malformed(
                        "MCS readiness contract invariant failed".to_owned(),
                    ));
                }
            }
        }
        "pairing" => {
            for contract in [current, modern] {
                if contract.endpoint != "fcm://notification/app-data"
                    || contract.method != "APP_DATA"
                    || contract.payload["key"] != "pairing"
                    || contract.response["paired"] != true
                {
                    return Err(EvidenceError::Malformed(
                        "pairing notification contract invariant failed".to_owned(),
                    ));
                }
            }
        }
        "getEntityInfo" => {
            for contract in [current, modern] {
                if contract.payload["getEntityInfo"] != Value::Object(Map::new())
                    || contract.response["entityInfo"]["type"] != 2
                    || contract.response["entityInfo"]["payload"]["value"] != false
                {
                    return Err(EvidenceError::Malformed(
                        "getEntityInfo Alarm baseline invariant failed".to_owned(),
                    ));
                }
            }
        }
        "alarm_2_false" | "alarm_2_true" => {
            let expected = route.name == "alarm_2_true";
            for contract in [current, modern] {
                if contract.payload["setEntityValue"]["value"] != expected
                    || contract.response["broadcast"]["entityChanged"]["payload"]["value"]
                        != expected
                {
                    return Err(EvidenceError::Malformed(
                        "Alarm=2 transition invariant failed".to_owned(),
                    ));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn artifact(contract: &Contract) -> Result<Artifact, EvidenceError> {
    let value =
        serde_json::to_value(contract).map_err(|error| EvidenceError::Json(error.to_string()))?;
    Ok(Artifact {
        sha256: sha256(canonical_json(&value)?.as_bytes()),
        redaction: "validated-before-hash",
        raw_payload_persisted: false,
    })
}

fn scan_value(value: &Value, path: &str) -> Result<(), EvidenceError> {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
                let sensitive_key = [
                    "token",
                    "password",
                    "cookie",
                    "authorization",
                    "privatekey",
                    "bearer",
                    "credential",
                ]
                .iter()
                .any(|needle| normalized.contains(needle));
                if sensitive_key && child != REDACTED {
                    return Err(EvidenceError::Secret(format!(
                        "{path}.{key} is not redacted"
                    )));
                }
                scan_value(child, &format!("{path}.{key}"))?;
            }
        }
        Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                scan_value(child, &format!("{path}[{index}]"))?;
            }
        }
        Value::String(text) => {
            let lower = text.to_ascii_lowercase();
            if text.contains("-----BEGIN")
                || lower.contains("bearer ")
                || lower.contains("session=")
                || lower.contains("password=")
                || lower.contains("authorization:")
            {
                return Err(EvidenceError::Secret(path.to_owned()));
            }
            if let Ok(nested) = serde_json::from_str::<Value>(text) {
                scan_value(&nested, &format!("{path}.json"))?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn canonical_json(value: &impl Serialize) -> Result<String, EvidenceError> {
    let value =
        serde_json::to_value(value).map_err(|error| EvidenceError::Json(error.to_string()))?;
    let sorted = sort_value(value);
    serde_json::to_string(&sorted).map_err(|error| EvidenceError::Json(error.to_string()))
}

fn sort_value(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut sorted = Map::new();
            let mut entries: Vec<_> = object.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            for (key, value) in entries {
                sorted.insert(key, sort_value(value));
            }
            Value::Object(sorted)
        }
        Value::Array(values) => Value::Array(values.into_iter().map(sort_value).collect()),
        other => other,
    }
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifest_is_parseable_and_offline() {
        let output = render_embedded().expect("checked-in fixture must render");
        assert!(output.contains("\"offline\":true"));
        assert!(output.contains("\"live_proof\":[]"));
        assert_eq!(output, render_embedded().expect("second render must match"));
    }

    #[test]
    fn malformed_fixture_fails_closed() {
        let mut value: Value = serde_json::from_str(FIXTURE).expect("fixture JSON");
        value["routes"][0]["current"]["payload"] = Value::Null;
        let error = render(&value.to_string()).expect_err("null payload must fail");
        assert!(matches!(error, EvidenceError::Malformed(_)));
    }

    #[test]
    fn unredacted_secret_fails_closed() {
        let mut value: Value = serde_json::from_str(FIXTURE).expect("fixture JSON");
        value["routes"][0]["current"]["headers"]["authorization"] =
            Value::String("Bearer synthetic-secret".to_owned());
        let error = render(&value.to_string()).expect_err("authorization must be redacted");
        assert!(matches!(error, EvidenceError::Secret(_)));
    }

    #[test]
    fn provenance_and_assertions_are_scanned_for_secrets() {
        let mut value: Value = serde_json::from_str(FIXTURE).expect("fixture JSON");
        value["routes"][0]["current_source"] = Value::String("Bearer synthetic-secret".to_owned());
        let error = render(&value.to_string()).expect_err("provenance must be scanned");
        assert!(matches!(error, EvidenceError::Secret(_)));

        value["routes"][0]["current_source"] = Value::String("current@synthetic".to_owned());
        value["routes"][0]["expected_assertions"][0] =
            Value::String("password=synthetic-secret".to_owned());
        let error = render(&value.to_string()).expect_err("assertions must be scanned");
        assert!(matches!(error, EvidenceError::Secret(_)));
    }

    #[test]
    fn hash_changes_for_fixture_changes() {
        let first = render_embedded().expect("fixture must render");
        let mut value: Value = serde_json::from_str(FIXTURE).expect("fixture JSON");
        value["routes"][0]["expected_assertions"][0] = Value::String("changed".to_owned());
        let second = render(&value.to_string()).expect("changed assertion still valid");
        assert_ne!(first, second);
    }

    #[test]
    fn parser_rejects_unknown_fields() {
        let value = format!("{}\n", FIXTURE.trim_end()).replacen('{', "{\"unexpected\":true,", 1);
        let error = render(&value).expect_err("unknown field must fail");
        assert!(matches!(error, EvidenceError::Json(_)));
    }
}
