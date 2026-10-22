use hkdf::Hkdf;
use sha2::Sha256;

use crate::{CONFIRM_CODE_LEN, LinkSecret};

const CONFIRM_INFO: &[u8] = b"xiao.login.confirm.v1";
const ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

/// Short code the user compares between Discord and the login page.
///
/// The first 6 bytes of `HKDF-SHA256(ikm = secret, salt = empty,
/// info = "xiao.login.confirm.v1")`, each masked with `& 31` and mapped through
/// a 32-character alphabet without look-alike glyphs. The page recomputes it
/// with WebCrypto; `tests/fixtures/confirm_code.json` pins both sides.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfirmCode(String);

impl ConfirmCode {
    #[must_use]
    pub fn of(secret: &LinkSecret) -> Self {
        let mut okm = [0u8; CONFIRM_CODE_LEN];
        // Expanding 6 bytes is far below the HKDF-SHA256 output limit
        // (255 * 32), so the error arm is unreachable.
        let _ = Hkdf::<Sha256>::new(None, secret.as_bytes()).expand(CONFIRM_INFO, &mut okm);
        let code = okm
            .iter()
            // `& 31` bounds the index below the 32-entry alphabet.
            .map(|byte| char::from(ALPHABET[usize::from(byte & 31)]))
            .collect();
        Self(code)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_secret_matches_independent_vector() {
        // python3 hmac/hashlib HKDF: a7b1cd67bf49 -> & 31 -> 7,17,13,7,31,9.
        let code = ConfirmCode::of(&LinkSecret::from_bytes([0; 32]));
        assert_eq!(code.as_str(), "9KF9ZB");
    }

    #[test]
    fn code_has_fixed_length_and_alphabet() {
        for _ in 0..64 {
            let code = ConfirmCode::of(&LinkSecret::generate());
            assert_eq!(code.as_str().len(), CONFIRM_CODE_LEN);
            assert!(code.as_str().bytes().all(|byte| ALPHABET.contains(&byte)));
        }
    }

    #[test]
    fn alphabet_has_no_lookalikes_and_is_unique() {
        for banned in *b"01OI" {
            assert!(!ALPHABET.contains(&banned));
        }
        let mut sorted = ALPHABET.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 32);
    }

    #[test]
    fn same_secret_same_code() {
        let a = ConfirmCode::of(&LinkSecret::from_bytes([5; 32]));
        let b = ConfirmCode::of(&LinkSecret::from_bytes([5; 32]));
        assert_eq!(a, b);
    }
}
