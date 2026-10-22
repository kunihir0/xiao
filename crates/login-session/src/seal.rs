use aes_gcm_siv::{
    Aes256GcmSiv, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use hkdf::Hkdf;
use p256::{SecretKey, elliptic_curve::Generate};
use rand::{RngExt, rng};
use sha2::Sha256;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::{
    CompanionLoginResult, LinkSecret, LoginSessionId, RecipientKey, RecipientSecret,
    result::{MAX_PLAINTEXT_BYTES, MIN_PLAINTEXT_BYTES},
};

/// Version of the sealed-result envelope. Bound into the AAD.
pub const RESULT_FORMAT_VERSION: u16 = 1;
/// Algorithm suite name. Bound into the AAD.
pub const RESULT_ALGORITHM: &str = "p256-hkdf-sha256-aes-256-gcm-siv";

const AAD_DOMAIN: &str = "xiao.login.result";
const AAD_TABLE: &str = "rustplus_login_sessions";
const KDF_INFO: &[u8] = b"xiao.login.result.v1";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// Shortest valid ciphertext: tag plus the minimal plaintext.
pub const MIN_CIPHERTEXT_BYTES: usize = MIN_PLAINTEXT_BYTES + TAG_LEN;
/// Longest valid ciphertext: tag plus the maximal plaintext.
pub const MAX_CIPHERTEXT_BYTES: usize = MAX_PLAINTEXT_BYTES + TAG_LEN;

/// The columns `xiao-login` writes: public data only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SealedResult {
    pub ephemeral_public_key: [u8; 65],
    pub nonce: [u8; NONCE_LEN],
    pub ciphertext: Vec<u8>,
}

/// Why sealing failed. Fixed messages, no secret data.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SealError {
    #[error("login result plaintext has an invalid length")]
    InvalidPlaintextLength,
    #[error("login result key derivation failed")]
    KeyDerivation,
    #[error("login result encryption failed")]
    Encryption,
}

/// Why opening failed. Fixed messages, no secret data.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum OpenError {
    #[error("invalid ephemeral public key")]
    InvalidEphemeralKey,
    #[error("sealed login result has an invalid length")]
    InvalidCiphertextLength,
    #[error("login result key derivation failed")]
    KeyDerivation,
    #[error("login result authentication failed")]
    Decryption,
    #[error("sealed login result contents are invalid")]
    InvalidPlaintext,
}

/// Seals `result` to `recipient`, bound to the link secret and session.
///
/// Ephemeral-static P-256 ECDH, then HKDF-SHA256 over the shared secret and
/// the link secret, then AES-256-GCM-SIV. Reading needs the recipient private
/// key; forging needs the link secret.
pub fn seal(
    recipient: &RecipientKey,
    secret: &LinkSecret,
    session: LoginSessionId,
    result: &CompanionLoginResult,
) -> Result<SealedResult, SealError> {
    let ephemeral = SecretKey::generate_from_rng(&mut rng());
    let mut nonce = [0u8; NONCE_LEN];
    rng().fill(&mut nonce);
    seal_with(&ephemeral, nonce, recipient, secret, session, result)
}

/// Deterministic core of [`seal`] for known-answer tests.
pub(crate) fn seal_with(
    ephemeral: &SecretKey,
    nonce: [u8; NONCE_LEN],
    recipient: &RecipientKey,
    secret: &LinkSecret,
    session: LoginSessionId,
    result: &CompanionLoginResult,
) -> Result<SealedResult, SealError> {
    let plaintext = result.encode();
    seal_plaintext_with(ephemeral, nonce, recipient, secret, session, &plaintext)
}

/// Seals raw plaintext bytes; lets tests craft malformed plaintexts that the
/// public API cannot produce.
pub(crate) fn seal_plaintext_with(
    ephemeral: &SecretKey,
    nonce: [u8; NONCE_LEN],
    recipient: &RecipientKey,
    secret: &LinkSecret,
    session: LoginSessionId,
    plaintext: &[u8],
) -> Result<SealedResult, SealError> {
    if plaintext.len() > MAX_PLAINTEXT_BYTES {
        return Err(SealError::InvalidPlaintextLength);
    }
    let ephemeral_public = RecipientKey::from_public(ephemeral.public_key());
    let ephemeral_bytes = ephemeral_public.to_uncompressed();
    let shared = ephemeral.diffie_hellman(recipient.public_key());
    let key = derive_key(
        shared.raw_secret_bytes().as_slice(),
        secret,
        session,
        &ephemeral_bytes,
        &recipient.to_uncompressed(),
    )
    .ok_or(SealError::KeyDerivation)?;
    let cipher = Aes256GcmSiv::new_from_slice(key.as_slice()).map_err(|_| SealError::Encryption)?;
    let ciphertext = cipher
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: plaintext,
                aad: &aad(session),
            },
        )
        .map_err(|_| SealError::Encryption)?;
    Ok(SealedResult {
        ephemeral_public_key: ephemeral_bytes,
        nonce,
        ciphertext,
    })
}

/// Opens a sealed result with the recipient private key, link secret and
/// session ID. Every failure is a fixed-message error.
pub fn open(
    recipient: &RecipientSecret,
    secret: &LinkSecret,
    session: LoginSessionId,
    sealed: &SealedResult,
) -> Result<CompanionLoginResult, OpenError> {
    let ephemeral = RecipientKey::from_uncompressed(&sealed.ephemeral_public_key)
        .map_err(|_| OpenError::InvalidEphemeralKey)?;
    if !(MIN_CIPHERTEXT_BYTES..=MAX_CIPHERTEXT_BYTES).contains(&sealed.ciphertext.len()) {
        return Err(OpenError::InvalidCiphertextLength);
    }
    let shared = recipient
        .secret_key()
        .diffie_hellman(ephemeral.public_key());
    let key = derive_key(
        shared.raw_secret_bytes().as_slice(),
        secret,
        session,
        &ephemeral.to_uncompressed(),
        &recipient.public_key().to_uncompressed(),
    )
    .ok_or(OpenError::KeyDerivation)?;
    let cipher = Aes256GcmSiv::new_from_slice(key.as_slice()).map_err(|_| OpenError::Decryption)?;
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                &Nonce::from(sealed.nonce),
                Payload {
                    msg: &sealed.ciphertext,
                    aad: &aad(session),
                },
            )
            .map_err(|_| OpenError::Decryption)?,
    );
    CompanionLoginResult::decode(&plaintext).map_err(|_| OpenError::InvalidPlaintext)
}

/// `ikm = ECDH x-coordinate || link secret`, `salt = session UUID`,
/// `info = "xiao.login.result.v1" || ephemeral pub || recipient pub` (both
/// uncompressed SEC1). Returns `None` only if HKDF refuses the output length,
/// which cannot happen for 32 bytes.
fn derive_key(
    shared: &[u8],
    secret: &LinkSecret,
    session: LoginSessionId,
    ephemeral_public: &[u8; 65],
    recipient_public: &[u8; 65],
) -> Option<Zeroizing<[u8; 32]>> {
    let mut ikm = Zeroizing::new(Vec::with_capacity(shared.len() + secret.as_bytes().len()));
    ikm.extend_from_slice(shared);
    ikm.extend_from_slice(secret.as_bytes());
    let mut info = Vec::with_capacity(KDF_INFO.len() + 65 + 65);
    info.extend_from_slice(KDF_INFO);
    info.extend_from_slice(ephemeral_public);
    info.extend_from_slice(recipient_public);
    let mut key = Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(session.as_bytes()), ikm.as_slice())
        .expand(&info, key.as_mut_slice())
        .ok()?;
    Some(key)
}

/// Canonical, length-prefixed associated data (ADR 0002 style): domain, table
/// and session ID as `u32 BE` length-prefixed strings, then the `u16 BE`
/// format version, then the length-prefixed algorithm name.
fn aad(session: LoginSessionId) -> Vec<u8> {
    let mut out = Vec::new();
    for field in [AAD_DOMAIN, AAD_TABLE, session.to_string().as_str()] {
        put_str(&mut out, field);
    }
    out.extend_from_slice(&RESULT_FORMAT_VERSION.to_be_bytes());
    put_str(&mut out, RESULT_ALGORITHM);
    out
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&u32::try_from(value.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use bot_domain::SteamId;
    use proptest::prelude::*;
    use uuid::Uuid;

    use super::*;
    use crate::MAX_TOKEN_BYTES;

    // Known-answer inputs; expected outputs were computed with python3
    // `cryptography` (ECDH, HKDF-SHA256, AESGCMSIV) and cross-checked against a
    // stdlib hmac/hashlib HKDF. See the report for provenance.
    const KAT_EPHEMERAL_PUBLIC: &str = "04d65a93977caa3d1b081852ff57a79e465f1660577304baead505dd3a48589cf350185e895372df6221ea3a137557e473fddb6755f05bd507c3c533fce9c91285";
    const KAT_AAD: &str = "000000117869616f2e6c6f67696e2e726573756c740000001772757374706c75735f6c6f67696e5f73657373696f6e730000002430303131323233332d343435352d363637372d383839392d616162626363646465656666000100000020703235362d686b64662d7368613235362d6165732d3235362d67636d2d736976";
    const KAT_CIPHERTEXT: &str =
        "74ba15966596dbb5a8240675fbc3e0bbffd592f4fa445e6eb82e681b9cbc082d531951a4a8e14d2e7b9d2dde";
    const KAT_TOKEN: &str = "tok-ABC123_xyz.-";
    const KAT_STEAM_ID: u64 = 76_561_198_000_000_000;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn kat_session() -> LoginSessionId {
        LoginSessionId::from_uuid(Uuid::from_bytes([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ]))
    }

    fn kat_link() -> LinkSecret {
        let mut bytes = [0u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::try_from(index).unwrap();
        }
        LinkSecret::from_bytes(bytes)
    }

    fn kat_nonce() -> [u8; 12] {
        let mut nonce = [0u8; 12];
        for (index, byte) in nonce.iter_mut().enumerate() {
            *byte = 0xa0 + u8::try_from(index).unwrap();
        }
        nonce
    }

    fn kat_result() -> CompanionLoginResult {
        CompanionLoginResult::new(SteamId::new(KAT_STEAM_ID).unwrap(), KAT_TOKEN.to_string())
            .unwrap()
    }

    fn recipient() -> RecipientSecret {
        RecipientSecret::from_scalar_bytes(&[0x11; 32]).unwrap()
    }

    fn ephemeral() -> SecretKey {
        SecretKey::from_slice(&[0x22; 32]).unwrap()
    }

    fn kat_sealed() -> SealedResult {
        seal_with(
            &ephemeral(),
            kat_nonce(),
            &recipient().public_key(),
            &kat_link(),
            kat_session(),
            &kat_result(),
        )
        .unwrap()
    }

    fn open_kat(sealed: &SealedResult) -> Result<CompanionLoginResult, OpenError> {
        open(&recipient(), &kat_link(), kat_session(), sealed)
    }

    #[test]
    fn known_answer_matches_independent_implementation() {
        let sealed = kat_sealed();
        assert_eq!(hex(&sealed.ephemeral_public_key), KAT_EPHEMERAL_PUBLIC);
        assert_eq!(sealed.nonce, kat_nonce());
        assert_eq!(hex(&sealed.ciphertext), KAT_CIPHERTEXT);
    }

    #[test]
    fn known_answer_opens() {
        let opened = open_kat(&kat_sealed()).unwrap();
        assert_eq!(opened.steam_id().value(), KAT_STEAM_ID);
        assert_eq!(opened.token(), KAT_TOKEN);
    }

    #[test]
    fn aad_matches_independent_vector() {
        assert_eq!(hex(&aad(kat_session())), KAT_AAD);
        assert_eq!(RESULT_FORMAT_VERSION, 1);
        assert_eq!(RESULT_ALGORITHM, "p256-hkdf-sha256-aes-256-gcm-siv");
    }

    #[test]
    fn public_seal_round_trips_with_fresh_randomness() {
        let secret = LinkSecret::generate();
        let recipient = RecipientSecret::generate();
        let session = LoginSessionId::generate();
        let sealed = seal(&recipient.public_key(), &secret, session, &kat_result()).unwrap();
        let again = seal(&recipient.public_key(), &secret, session, &kat_result()).unwrap();
        assert_ne!(sealed.ephemeral_public_key, again.ephemeral_public_key);
        assert_ne!(sealed.nonce, again.nonce);
        assert_ne!(sealed.ciphertext, again.ciphertext);
        let opened = open(&recipient, &secret, session, &sealed).unwrap();
        assert_eq!(opened.token(), KAT_TOKEN);
        assert_eq!(opened.steam_id().value(), KAT_STEAM_ID);
    }

    #[test]
    fn ciphertext_does_not_contain_the_token() {
        let sealed = kat_sealed();
        assert!(
            !sealed
                .ciphertext
                .windows(KAT_TOKEN.len())
                .any(|window| window == KAT_TOKEN.as_bytes())
        );
    }

    #[test]
    fn wrong_link_secret_is_rejected() {
        let wrong = LinkSecret::from_bytes([9; 32]);
        let error = open(&recipient(), &wrong, kat_session(), &kat_sealed()).unwrap_err();
        assert_eq!(error, OpenError::Decryption);
    }

    #[test]
    fn wrong_session_id_is_rejected() {
        // A different session changes both the HKDF salt and the AAD.
        let other = LoginSessionId::from_uuid(Uuid::from_bytes([1; 16]));
        let error = open(&recipient(), &kat_link(), other, &kat_sealed()).unwrap_err();
        assert_eq!(error, OpenError::Decryption);
    }

    #[test]
    fn wrong_recipient_secret_is_rejected() {
        let other = RecipientSecret::from_scalar_bytes(&[0x12; 32]).unwrap();
        let error = open(&other, &kat_link(), kat_session(), &kat_sealed()).unwrap_err();
        assert_eq!(error, OpenError::Decryption);
    }

    #[test]
    fn every_ciphertext_bit_flip_is_rejected() {
        let sealed = kat_sealed();
        for byte in 0..sealed.ciphertext.len() {
            for bit in 0..8 {
                let mut tampered = sealed.clone();
                tampered.ciphertext[byte] ^= 1 << bit;
                assert_eq!(
                    open_kat(&tampered).unwrap_err(),
                    OpenError::Decryption,
                    "{byte}:{bit}"
                );
            }
        }
    }

    #[test]
    fn every_nonce_bit_flip_is_rejected() {
        let sealed = kat_sealed();
        for byte in 0..sealed.nonce.len() {
            for bit in 0..8 {
                let mut tampered = sealed.clone();
                tampered.nonce[byte] ^= 1 << bit;
                assert_eq!(
                    open_kat(&tampered).unwrap_err(),
                    OpenError::Decryption,
                    "{byte}:{bit}"
                );
            }
        }
    }

    #[test]
    fn every_ephemeral_key_bit_flip_is_rejected() {
        let sealed = kat_sealed();
        for byte in 0..sealed.ephemeral_public_key.len() {
            for bit in 0..8 {
                let mut tampered = sealed.clone();
                tampered.ephemeral_public_key[byte] ^= 1 << bit;
                assert!(open_kat(&tampered).is_err(), "{byte}:{bit}");
            }
        }
    }

    #[test]
    fn a_valid_but_different_ephemeral_key_fails_authentication() {
        let mut sealed = kat_sealed();
        let other = SecretKey::from_slice(&[0x23; 32]).unwrap();
        sealed.ephemeral_public_key =
            RecipientKey::from_public(other.public_key()).to_uncompressed();
        assert_eq!(open_kat(&sealed).unwrap_err(), OpenError::Decryption);
    }

    #[test]
    fn invalid_ephemeral_keys_are_rejected_before_any_crypto() {
        let good = kat_sealed();
        let mut cases: Vec<(&str, [u8; 65])> = Vec::new();
        cases.push(("all zero (identity encoding)", [0u8; 65]));
        let mut zeros_with_prefix = [0u8; 65];
        zeros_with_prefix[0] = 0x04;
        cases.push(("0x04 zero coordinates", zeros_with_prefix));
        let mut off_curve = good.ephemeral_public_key;
        off_curve[64] ^= 1;
        cases.push(("off curve", off_curve));
        for prefix in [0x00u8, 0x02, 0x03, 0x05, 0xff] {
            let mut wrong_prefix = good.ephemeral_public_key;
            wrong_prefix[0] = prefix;
            cases.push(("wrong prefix", wrong_prefix));
        }
        for (name, key) in cases {
            let mut sealed = good.clone();
            sealed.ephemeral_public_key = key;
            assert_eq!(
                open_kat(&sealed).unwrap_err(),
                OpenError::InvalidEphemeralKey,
                "{name}"
            );
        }
    }

    #[test]
    fn ciphertext_length_bounds() {
        assert_eq!(MIN_CIPHERTEXT_BYTES, 13 + 16);
        assert_eq!(MAX_CIPHERTEXT_BYTES, 12 + MAX_TOKEN_BYTES + 16);
        let good = kat_sealed();
        for length in [
            0usize,
            1,
            15,
            16,
            MIN_CIPHERTEXT_BYTES - 1,
            MAX_CIPHERTEXT_BYTES + 1,
            100_000,
        ] {
            let mut sealed = good.clone();
            sealed.ciphertext = vec![0u8; length];
            assert_eq!(
                open_kat(&sealed).unwrap_err(),
                OpenError::InvalidCiphertextLength,
                "{length}"
            );
        }
        // Truncating a real ciphertext is caught by the bounds or the tag.
        for cut in 0..good.ciphertext.len() {
            let mut sealed = good.clone();
            sealed.ciphertext.truncate(cut);
            assert!(open_kat(&sealed).is_err(), "{cut}");
        }
        // Lengths at the bounds pass the length gate and fail authentication.
        for length in [MIN_CIPHERTEXT_BYTES, MAX_CIPHERTEXT_BYTES] {
            let mut sealed = good.clone();
            sealed.ciphertext = vec![0u8; length];
            assert_eq!(
                open_kat(&sealed).unwrap_err(),
                OpenError::Decryption,
                "{length}"
            );
        }
    }

    #[test]
    fn maximum_size_result_round_trips_at_the_ciphertext_bound() {
        let result =
            CompanionLoginResult::new(SteamId::new(1).unwrap(), "a".repeat(MAX_TOKEN_BYTES))
                .unwrap();
        let sealed = seal(
            &recipient().public_key(),
            &kat_link(),
            kat_session(),
            &result,
        )
        .unwrap();
        assert_eq!(sealed.ciphertext.len(), MAX_CIPHERTEXT_BYTES);
        assert_eq!(open_kat(&sealed).unwrap().token().len(), MAX_TOKEN_BYTES);
    }

    fn seal_raw(plaintext: &[u8]) -> Result<SealedResult, SealError> {
        seal_plaintext_with(
            &ephemeral(),
            kat_nonce(),
            &recipient().public_key(),
            &kat_link(),
            kat_session(),
            plaintext,
        )
    }

    #[test]
    fn authentic_but_malformed_plaintexts_are_rejected() {
        let good = kat_result().encode();
        let mut trailing = good.to_vec();
        trailing.push(0);
        let mut short_len = good.to_vec();
        short_len[8..12].copy_from_slice(&3u32.to_be_bytes());
        let mut long_len = good.to_vec();
        long_len[8..12].copy_from_slice(&1000u32.to_be_bytes());
        let mut zero_steam = good.to_vec();
        zero_steam[..8].copy_from_slice(&0u64.to_be_bytes());
        let mut bad_token = good.to_vec();
        bad_token[12] = b' ';
        let mut bad_utf8 = good.to_vec();
        bad_utf8[12] = 0xff;
        let too_short = vec![0u8; MIN_PLAINTEXT_BYTES - 1];
        let mut empty_token = vec![0u8; 12];
        empty_token[7] = 1;
        let cases: [(&str, Vec<u8>); 8] = [
            ("trailing byte", trailing),
            ("length smaller than token", short_len),
            ("length larger than token", long_len),
            ("zero steam id", zero_steam),
            ("whitespace in token", bad_token),
            ("invalid utf-8", bad_utf8),
            ("shorter than minimum", too_short),
            ("empty token", empty_token),
        ];
        for (name, plaintext) in cases {
            // Those at or above the minimum length also pass the ciphertext
            // length gate; the short one is stopped there instead.
            let sealed = seal_raw(&plaintext).unwrap();
            let error = open_kat(&sealed).unwrap_err();
            let expected = if plaintext.len() < MIN_PLAINTEXT_BYTES {
                OpenError::InvalidCiphertextLength
            } else {
                OpenError::InvalidPlaintext
            };
            assert_eq!(error, expected, "{name}");
        }
    }

    #[test]
    fn oversized_plaintext_is_refused_at_seal_time() {
        let error = seal_raw(&vec![1u8; MAX_PLAINTEXT_BYTES + 1]).unwrap_err();
        assert_eq!(error, SealError::InvalidPlaintextLength);
        assert!(seal_raw(&vec![1u8; MAX_PLAINTEXT_BYTES]).is_ok());
    }

    #[test]
    fn error_messages_carry_no_secret_data() {
        for message in [
            OpenError::InvalidEphemeralKey.to_string(),
            OpenError::InvalidCiphertextLength.to_string(),
            OpenError::KeyDerivation.to_string(),
            OpenError::Decryption.to_string(),
            OpenError::InvalidPlaintext.to_string(),
            SealError::InvalidPlaintextLength.to_string(),
            SealError::KeyDerivation.to_string(),
            SealError::Encryption.to_string(),
        ] {
            assert!(!message.contains(KAT_TOKEN));
            assert!(!message.is_empty());
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        #[test]
        fn seal_open_round_trip(
            steam_id in 1u64..=u64::MAX,
            token in "[!-~]{1,300}",
        ) {
            let result = CompanionLoginResult::new(SteamId::new(steam_id).unwrap(), token.clone()).unwrap();
            let secret = LinkSecret::generate();
            let recipient = RecipientSecret::generate();
            let session = LoginSessionId::generate();
            let sealed = seal(&recipient.public_key(), &secret, session, &result).unwrap();
            prop_assert!(sealed.ciphertext.len() >= MIN_CIPHERTEXT_BYTES);
            prop_assert!(sealed.ciphertext.len() <= MAX_CIPHERTEXT_BYTES);
            let opened = open(&recipient, &secret, session, &sealed).unwrap();
            prop_assert_eq!(opened.steam_id().value(), steam_id);
            prop_assert_eq!(opened.token(), token.as_str());
        }

        #[test]
        fn seal_open_round_trip_non_ascii(
            steam_id in 1u64..=u64::MAX,
            token in "[\u{c0}-\u{24f}\u{4e00}-\u{4e50}]{1,100}",
        ) {
            let result = CompanionLoginResult::new(SteamId::new(steam_id).unwrap(), token.clone()).unwrap();
            let secret = LinkSecret::generate();
            let recipient = RecipientSecret::generate();
            let session = LoginSessionId::generate();
            let sealed = seal(&recipient.public_key(), &secret, session, &result).unwrap();
            let opened = open(&recipient, &secret, session, &sealed).unwrap();
            prop_assert_eq!(opened.token(), token.as_str());
        }

        #[test]
        fn any_single_bit_flip_never_opens(
            token in "[!-~]{1,64}",
            position in 0usize..4096,
        ) {
            let result = CompanionLoginResult::new(SteamId::new(7).unwrap(), token).unwrap();
            let secret = LinkSecret::generate();
            let recipient = RecipientSecret::generate();
            let session = LoginSessionId::generate();
            let mut sealed = seal(&recipient.public_key(), &secret, session, &result).unwrap();
            let bit = position % (sealed.ciphertext.len() * 8);
            sealed.ciphertext[bit / 8] ^= 1 << (bit % 8);
            prop_assert!(open(&recipient, &secret, session, &sealed).is_err());
        }
    }
}
