use aes_gcm::{
    Aes128Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use base64::{
    Engine,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use hkdf::Hkdf;
use p256::{PublicKey, SecretKey, ecdh::diffie_hellman, elliptic_curve::sec1::ToSec1Point};
use sha2::Sha256;

use crate::{Error, Result};

#[derive(Clone, PartialEq, Eq)]
pub struct WebPushKeyMaterial {
    private_key: Vec<u8>,
    auth_secret: Vec<u8>,
}

impl std::fmt::Debug for WebPushKeyMaterial {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebPushKeyMaterial")
            .field("private_key", &"<redacted>")
            .field("auth_secret", &"<redacted>")
            .finish()
    }
}

impl WebPushKeyMaterial {
    pub fn from_base64(private_key: &str, auth_secret: &str) -> Result<Self> {
        let private_key = decode(private_key)?;
        let auth_secret = decode(auth_secret)?;
        if private_key.len() != 32 {
            return Err(Error::InvalidKey(
                "P-256 private key must be 32 bytes".into(),
            ));
        }
        if auth_secret.len() != 16 {
            return Err(Error::InvalidKey(
                "Web Push auth secret must be 16 bytes".into(),
            ));
        }
        Ok(Self {
            private_key,
            auth_secret,
        })
    }
    pub fn from_bytes(private_key: [u8; 32], auth_secret: [u8; 16]) -> Self {
        Self {
            private_key: private_key.to_vec(),
            auth_secret: auth_secret.to_vec(),
        }
    }
}

/// Decrypts the legacy Web Push `aesgcm` content encoding used by FCM.
/// The two-byte network-order padding length is mandatory; malformed padding is rejected.
pub fn decrypt_web_push(
    material: &WebPushKeyMaterial,
    crypto_key: &str,
    salt: &str,
    ciphertext: &[u8],
) -> Result<Vec<u8>> {
    let salt = decode_prefixed(salt, "salt=")?;
    let peer = decode_prefixed(crypto_key, "dh=")?;
    if salt.len() != 16 {
        return Err(Error::InvalidKey("Web Push salt must be 16 bytes".into()));
    }
    let private = SecretKey::from_slice(&material.private_key)
        .map_err(|error| Error::InvalidKey(error.to_string()))?;
    let public =
        PublicKey::from_sec1_bytes(&peer).map_err(|error| Error::InvalidKey(error.to_string()))?;
    let shared = diffie_hellman(private.to_nonzero_scalar(), public.as_affine());
    let ua_public = private.public_key().to_sec1_point(false);
    let as_public = public.to_sec1_point(false);
    let mut key_info =
        Vec::with_capacity(14 + ua_public.as_bytes().len() + as_public.as_bytes().len());
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(ua_public.as_bytes());
    key_info.extend_from_slice(as_public.as_bytes());
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(
        Some(&material.auth_secret),
        shared.raw_secret_bytes().as_slice(),
    )
    .expand(&key_info, &mut ikm)
    .map_err(|_| Error::DecryptionFailed)?;
    let hkdf = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut context =
        Vec::with_capacity(8 + ua_public.as_bytes().len() + as_public.as_bytes().len());
    context.extend_from_slice(b"P-256\0");
    context.extend_from_slice(&(ua_public.as_bytes().len() as u16).to_be_bytes());
    context.extend_from_slice(ua_public.as_bytes());
    context.extend_from_slice(&(as_public.as_bytes().len() as u16).to_be_bytes());
    context.extend_from_slice(as_public.as_bytes());
    let mut cek_info = b"Content-Encoding: aesgcm\0".to_vec();
    cek_info.extend_from_slice(&context);
    let mut cek = [0u8; 16];
    hkdf.expand(&cek_info, &mut cek)
        .map_err(|_| Error::DecryptionFailed)?;
    let mut nonce_info = b"Content-Encoding: nonce\0".to_vec();
    nonce_info.extend_from_slice(&context);
    let mut nonce_bytes = [0u8; 12];
    hkdf.expand(&nonce_info, &mut nonce_bytes)
        .map_err(|_| Error::DecryptionFailed)?;
    let nonce = Nonce::try_from(nonce_bytes.as_slice()).map_err(|_| Error::DecryptionFailed)?;
    let plaintext = Aes128Gcm::new_from_slice(&cek)
        .map_err(|_| Error::DecryptionFailed)?
        .decrypt(&nonce, ciphertext)
        .map_err(|_| Error::DecryptionFailed)?;
    let padding = usize::from(u16::from_be_bytes([plaintext[0], plaintext[1]]));
    let start = 2usize.checked_add(padding).ok_or(Error::InvalidPadding)?;
    if start >= plaintext.len() || plaintext[2..start].iter().any(|byte| *byte != 0) {
        return Err(Error::InvalidPadding);
    }
    Ok(plaintext[start..].to_vec())
}

fn decode_prefixed(value: &str, prefix: &str) -> Result<Vec<u8>> {
    decode(value.strip_prefix(prefix).unwrap_or(value))
}
fn decode(value: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(value)
        .or_else(|_| URL_SAFE.decode(value))
        .map_err(|error| Error::InvalidBase64(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::aead::{Aead, KeyInit};

    #[test]
    fn malformed_base64_and_padding_fail_closed() {
        let material = WebPushKeyMaterial::from_bytes([1; 32], [2; 16]);
        assert!(matches!(
            decrypt_web_push(&material, "dh=not-base64", "salt=bad", &[]),
            Err(Error::InvalidBase64(_))
        ));
    }

    #[test]
    fn rejects_invalid_key_sizes() {
        assert!(WebPushKeyMaterial::from_base64("AQ", "Ag").is_err());
    }
    #[test]
    fn rfc8291_p256_ecdh_known_answer() {
        let private_result = decode("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94");
        let public_result = decode(
            "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8",
        );
        assert!(private_result.is_ok());
        assert!(public_result.is_ok());
        if let (Ok(private_bytes), Ok(public_bytes)) = (private_result, public_result) {
            let secret_result = SecretKey::from_slice(&private_bytes);
            let public_result = PublicKey::from_sec1_bytes(&public_bytes);
            assert!(secret_result.is_ok());
            assert!(public_result.is_ok());
            if let (Ok(secret), Ok(public)) = (secret_result, public_result) {
                let shared = diffie_hellman(secret.to_nonzero_scalar(), public.as_affine());
                assert_eq!(
                    URL_SAFE_NO_PAD.encode(shared.raw_secret_bytes()),
                    "kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs"
                );
            }
        }
    }

    #[test]
    fn decrypts_external_legacy_aesgcm_vector_and_rejects_authenticated_padding() {
        let material = WebPushKeyMaterial::from_base64(
            "9FWl15_QUQAWDaD3k3l50ZBZQJ4au27F1V4F0uLSD_M",
            "R29vIGdvbyBnJyBqb29iIQ",
        );
        assert!(material.is_ok());
        let Ok(material) = material else {
            return;
        };
        let ciphertext = include_str!("../fixtures/web_push_aesgcm.hex")
            .split_whitespace()
            .filter_map(|pair| u8::from_str_radix(pair, 16).ok())
            .collect::<Vec<_>>();
        let malformed = include_str!("../fixtures/web_push_malformed_padding.hex")
            .split_whitespace()
            .filter_map(|pair| u8::from_str_radix(pair, 16).ok())
            .collect::<Vec<_>>();
        let all_padding = include_str!("../fixtures/web_push_all_padding.hex")
            .split_whitespace()
            .filter_map(|pair| u8::from_str_radix(pair, 16).ok())
            .collect::<Vec<_>>();
        let peer = "BNoRDbb84JGm8g5Z5CFxurSqsXWJ11ItfXEWYVLE85Y7CYkDjXsIEc4aqxYaQ1G8BqkXCJ6DPpDrWtdWj_mugHU";
        let salt = "lngarbyKfMoi9Z75xYXmkg";
        assert_eq!(
            decrypt_web_push(&material, &format!("dh={peer}"), &format!("salt={salt}"), &ciphertext)
                .ok(),
            Some(b"I am the walrus".to_vec())
        );
        assert!(matches!(
            decrypt_web_push(&material, &format!("dh={peer}"), &format!("salt={salt}"), &malformed),
            Err(Error::InvalidPadding)
        ));
        assert!(matches!(
            decrypt_web_push(&material, &format!("dh={peer}"), &format!("salt={salt}"), &all_padding),
            Err(Error::InvalidPadding)
        ));
    }

    #[test]
    fn authenticated_ciphertext_tampering_fails_closed() {
        let material = WebPushKeyMaterial::from_bytes([1; 32], [2; 16]);
        let peer = SecretKey::from_slice(&[2; 32]).ok().map(|secret| {
            URL_SAFE_NO_PAD.encode(secret.public_key().to_sec1_point(false).as_bytes())
        });
        assert!(peer.is_some());
        if let Some(peer) = peer {
            let salt = URL_SAFE_NO_PAD.encode([3_u8; 16]);
            assert!(matches!(
                decrypt_web_push(
                    &material,
                    &format!("dh={peer}"),
                    &format!("salt={salt}"),
                    &[0_u8; 32]
                ),
                Err(Error::DecryptionFailed)
            ));
        }
    }

    #[test]
    fn nist_aes128_gcm_known_answer_empty_plaintext() {
        let key = [0_u8; 16];
        let nonce_bytes = [0_u8; 12];
        let nonce_result = Nonce::try_from(nonce_bytes.as_slice());
        assert!(nonce_result.is_ok());
        let key_result = Aes128Gcm::new_from_slice(&key);
        assert!(key_result.is_ok());
        if let (Ok(nonce), Ok(cipher)) = (nonce_result, key_result) {
            let ciphertext = cipher.encrypt(&nonce, &[][..]);
            assert!(ciphertext.is_ok());
            if let Ok(ciphertext) = ciphertext {
                assert_eq!(
                    ciphertext,
                    [
                        0x58, 0xe2, 0xfc, 0xce, 0xfa, 0x7e, 0x30, 0x61, 0x36, 0x7f, 0x1d, 0x57,
                        0xa4, 0xe7, 0x45, 0x5a
                    ]
                );
            }
        }
    }
}
