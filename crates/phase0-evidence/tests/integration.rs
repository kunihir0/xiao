#![allow(clippy::expect_used)]

use std::io::Write;
use std::process::{Command, Stdio};

const EMBEDDED_FIXTURE: &str = include_str!("../fixtures/evidence.json");

fn run_stdin(input: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_phase0-evidence"))
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn binary");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait child")
}

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
fn cli_stdin_works() {
    let output = run_stdin(EMBEDDED_FIXTURE);
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .expect("utf8 stdout")
            .contains("\"offline\":true")
    );
}

#[test]
fn cli_invalid_flags_exit_code_2() {
    let output = Command::new(env!("CARGO_BIN_EXE_phase0-evidence"))
        .arg("--bogus-flag")
        .output()
        .expect("run binary");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .expect("utf8 stderr")
            .contains("Usage:")
    );
}

#[test]
fn cli_secret_error_does_not_leak() {
    for secret in [
        "secret123",
        "76561198000000001",
        "https://u:password@example.invalid/path",
        "https://fcm.googleapis.com/fcm/connect/REAL_TOKEN",
        "{\"token\":\"REAL_TOKEN\"}",
    ] {
        let mut bad: serde_json::Value = serde_json::from_str(EMBEDDED_FIXTURE).expect("fixture");
        bad["routes"][0]["expected_assertions"][0] = serde_json::Value::String(secret.to_owned());
        let output = run_stdin(&bad.to_string());
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
        assert!(stderr.contains("phase0-evidence error:"));
        assert!(!stderr.contains(secret));
    }
}

#[test]
fn cli_rejects_unapproved_register_device() {
    let mut bad: serde_json::Value = serde_json::from_str(EMBEDDED_FIXTURE).expect("fixture");
    bad["routes"][0]["current"]["steps"][1]["payload"]["device"] =
        serde_json::Value::String("1234".to_owned());
    let output = run_stdin(&bad.to_string());
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn cli_oversized_stdin_rejected() {
    let output = run_stdin(&" ".repeat(65537));
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stderr)
            .expect("utf8 stderr")
            .contains("exceeds")
    );
}
