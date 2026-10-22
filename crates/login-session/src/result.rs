use std::fmt;

use bot_domain::SteamId;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::MAX_TOKEN_BYTES;

const STEAM_ID_LEN: usize = 8;
const TOKEN_LEN_LEN: usize = 4;
/// Smallest valid plaintext: SteamID64, token length, one token byte.
pub(crate) const MIN_PLAINTEXT_BYTES: usize = STEAM_ID_LEN + TOKEN_LEN_LEN + 1;
/// Largest valid plaintext: SteamID64, token length, maximum token.
pub(crate) const MAX_PLAINTEXT_BYTES: usize = STEAM_ID_LEN + TOKEN_LEN_LEN + MAX_TOKEN_BYTES;

/// Fixed-message rejection of a login result. Carries no secret data.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ResultError {
    #[error("invalid steam id")]
    InvalidSteamId,
    #[error("invalid companion token")]
    InvalidToken,
    #[error("malformed login result encoding")]
    Malformed,
}

/// The validated outcome of a companion login: a SteamID64 and the companion
/// auth token. The token is zeroized on drop and never printed.
pub struct CompanionLoginResult {
    steam_id: SteamId,
    token: Zeroizing<String>,
}

impl CompanionLoginResult {
    /// Accepts 1..=[`MAX_TOKEN_BYTES`] bytes with no whitespace or control
    /// characters, mirroring the application's `SteamAuthToken`.
    pub fn new(steam_id: SteamId, token: String) -> Result<Self, ResultError> {
        let token = Zeroizing::new(token);
        if !is_valid_token(&token) {
            return Err(ResultError::InvalidToken);
        }
        Ok(Self { steam_id, token })
    }

    #[must_use]
    pub const fn steam_id(&self) -> SteamId {
        self.steam_id
    }

    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Consumes the result so the token moves on without an extra copy.
    #[must_use]
    pub fn into_parts(self) -> (SteamId, Zeroizing<String>) {
        (self.steam_id, self.token)
    }

    /// Deterministic plaintext for sealing:
    /// `steam_id u64 BE || token_len u32 BE || token bytes`. The envelope
    /// carries the format version, so nothing is versioned in here.
    pub(crate) fn encode(&self) -> Zeroizing<Vec<u8>> {
        let token = self.token.as_bytes();
        let mut out = Zeroizing::new(Vec::with_capacity(
            STEAM_ID_LEN + TOKEN_LEN_LEN + token.len(),
        ));
        out.extend_from_slice(&self.steam_id.value().to_be_bytes());
        // Token length is validated to be at most MAX_TOKEN_BYTES.
        out.extend_from_slice(&u32::try_from(token.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(token);
        out
    }

    /// Strict inverse of [`Self::encode`]: rejects truncation, a length that
    /// disagrees with the buffer, trailing bytes, and re-validates both fields.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, ResultError> {
        let (steam_bytes, rest) = bytes
            .split_first_chunk::<STEAM_ID_LEN>()
            .ok_or(ResultError::Malformed)?;
        let (len_bytes, token_bytes) = rest
            .split_first_chunk::<TOKEN_LEN_LEN>()
            .ok_or(ResultError::Malformed)?;
        let declared =
            usize::try_from(u32::from_be_bytes(*len_bytes)).map_err(|_| ResultError::Malformed)?;
        if declared != token_bytes.len() {
            return Err(ResultError::Malformed);
        }
        let steam_id = SteamId::new(u64::from_be_bytes(*steam_bytes))
            .map_err(|_| ResultError::InvalidSteamId)?;
        let token = std::str::from_utf8(token_bytes).map_err(|_| ResultError::InvalidToken)?;
        Self::new(steam_id, token.to_owned())
    }
}

fn is_valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_TOKEN_BYTES
        && !token
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

impl fmt::Debug for CompanionLoginResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompanionLoginResult")
            .field("steam_id", &self.steam_id)
            .field("token", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    fn steam(value: u64) -> SteamId {
        SteamId::new(value).unwrap()
    }

    fn result(token: &str) -> Result<CompanionLoginResult, ResultError> {
        CompanionLoginResult::new(steam(76_561_198_000_000_000), token.to_string())
    }

    #[test]
    fn token_length_boundaries() {
        assert_eq!(result("").unwrap_err(), ResultError::InvalidToken);
        assert!(result("a").is_ok());
        assert!(result(&"a".repeat(MAX_TOKEN_BYTES)).is_ok());
        assert_eq!(
            result(&"a".repeat(MAX_TOKEN_BYTES + 1)).unwrap_err(),
            ResultError::InvalidToken
        );
    }

    #[test]
    fn token_limit_counts_bytes_not_characters() {
        // 4096 two-byte characters is exactly 8192 bytes; one more is over.
        assert!(result(&"\u{e9}".repeat(4096)).is_ok());
        assert!(result(&"\u{e9}".repeat(4097)).is_err());
    }

    #[test]
    fn token_rejects_whitespace_and_control_characters() {
        for bad in [
            "a b",
            " a",
            "a ",
            "a\tb",
            "a\nb",
            "a\rb",
            "a\u{7f}b",
            "a\u{0}b",
            "a\u{1b}b",
            "a\u{85}b",
            "a\u{a0}b",
            "a\u{2028}b",
            "a\u{3000}b",
        ] {
            assert_eq!(
                result(bad).unwrap_err(),
                ResultError::InvalidToken,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn token_accepts_typical_credential_characters() {
        assert!(result("tok-ABC123_xyz.-=+/").is_ok());
    }

    #[test]
    fn steam_id_zero_is_rejected_by_domain() {
        assert!(SteamId::new(0).is_err());
        let mut plain = vec![0u8; 8];
        plain.extend_from_slice(&1u32.to_be_bytes());
        plain.push(b'a');
        assert_eq!(
            CompanionLoginResult::decode(&plain).unwrap_err(),
            ResultError::InvalidSteamId
        );
    }

    #[test]
    fn encoding_is_deterministic_and_matches_layout() {
        let encoded = result("tok-ABC123_xyz.-").unwrap().encode();
        assert_eq!(
            hex(&encoded),
            "01100001025e4c0000000010746f6b2d4142433132335f78797a2e2d"
        );
    }

    #[test]
    fn decode_round_trips() {
        let original = result("tok-ABC123_xyz.-").unwrap();
        let decoded = CompanionLoginResult::decode(&original.encode()).unwrap();
        assert_eq!(decoded.steam_id(), original.steam_id());
        assert_eq!(decoded.token(), original.token());
    }

    #[test]
    fn decode_rejects_malformed_buffers() {
        let good = result("abc").unwrap().encode();
        // Trailing bytes.
        let mut trailing = good.to_vec();
        trailing.push(0);
        assert_eq!(
            CompanionLoginResult::decode(&trailing).unwrap_err(),
            ResultError::Malformed
        );
        // Truncation at every length shorter than the full buffer.
        for cut in 0..good.len() {
            assert!(
                CompanionLoginResult::decode(&good[..cut]).is_err(),
                "cut {cut}"
            );
        }
        // Declared length larger and smaller than the actual token.
        for declared in [0u32, 2, 4, u32::MAX] {
            let mut bad = good.to_vec();
            bad[8..12].copy_from_slice(&declared.to_be_bytes());
            assert_eq!(
                CompanionLoginResult::decode(&bad).unwrap_err(),
                ResultError::Malformed,
                "{declared}"
            );
        }
    }

    #[test]
    fn decode_revalidates_token() {
        let mut bad = Vec::new();
        bad.extend_from_slice(&1u64.to_be_bytes());
        bad.extend_from_slice(&3u32.to_be_bytes());
        bad.extend_from_slice(b"a b");
        assert_eq!(
            CompanionLoginResult::decode(&bad).unwrap_err(),
            ResultError::InvalidToken
        );
        // Empty token.
        let mut empty = Vec::new();
        empty.extend_from_slice(&1u64.to_be_bytes());
        empty.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(
            CompanionLoginResult::decode(&empty).unwrap_err(),
            ResultError::InvalidToken
        );
        // Invalid UTF-8.
        let mut utf8 = Vec::new();
        utf8.extend_from_slice(&1u64.to_be_bytes());
        utf8.extend_from_slice(&2u32.to_be_bytes());
        utf8.extend_from_slice(&[0xff, 0xfe]);
        assert_eq!(
            CompanionLoginResult::decode(&utf8).unwrap_err(),
            ResultError::InvalidToken
        );
        // Oversized token.
        let mut big = Vec::new();
        big.extend_from_slice(&1u64.to_be_bytes());
        let len = u32::try_from(MAX_TOKEN_BYTES + 1).unwrap();
        big.extend_from_slice(&len.to_be_bytes());
        big.extend_from_slice(&vec![b'a'; MAX_TOKEN_BYTES + 1]);
        assert_eq!(
            CompanionLoginResult::decode(&big).unwrap_err(),
            ResultError::InvalidToken
        );
    }

    #[test]
    fn plaintext_bounds_match_encoding() {
        assert_eq!(result("a").unwrap().encode().len(), MIN_PLAINTEXT_BYTES);
        assert_eq!(
            result(&"a".repeat(MAX_TOKEN_BYTES)).unwrap().encode().len(),
            MAX_PLAINTEXT_BYTES
        );
    }

    #[test]
    fn debug_never_prints_the_token() {
        let token = "super-secret-token-value";
        let rendered = format!("{:?}", result(token).unwrap());
        assert!(!rendered.contains(token));
        assert!(!rendered.contains("secret"));
        assert!(rendered.contains("<redacted>"));
        let pretty = format!("{:#?}", result(token).unwrap());
        assert!(!pretty.contains(token));
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn into_parts_moves_the_validated_values() {
        let (steam_id, token) = result("tok-ABC123").unwrap().into_parts();
        assert_eq!(steam_id.value(), 76_561_198_000_000_000);
        assert_eq!(token.as_str(), "tok-ABC123");
    }
}
