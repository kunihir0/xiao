use std::fmt;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngExt, rng};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

const LINK_SECRET_LEN: usize = 32;
/// Length of a canonical unpadded base64url encoding of a 32-byte secret.
pub const LINK_SECRET_BASE64_LEN: usize = 43;
const LOOKUP_DOMAIN: &[u8] = b"xiao.login.lookup.v1\0";

/// Fixed-message error for malformed secret encodings. Carries no input.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("invalid link secret encoding")]
pub struct SecretError;

/// Strictly decodes canonical unpadded URL-safe base64 of exactly
/// `expected_chars` characters into exactly `N` bytes.
///
/// The engine rejects padding, the standard alphabet, whitespace, non-ASCII
/// input and non-zero trailing bits; the length checks pin the canonical form.
pub(crate) fn decode_base64url_exact<const N: usize>(
    input: &str,
    expected_chars: usize,
) -> Option<Zeroizing<[u8; N]>> {
    if input.len() != expected_chars {
        return None;
    }
    let decoded = Zeroizing::new(URL_SAFE_NO_PAD.decode(input).ok()?);
    if decoded.len() != N {
        return None;
    }
    let mut out = Zeroizing::new([0u8; N]);
    for (slot, byte) in out.iter_mut().zip(decoded.iter()) {
        *slot = *byte;
    }
    Some(out)
}

/// 32 random bytes that authenticate the browser and bind the sealed result.
///
/// Zeroized on drop, never `Display`ed, and deliberately not `Clone`.
pub struct LinkSecret(Zeroizing<[u8; LINK_SECRET_LEN]>);

impl LinkSecret {
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = Zeroizing::new([0u8; LINK_SECRET_LEN]);
        rng().fill(&mut bytes[..]);
        Self(bytes)
    }

    /// Wraps known bytes, for tests and fixtures. The caller's array is a
    /// plain copy that is not wiped; production secrets come from
    /// [`LinkSecret::generate`] or a parse.
    #[must_use]
    pub fn from_bytes(bytes: [u8; LINK_SECRET_LEN]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Unpadded URL-safe base64, exactly 43 characters.
    #[must_use]
    pub fn to_base64url(&self) -> Zeroizing<String> {
        Zeroizing::new(URL_SAFE_NO_PAD.encode(self.0.as_slice()))
    }

    /// Strict inverse of [`Self::to_base64url`]: exactly 43 canonical
    /// characters of the URL-safe alphabet, no padding, no trailing bits.
    pub fn parse_base64url(input: &str) -> Result<Self, SecretError> {
        decode_base64url_exact::<LINK_SECRET_LEN>(input, LINK_SECRET_BASE64_LEN)
            .map(Self)
            .ok_or(SecretError)
    }

    pub(crate) fn as_bytes(&self) -> &[u8; LINK_SECRET_LEN] {
        &self.0
    }
}

impl fmt::Debug for LinkSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LinkSecret(<redacted>)")
    }
}

/// Identifier of one login session. Distinct from the domain's Rust+
/// `SessionId`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LoginSessionId(Uuid);

impl LoginSessionId {
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }

    #[must_use]
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    #[must_use]
    pub const fn as_uuid(&self) -> Uuid {
        self.0
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }
}

impl fmt::Display for LoginSessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.hyphenated(), formatter)
    }
}

/// `SHA-256(b"xiao.login.lookup.v1\0" || secret)`: the database lookup key.
/// The database never stores the secret itself.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct LookupHash([u8; 32]);

impl LookupHash {
    #[must_use]
    pub fn of(secret: &LinkSecret) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(LOOKUP_DOMAIN);
        hasher.update(secret.as_bytes());
        Self(hasher.finalize().into())
    }

    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for LookupHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LookupHash(<hash>)")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    const ZEROS_B64: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    #[test]
    fn generate_round_trips_through_base64url() {
        let secret = LinkSecret::generate();
        let encoded = secret.to_base64url();
        assert_eq!(encoded.len(), 43);
        let parsed = LinkSecret::parse_base64url(&encoded).unwrap();
        assert_eq!(parsed.as_bytes(), secret.as_bytes());
    }

    #[test]
    fn two_generated_secrets_differ() {
        assert_ne!(
            LinkSecret::generate().as_bytes(),
            LinkSecret::generate().as_bytes()
        );
    }

    #[test]
    fn debug_is_redacted() {
        let secret = LinkSecret::from_bytes([7; 32]);
        assert_eq!(format!("{secret:?}"), "LinkSecret(<redacted>)");
        assert_eq!(format!("{secret:#?}"), "LinkSecret(<redacted>)");
    }

    #[test]
    fn parse_accepts_canonical_input() {
        let secret = LinkSecret::parse_base64url(ZEROS_B64).ok();
        assert_eq!(secret.map(|s| *s.as_bytes()), Some([0u8; 32]));
    }

    #[test]
    fn parse_rejects_every_non_canonical_form() {
        // Last symbol carries 4 data bits plus 2 trailing bits; 'B' sets one.
        let trailing_bits = format!("{}B", &ZEROS_B64[..42]);
        let cases = [
            ("empty", String::new()),
            ("42 chars", ZEROS_B64[..42].to_string()),
            ("44 chars", format!("{ZEROS_B64}A")),
            ("padded", format!("{}=", &ZEROS_B64[..42])),
            ("padded to 44", format!("{ZEROS_B64}=")),
            ("plus", format!("{}+", &ZEROS_B64[..42])),
            ("slash", format!("/{}", &ZEROS_B64[1..])),
            ("space", format!(" {}", &ZEROS_B64[1..])),
            ("newline", format!("{}\n", &ZEROS_B64[..42])),
            ("non-ascii", format!("{}\u{e9}", &ZEROS_B64[..41])),
            ("trailing bits", trailing_bits),
        ];
        for (name, input) in cases {
            assert!(
                LinkSecret::parse_base64url(&input).is_err(),
                "{name} must be rejected"
            );
        }
    }

    #[test]
    fn parse_rejects_all_non_canonical_last_symbols() {
        // Only symbols whose low two bits are zero are canonical in position 43.
        for (index, character) in "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
            .chars()
            .enumerate()
        {
            let input = format!("{}{character}", &ZEROS_B64[..42]);
            assert_eq!(
                LinkSecret::parse_base64url(&input).is_ok(),
                index % 4 == 0,
                "last symbol {character}"
            );
        }
    }

    #[test]
    fn lookup_hash_matches_independent_vectors() {
        // Computed with python3 hashlib.sha256(b"xiao.login.lookup.v1\0" + secret).
        let cases: [([u8; 32], &str); 2] = [
            (
                [0; 32],
                "14a1484f7a744a1a3dc020ad03e9c826ef5f39743c38cbf1352aa0273617b38e",
            ),
            (
                [0xff; 32],
                "d44f7ebc323ffc7204719ff18d26e1c23eb652fdffa5a52f9df8c7a58e11f5b7",
            ),
        ];
        for (bytes, expected) in cases {
            let hash = LookupHash::of(&LinkSecret::from_bytes(bytes));
            assert_eq!(hex(hash.as_bytes()), expected);
        }
        let mut ascending = [0u8; 32];
        for (index, byte) in ascending.iter_mut().enumerate() {
            *byte = u8::try_from(index).unwrap_or(0);
        }
        let hash = LookupHash::of(&LinkSecret::from_bytes(ascending));
        assert_eq!(
            hex(hash.as_bytes()),
            "24874b724d3fabb7448fed0b2618091c200146a3e3a0dcc48736b92a5ac6612f"
        );
    }

    #[test]
    fn lookup_hash_round_trips_bytes_and_hides_debug() {
        let hash = LookupHash::from_bytes([9; 32]);
        assert_eq!(hash.as_bytes(), &[9; 32]);
        assert_eq!(format!("{hash:?}"), "LookupHash(<hash>)");
    }

    #[test]
    fn session_id_exposes_uuid_bytes_and_hyphenated_display() {
        let uuid = Uuid::from_bytes([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ]);
        let id = LoginSessionId::from_uuid(uuid);
        assert_eq!(id.as_bytes(), uuid.as_bytes());
        assert_eq!(id.to_string(), "00112233-4455-6677-8899-aabbccddeeff");
        assert_eq!(id.as_uuid(), uuid);
        assert_ne!(LoginSessionId::generate(), LoginSessionId::generate());
    }

    pub(crate) fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}
