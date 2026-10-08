#![allow(clippy::expect_used)]

use std::fs;
use std::process::Command;

const EMBEDDED_FIXTURE: &str = include_str!("../fixtures/evidence.json");

#[test]
fn cli_default_emits_valid_manifest() {
    let output = Command::new(env!("CARGO_BIN_EXE_phase0-evidence"))
        .output()
        .expect("run binary");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert!(stdout.contains("\"offline\":true"));
    assert!(stdout.contains("\"live_proof\":[]"));
}

#[test]
fn cli_input_file_works() {
    let tmp_path = format!("{}/valid_fixture.json", env!("CARGO_TARGET_TMPDIR"));
    fs::write(&tmp_path, EMBEDDED_FIXTURE).expect("write temp fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_phase0-evidence"))
        .arg("--input")
        .arg(&tmp_path)
        .output()
        .expect("run binary with --input");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert!(stdout.contains("\"offline\":true"));
}

#[test]
fn cli_rejects_sensitive_files() {
    let output = Command::new(env!("CARGO_BIN_EXE_phase0-evidence"))
        .arg("--input")
        .arg(".env")
        .output()
        .expect("run binary with .env");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(stderr.contains("access blocked"));

    let tmp_keyring = format!("{}/my_keyring.json", env!("CARGO_TARGET_TMPDIR"));
    fs::write(&tmp_keyring, "{}").expect("write temp");
    let output = Command::new(env!("CARGO_BIN_EXE_phase0-evidence"))
        .arg("--input")
        .arg(&tmp_keyring)
        .output()
        .expect("run binary with keyring name");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(stderr.contains("access blocked"));
}

#[test]
fn cli_invalid_flags_exit_code_2() {
    let output = Command::new(env!("CARGO_BIN_EXE_phase0-evidence"))
        .arg("--bogus-flag")
        .output()
        .expect("run binary");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(stderr.contains("Usage:"));
}

#[test]
fn cli_malformed_input_exit_code_1_no_stdout_no_secret_leak() {
    let test_secrets = [
        "super_confidential_secret_value",
        "user_secret:pass_secret",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "76561198000000001",
    ];

    for secret in test_secrets {
        let tmp_path = format!("{}/secret_leak_test.json", env!("CARGO_TARGET_TMPDIR"));
        let mut bad =
            serde_json::from_str::<serde_json::Value>(EMBEDDED_FIXTURE).expect("parse json");
        bad["routes"][0]["expected_assertions"][0] =
            serde_json::Value::String(format!("token={secret}"));
        fs::write(&tmp_path, bad.to_string()).expect("write temp");

        let output = Command::new(env!("CARGO_BIN_EXE_phase0-evidence"))
            .arg("--input")
            .arg(&tmp_path)
            .output()
            .expect("run binary with malformed");

        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
        assert!(stderr.contains("phase0-evidence error:"));
        // Fail-closed error must NOT contain the secret value itself
        assert!(
            !stderr.contains(secret),
            "stderr leaked secret '{secret}': {stderr}"
        );
    }
}

#[test]
fn cli_oversized_file_rejected() {
    let tmp_path = format!("{}/oversized.json", env!("CARGO_TARGET_TMPDIR"));
    fs::write(&tmp_path, " ".repeat(65537)).expect("write oversized");

    let output = Command::new(env!("CARGO_BIN_EXE_phase0-evidence"))
        .arg("--input")
        .arg(&tmp_path)
        .output()
        .expect("run binary with oversized");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(stderr.contains("exceeds"));
}
