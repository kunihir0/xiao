use std::fmt;

use p256::{
    PublicKey, SecretKey,
    elliptic_curve::{Generate, sec1::ToSec1Point},
};
use rand::rng;
use thiserror::Error;

const UNCOMPRESSED_LEN: usize = 65;
const COMPRESSED_LEN: usize = 33;

/// Fixed-message error for any rejected P-256 public key. Carries no input.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("invalid P-256 public key")]
pub struct KeyError;

/// Ephemeral P-256 private key that receives the sealed result.
///
/// Lives only in bot memory and is zeroized on drop.
pub struct RecipientSecret(SecretKey);

impl RecipientSecret {
    #[must_use]
    pub fn generate() -> Self {
        Self(SecretKey::generate_from_rng(&mut rng()))
    }

    #[cfg(test)]
    pub(crate) fn from_scalar_bytes(bytes: &[u8; 32]) -> Result<Self, KeyError> {
        SecretKey::from_slice(bytes).map(Self).map_err(|_| KeyError)
    }

    #[must_use]
    pub fn public_key(&self) -> RecipientKey {
        RecipientKey(self.0.public_key())
    }

    pub(crate) const fn secret_key(&self) -> &SecretKey {
        &self.0
    }
}

impl fmt::Debug for RecipientSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RecipientSecret(<redacted>)")
    }
}

/// A valid, non-identity P-256 public key.
///
/// Equality compares curve points, so the compressed and uncompressed
/// encodings of one key are equal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecipientKey(PublicKey);

impl RecipientKey {
    /// Strict 65-byte SEC1 uncompressed form (`0x04 || X || Y`), as stored in
    /// the database and used for the ephemeral key. Rejects any other length
    /// or prefix, off-curve points and the identity.
    pub fn from_uncompressed(bytes: &[u8]) -> Result<Self, KeyError> {
        Self::from_sec1(bytes, UNCOMPRESSED_LEN, &[0x04])
    }

    /// Strict 33-byte SEC1 compressed form (`0x02|0x03 || X`), as carried in
    /// the URL fragment. Rejects any other length or prefix, off-curve points
    /// and the identity.
    pub fn from_compressed(bytes: &[u8]) -> Result<Self, KeyError> {
        Self::from_sec1(bytes, COMPRESSED_LEN, &[0x02, 0x03])
    }

    fn from_sec1(bytes: &[u8], length: usize, prefixes: &[u8]) -> Result<Self, KeyError> {
        let prefix_ok = bytes
            .first()
            .is_some_and(|prefix| prefixes.contains(prefix));
        if bytes.len() != length || !prefix_ok {
            return Err(KeyError);
        }
        PublicKey::from_sec1_bytes(bytes)
            .map(Self)
            .map_err(|_| KeyError)
    }

    #[must_use]
    pub fn to_uncompressed(&self) -> [u8; UNCOMPRESSED_LEN] {
        copy_encoded(&self.0.to_sec1_point(false))
    }

    #[must_use]
    pub fn to_compressed(&self) -> [u8; COMPRESSED_LEN] {
        copy_encoded(&self.0.to_sec1_point(true))
    }

    pub(crate) const fn from_public(key: PublicKey) -> Self {
        Self(key)
    }

    pub(crate) const fn public_key(&self) -> &PublicKey {
        &self.0
    }
}

fn copy_encoded<const N: usize>(
    point: &p256::elliptic_curve::sec1::Sec1Point<p256::NistP256>,
) -> [u8; N] {
    let mut out = [0u8; N];
    for (slot, byte) in out.iter_mut().zip(point.as_bytes()) {
        *slot = *byte;
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    fn key(scalar: u8) -> RecipientKey {
        RecipientSecret::from_scalar_bytes(&[scalar; 32])
            .unwrap()
            .public_key()
    }

    #[test]
    fn compressed_and_uncompressed_round_trip_to_equal_keys() {
        let original = key(0x11);
        let from_unc = RecipientKey::from_uncompressed(&original.to_uncompressed()).unwrap();
        let from_cmp = RecipientKey::from_compressed(&original.to_compressed()).unwrap();
        assert_eq!(from_unc, from_cmp);
        assert_eq!(from_unc, original);
        assert_eq!(from_cmp.to_uncompressed(), original.to_uncompressed());
        assert_eq!(from_unc.to_compressed(), original.to_compressed());
    }

    #[test]
    fn different_keys_are_unequal() {
        assert_ne!(key(0x11), key(0x12));
    }

    #[test]
    fn encodings_have_expected_shape() {
        let key = key(0x11);
        assert_eq!(key.to_uncompressed()[0], 0x04);
        assert!(matches!(key.to_compressed()[0], 0x02 | 0x03));
        // Independent vector from python `cryptography`.
        assert_eq!(
            hex(&key.to_compressed()),
            "020217e617f0b6443928278f96999e69a23a4f2c152bdf6d6cdf66e5b80282d4ed"
        );
    }

    #[test]
    fn uncompressed_rejects_wrong_length_prefix_and_garbage() {
        let good = key(0x11).to_uncompressed();
        assert!(RecipientKey::from_uncompressed(&good).is_ok());
        assert!(RecipientKey::from_uncompressed(&[]).is_err());
        assert!(RecipientKey::from_uncompressed(&good[..64]).is_err());
        let mut longer = good.to_vec();
        longer.push(0);
        assert!(RecipientKey::from_uncompressed(&longer).is_err());
        for prefix in [0x00u8, 0x02, 0x03, 0x05, 0x06, 0x07, 0xff] {
            let mut bad = good;
            bad[0] = prefix;
            assert!(
                RecipientKey::from_uncompressed(&bad).is_err(),
                "{prefix:#x}"
            );
        }
        // The compressed encoding is not accepted as uncompressed.
        assert!(RecipientKey::from_uncompressed(&key(0x11).to_compressed()).is_err());
    }

    #[test]
    fn uncompressed_rejects_off_curve_and_identity() {
        let mut off_curve = key(0x11).to_uncompressed();
        off_curve[64] ^= 1;
        assert!(RecipientKey::from_uncompressed(&off_curve).is_err());
        let mut zeros = [0u8; 65];
        zeros[0] = 0x04;
        assert!(RecipientKey::from_uncompressed(&zeros).is_err());
        // SEC1 identity is the single byte 0x00 and the all-zero 65 bytes.
        assert!(RecipientKey::from_uncompressed(&[0u8; 65]).is_err());
        assert!(RecipientKey::from_uncompressed(&[0u8; 1]).is_err());
    }

    #[test]
    fn compressed_rejects_wrong_length_prefix_and_off_curve() {
        let good = key(0x11).to_compressed();
        assert!(RecipientKey::from_compressed(&good).is_ok());
        assert!(RecipientKey::from_compressed(&[]).is_err());
        assert!(RecipientKey::from_compressed(&good[..32]).is_err());
        let mut longer = good.to_vec();
        longer.push(0);
        assert!(RecipientKey::from_compressed(&longer).is_err());
        for prefix in [0x00u8, 0x01, 0x04, 0x05, 0xff] {
            let mut bad = good;
            bad[0] = prefix;
            assert!(RecipientKey::from_compressed(&bad).is_err(), "{prefix:#x}");
        }
        assert!(RecipientKey::from_compressed(&[0u8; 33]).is_err());
        assert!(RecipientKey::from_compressed(&[0u8; 1]).is_err());
        // x = 1 has no point on P-256 (x^3 - 3x + b is a non-residue mod p).
        let mut off_curve = [0u8; 33];
        off_curve[0] = 0x02;
        off_curve[32] = 1;
        assert!(RecipientKey::from_compressed(&off_curve).is_err());
        // The uncompressed encoding is not accepted as compressed.
        assert!(RecipientKey::from_compressed(&key(0x11).to_uncompressed()).is_err());
    }

    #[test]
    fn secret_debug_is_redacted() {
        let secret = RecipientSecret::generate();
        assert_eq!(format!("{secret:?}"), "RecipientSecret(<redacted>)");
        assert_eq!(format!("{secret:#?}"), "RecipientSecret(<redacted>)");
    }

    #[test]
    fn generated_secrets_have_distinct_public_keys() {
        assert_ne!(
            RecipientSecret::generate().public_key(),
            RecipientSecret::generate().public_key()
        );
    }

    #[test]
    fn scalar_zero_is_rejected() {
        assert!(RecipientSecret::from_scalar_bytes(&[0; 32]).is_err());
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}
