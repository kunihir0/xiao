pub mod model;
pub mod scanner;
pub mod validate;

pub use model::{
    Artifact, AssertionId, AssertionStatus, EvidenceError, EvidenceInput, LiveProofNeed, Manifest,
    ManifestRoute, RouteEntry, Source,
};
pub use scanner::{MAX_INPUT_BYTES, MAX_NESTING_DEPTH, REDACTED_MARKER};
pub use validate::{canonical_json, sha256_digest, validate_evidence};

use serde_json::Value;
use std::collections::BTreeSet;

pub const EMBEDDED_FIXTURE: &str = include_str!("../fixtures/evidence.json");

pub fn render_embedded() -> Result<String, EvidenceError> {
    render(EMBEDDED_FIXTURE)
}

pub fn render(input: &str) -> Result<String, EvidenceError> {
    scanner::validate_raw_limits(input)?;

    let raw_val: Value =
        serde_json::from_str(input).map_err(|e| EvidenceError::Json(e.to_string()))?;

    let mut known_hashes = BTreeSet::new();
    if let Some(sources) = raw_val.get("sources").and_then(Value::as_array) {
        for src in sources {
            if let Some(commit) = src.get("commit_or_version").and_then(Value::as_str) {
                known_hashes.insert(commit);
            }
        }
    }

    scanner::scan_value(&raw_val, 0, "root", &known_hashes)?;

    let parsed: EvidenceInput =
        serde_json::from_value(raw_val).map_err(|e| EvidenceError::Json(e.to_string()))?;

    let routes = validate::validate_evidence(&parsed)?;

    let mut manifest = Manifest {
        schema_version: parsed.schema_version,
        snapshot_date: parsed.snapshot_date,
        offline: true,
        sources: parsed.sources,
        routes,
        classification: model::Classification {
            facts: vec![
                "all source references are pinned to exact repository commit hashes and file paths".to_owned(),
                "current and modern request/response contracts and protobuf schemas are derived from pinned source files".to_owned(),
                "all secrets in request/response payloads are redacted to [REDACTED]".to_owned(),
                "artifact hashes are SHA-256 checksums over canonical redacted JSON".to_owned(),
            ],
            inferences: vec![
                "normalized application event shapes represent internal adapter projections".to_owned(),
                "modern pairing notification delivery via FCM app-data is an inference based on @liamcottle/push-receiver client event handling for notifications registered via Expo".to_owned(),
                "WebSocket endpoints (wss://203.0.113.10:28082/) in getEntityInfo and Alarm fixtures are synthetic test endpoints using RFC 5737 documentation addresses; production endpoints use ws:// or Facepunch proxy wss://companion-rust.facepunch.com/game/{server}/{port}".to_owned(),
                "modern route implementations may change in subsequent upstream releases".to_owned(),
            ],
            live_proof: Vec::new(),
        },
        retention: model::Retention {
            generated_artifact_max_days: parsed.retention_days,
            checked_in_fixture_retention: "indefinite in Git history".to_owned(),
            raw_payloads_persisted: false,
            manifest_retained: true,
            redaction_before_hash: true,
            checksum_not_authenticity: true,
        },
        manifest_sha256: String::new(),
    };

    let without_hash = canonical_json(&manifest)?;
    manifest.manifest_sha256 = sha256_digest(without_hash.as_bytes());
    canonical_json(&manifest)
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
    fn known_answer_manifest_and_route_hashes() {
        let output = render_embedded().expect("render");
        let manifest: Manifest = serde_json::from_str(&output).expect("manifest json");
        assert_eq!(manifest.routes.len(), 6);
        assert_eq!(
            manifest.manifest_sha256,
            "1100634186bd81ec9a9ade839a875d9d48f90c64a01edd19a8f105c7274a0081"
        );

        let expected_hashes = [
            (
                "enrollment",
                "ce6486b2e73092365d466ba4fa743b99a0f086e31267ce4457d3e932a13fa6f7",
                "f3b38278e87205f0e34e881eb311963b373f8be8e5590844803cad3fae0cc6b6",
            ),
            (
                "mcs_readiness",
                "c895d25113399e9a36002f6e8ad9bf8555086928761db5a2ff3613b0d5391a88",
                "c895d25113399e9a36002f6e8ad9bf8555086928761db5a2ff3613b0d5391a88",
            ),
            (
                "pairing",
                "f3469e22582638a18d7911aee3b67c080db6d9e6d6cd3673a3a978ece7aaa93d",
                "f3469e22582638a18d7911aee3b67c080db6d9e6d6cd3673a3a978ece7aaa93d",
            ),
            (
                "getEntityInfo",
                "ce0ff7782111194d1ca065bef3acdcf06419b14d62809d11eb7b9b64d4f1ce64",
                "5c206e6ad5d11dfdbf03501b283e036c4dca28893ecac1d3b934216293a7e780",
            ),
            (
                "alarm_2_false",
                "4ad577a41e679feafe01e85d59b32d95c87f599476c0d07f3cc0e8424c072389",
                "1fc510e03d40cc888f2cd917343a841ff0b88a84ecbb2ae98566fee8ed0f7b9a",
            ),
            (
                "alarm_2_true",
                "368d98f37f49e20094bcf429b6190d6b9fa0491e903b165c6d52c53d87de2bfc",
                "707f183fc04816e8f91e5b2511e76c73ed3ab54aeeccbbfc19f46942b91e9918",
            ),
        ];

        for (idx, (expected_name, expected_cur, expected_mod)) in expected_hashes.iter().enumerate()
        {
            let route = &manifest.routes[idx];
            assert_eq!(route.name, *expected_name);
            assert_eq!(route.current_artifact.sha256, *expected_cur);
            assert_eq!(route.modern_artifact.sha256, *expected_mod);
            assert_eq!(route.current_artifact.redaction, "validated-before-hash");
            assert_eq!(route.modern_artifact.redaction, "validated-before-hash");
            assert!(!route.current_artifact.raw_payload_persisted);
            assert!(!route.modern_artifact.raw_payload_persisted);
            for a in &route.assertions {
                assert_eq!(a.status, "PASSED");
            }
        }
    }

    #[test]
    fn canonicalization_ignores_object_key_order() {
        let value: Value = serde_json::from_str(EMBEDDED_FIXTURE).expect("fixture JSON");
        let reordered = serde_json::to_string(&value).expect("serialized JSON");
        let first = render(EMBEDDED_FIXTURE).expect("fixture render");
        let second = render(&reordered).expect("reordered render");
        assert_eq!(first, second);
    }

    #[test]
    fn semantic_mutations_fail_closed_for_each_route() {
        // Route 0: enrollment sequence altered
        let mut v: Value = serde_json::from_str(EMBEDDED_FIXTURE).expect("fixture");
        v["routes"][0]["current"]["steps"][0]["step"] = Value::String("wrong".into());
        assert!(render(&v.to_string()).is_err());

        // Route 1: mcs protocol version altered
        let mut v: Value = serde_json::from_str(EMBEDDED_FIXTURE).expect("fixture");
        v["routes"][1]["current"]["protocol_version"] = Value::from(42);
        assert!(render(&v.to_string()).is_err());

        // Route 2: pairing player ID mismatch
        let mut v: Value = serde_json::from_str(EMBEDDED_FIXTURE).expect("fixture");
        v["routes"][2]["current"]["normalized_pairing"]["player_id"] = Value::from(9999);
        assert!(render(&v.to_string()).is_err());

        // Route 3: getEntityInfo sequence mismatch
        let mut v: Value = serde_json::from_str(EMBEDDED_FIXTURE).expect("fixture");
        v["routes"][3]["current"]["response"]["seq"] = Value::from(999);
        assert!(render(&v.to_string()).is_err());

        // Route 4: alarm_2_false broadcast value altered
        let mut v: Value = serde_json::from_str(EMBEDDED_FIXTURE).expect("fixture");
        v["routes"][4]["current"]["broadcast"]["entity_changed"]["payload"]["value"] =
            Value::Bool(true);
        assert!(render(&v.to_string()).is_err());

        // Route 5: alarm_2_true broadcast value altered
        let mut v: Value = serde_json::from_str(EMBEDDED_FIXTURE).expect("fixture");
        v["routes"][5]["current"]["broadcast"]["entity_changed"]["payload"]["value"] =
            Value::Bool(false);
        assert!(render(&v.to_string()).is_err());
    }

    #[test]
    fn secret_scanner_catches_all_forbidden_patterns() {
        let test_cases = vec![
            (
                "token=",
                "routes[0].expected_assertions[0]",
                "token=secret123",
            ),
            ("secret=", "routes[0].expected_assertions[0]", "secret=123"),
            (
                "api_key=",
                "routes[0].expected_assertions[0]",
                "api_key=xyz",
            ),
            (
                "password=",
                "routes[0].expected_assertions[0]",
                "password=abc",
            ),
            (
                "PEM",
                "routes[0].expected_assertions[0]",
                "-----BEGIN PRIVATE KEY-----",
            ),
            (
                "Bearer",
                "routes[0].expected_assertions[0]",
                "Bearer some-token",
            ),
            (
                "URL userinfo",
                "routes[0].current.steps[0].endpoint",
                "https://user:pass@google.com/checkin",
            ),
            (
                "URL query",
                "routes[0].current.steps[0].endpoint",
                "https://google.com/checkin?key=1",
            ),
            (
                "URL fragment",
                "routes[0].current.steps[0].endpoint",
                "https://google.com/checkin#fragment",
            ),
            (
                "URL empty host",
                "routes[0].current.steps[0].endpoint",
                "https:///checkin",
            ),
            (
                "Production SteamID string",
                "routes[0].expected_assertions[0]",
                "76561198000000001",
            ),
            (
                "API key prefix",
                "routes[0].expected_assertions[0]",
                "AIzaSyB5y2yTzqb4",
            ),
            (
                "Uppercase unredacted token",
                "routes[0].expected_assertions[0]",
                "SUPERSECRETUPPERCASETOKEN123",
            ),
        ];

        for (desc, path, injected) in test_cases {
            let mut v: Value = serde_json::from_str(EMBEDDED_FIXTURE).expect("fixture");
            let mut ptr = String::new();
            for seg in path.split('.') {
                if let Some((arr, idx)) = seg.split_once('[') {
                    let idx_str = idx.trim_end_matches(']');
                    ptr.push('/');
                    ptr.push_str(arr);
                    ptr.push('/');
                    ptr.push_str(idx_str);
                } else {
                    ptr.push('/');
                    ptr.push_str(seg);
                }
            }

            *v.pointer_mut(&ptr).expect(desc) = Value::String(injected.to_string());
            let err = render(&v.to_string()).expect_err(desc);
            assert!(
                matches!(err, EvidenceError::Secret(_) | EvidenceError::Malformed(_)),
                "Case {desc} failed to error as Secret or Malformed: {err:?}"
            );
        }
    }

    #[test]
    fn unredacted_nested_json_secret_fails() {
        let mut v: Value = serde_json::from_str(EMBEDDED_FIXTURE).expect("fixture");
        v["routes"][2]["current"]["app_data"][0]["value"] = Value::String(
            r#"{"ip":"203.0.113.10","port":28082,"playerId":1001,"playerToken":"plaintext_token","type":"server"}"#.to_string(),
        );
        let err = render(&v.to_string()).expect_err("nested secret must fail");
        assert!(matches!(err, EvidenceError::Secret(_)));
    }

    #[test]
    fn production_steam_id_number_fails() {
        let mut v: Value = serde_json::from_str(EMBEDDED_FIXTURE).expect("fixture");
        v["routes"][3]["current"]["request"]["player_id"] = Value::from(76561198000000001u64);
        let err = render(&v.to_string()).expect_err("steam id number must fail");
        assert!(matches!(err, EvidenceError::Secret(_)));
    }

    #[test]
    fn duplicate_source_name_fails() {
        let mut v: Value = serde_json::from_str(EMBEDDED_FIXTURE).expect("fixture");
        let duplicate = v["sources"][0].clone();
        v["sources"]
            .as_array_mut()
            .expect("sources array")
            .push(duplicate);
        let err = render(&v.to_string()).expect_err("duplicate source must fail");
        assert!(matches!(err, EvidenceError::Malformed(_)));
    }

    #[test]
    fn source_commit_mismatch_fails() {
        let mut v: Value = serde_json::from_str(EMBEDDED_FIXTURE).expect("fixture");
        v["routes"][0]["current_source"] = Value::String(
            "xiao-current-rustplus@0000000000000000000000000000000000000000".to_string(),
        );
        let err = render(&v.to_string()).expect_err("commit mismatch must fail");
        assert!(matches!(err, EvidenceError::Malformed(_)));
    }

    #[test]
    fn oversized_input_fails() {
        let oversized = " ".repeat(MAX_INPUT_BYTES + 1);
        let err = render(&oversized).expect_err("oversized must fail");
        assert!(matches!(err, EvidenceError::LimitExceeded(_)));
    }

    #[test]
    fn deeply_nested_input_fails() {
        let deep = format!("{}{}", "{\"a\":".repeat(40), "1}".repeat(40));
        let err = render(&deep).expect_err("deeply nested must fail");
        assert!(matches!(err, EvidenceError::LimitExceeded(_)));
    }

    #[test]
    fn untyped_unknown_payload_key_fails() {
        let mut v: Value = serde_json::from_str(EMBEDDED_FIXTURE).expect("fixture");
        v["routes"][0]["current"]["steps"][0]["payload"]["unexpected_blob"] =
            Value::String("some_value".to_string());
        let err = render(&v.to_string()).expect_err("unknown payload key must fail");
        assert!(matches!(err, EvidenceError::Malformed(_)));
    }

    #[test]
    fn random_credential_shaped_value_fails() {
        let mut v: Value = serde_json::from_str(EMBEDDED_FIXTURE).expect("fixture");
        v["routes"][0]["current"]["steps"][3]["payload"]["deviceId"] =
            Value::String("dGVzdF9zZWNyZXRfdmFsdWUxMjM0NQ==".to_string());
        let err = render(&v.to_string()).expect_err("credential shaped string must fail");
        assert!(matches!(err, EvidenceError::Secret(_)));
    }

    #[test]
    fn untrusted_source_repository_fails() {
        let mut v: Value = serde_json::from_str(EMBEDDED_FIXTURE).expect("fixture");
        v["sources"][0]["repository"] = Value::String("https://github.com/evil/repo".to_string());
        let err = render(&v.to_string()).expect_err("untrusted repo must fail");
        assert!(matches!(err, EvidenceError::Malformed(_)));
    }
}
