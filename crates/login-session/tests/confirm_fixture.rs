//! The same fixture is reused by the login page's WebCrypto test, so the Rust
//! and JavaScript confirm-code implementations are pinned to one source.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use login_session::{ConfirmCode, LinkSecret, LookupHash};

const FIXTURE: &str = include_str!("fixtures/confirm_code.json");

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn fixture_vectors_match_confirm_code_and_lookup_hash() {
    let document: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    let vectors = document["vectors"].as_array().unwrap();
    assert!(vectors.len() >= 3);
    for vector in vectors {
        let secret_b64 = vector["secret_b64url"].as_str().unwrap();
        let expected_code = vector["code"].as_str().unwrap();
        let expected_lookup = vector["lookup_hash_hex"].as_str().unwrap();
        let secret = LinkSecret::parse_base64url(secret_b64).unwrap();
        assert_eq!(ConfirmCode::of(&secret).as_str(), expected_code);
        assert_eq!(hex(LookupHash::of(&secret).as_bytes()), expected_lookup);
        assert_eq!(secret.to_base64url().as_str(), secret_b64);
    }
}

#[test]
fn fixture_covers_required_secrets() {
    let document: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    let encoded: Vec<&str> = document["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|vector| vector["secret_b64url"].as_str().unwrap())
        .collect();
    for required in [[0x00u8; 32], [0xffu8; 32]] {
        let secret = LinkSecret::from_bytes(required);
        assert!(encoded.contains(&secret.to_base64url().as_str()));
    }
    let mut ascending = [0u8; 32];
    for (index, byte) in ascending.iter_mut().enumerate() {
        *byte = u8::try_from(index).unwrap();
    }
    let secret = LinkSecret::from_bytes(ascending);
    assert!(encoded.contains(&secret.to_base64url().as_str()));
}
