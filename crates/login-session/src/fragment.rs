use std::fmt;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::{LINK_SECRET_BASE64_LEN, LinkSecret, RecipientKey, secret::decode_base64url_exact};

const KEY_BASE64_LEN: usize = 44;

/// Why a fragment was rejected. Only visible to tests; callers see one opaque
/// error so the refusal page can be byte-identical.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Fault {
    Separator,
    Secret,
    Key,
}

/// Opaque rejection of a malformed URL fragment.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("invalid login link fragment")]
pub struct FragmentError(Fault);

/// The URL fragment of the login link: `<base64url(secret)>.<base64url(key)>`.
///
/// The compressed recipient key commits the link to the bot's key, so a
/// database writer cannot redirect the sealed result.
pub struct Fragment {
    pub secret: LinkSecret,
    pub recipient: RecipientKey,
}

impl Fragment {
    /// Strict parse: exactly one `.`, both parts canonical unpadded base64url
    /// of exact length (43 and 44 characters), and a valid compressed key.
    pub fn parse(input: &str) -> Result<Self, FragmentError> {
        let mut parts = input.split('.');
        let (Some(secret_part), Some(key_part), None) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(FragmentError(Fault::Separator));
        };
        let secret =
            LinkSecret::parse_base64url(secret_part).map_err(|_| FragmentError(Fault::Secret))?;
        let key_bytes = decode_base64url_exact::<33>(key_part, KEY_BASE64_LEN)
            .ok_or(FragmentError(Fault::Key))?;
        let recipient = RecipientKey::from_compressed(key_bytes.as_slice())
            .map_err(|_| FragmentError(Fault::Key))?;
        Ok(Self { secret, recipient })
    }
}

impl Fragment {
    /// The fragment text, zeroized on drop. There is deliberately no
    /// `Display`: formatting would copy the link secret into an ordinary
    /// `String` (or a log line) that is never wiped.
    #[must_use]
    pub fn to_zeroizing_string(&self) -> Zeroizing<String> {
        let secret = self.secret.to_base64url();
        let key = URL_SAFE_NO_PAD.encode(self.recipient.to_compressed());
        let mut out = Zeroizing::new(String::with_capacity(
            LINK_SECRET_BASE64_LEN + 1 + KEY_BASE64_LEN,
        ));
        out.push_str(&secret);
        out.push('.');
        out.push_str(&key);
        out
    }
}

impl fmt::Debug for Fragment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Fragment(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use crate::RecipientSecret;

    const KAT_FRAGMENT: &str =
        "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8.AgIX5hfwtkQ5KCePlpmeaaI6TywVK99tbN9m5bgCgtTt";

    fn sample() -> Fragment {
        let mut bytes = [0u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::try_from(index).unwrap();
        }
        Fragment {
            secret: LinkSecret::from_bytes(bytes),
            recipient: RecipientSecret::from_scalar_bytes(&[0x11; 32])
                .unwrap()
                .public_key(),
        }
    }

    #[test]
    fn encodes_to_independent_vector_with_exact_lengths() {
        let text = sample().to_zeroizing_string();
        assert_eq!(text.as_str(), KAT_FRAGMENT);
        assert_eq!(text.len(), 43 + 1 + 44);
    }

    #[test]
    fn parse_round_trips() {
        let parsed = Fragment::parse(KAT_FRAGMENT).unwrap();
        let original = sample();
        assert_eq!(parsed.recipient, original.recipient);
        assert_eq!(
            parsed.secret.to_base64url().as_str(),
            original.secret.to_base64url().as_str()
        );
        assert_eq!(parsed.to_zeroizing_string().as_str(), KAT_FRAGMENT);
    }

    #[test]
    fn random_fragments_round_trip() {
        let fragment = Fragment {
            secret: LinkSecret::generate(),
            recipient: RecipientSecret::generate().public_key(),
        };
        let parsed = Fragment::parse(&fragment.to_zeroizing_string()).unwrap();
        assert_eq!(parsed.recipient, fragment.recipient);
        assert_eq!(parsed.to_zeroizing_string(), fragment.to_zeroizing_string());
    }

    #[test]
    fn rejects_zero_or_multiple_separators() {
        let (secret, key) = KAT_FRAGMENT.split_once('.').unwrap();
        assert_eq!(
            Fragment::parse(&format!("{secret}{key}")).unwrap_err(),
            FragmentError(Fault::Separator)
        );
        assert_eq!(
            Fragment::parse(&format!("{secret}.{key}.")).unwrap_err(),
            FragmentError(Fault::Separator)
        );
        assert_eq!(
            Fragment::parse(&format!("{secret}.{key}.{key}")).unwrap_err(),
            FragmentError(Fault::Separator)
        );
        assert_eq!(
            Fragment::parse("").unwrap_err(),
            FragmentError(Fault::Separator)
        );
    }

    #[test]
    fn rejects_wrong_part_lengths_and_padding() {
        let (secret, key) = KAT_FRAGMENT.split_once('.').unwrap();
        let bad = [
            format!("{}.{key}", &secret[..42]),
            format!("{secret}A.{key}"),
            format!("{secret}=.{key}"),
            format!("{secret}.{}", &key[..43]),
            format!("{secret}.{key}A"),
            format!("{secret}.{key}="),
            format!(".{key}"),
            format!("{secret}."),
            ".".to_string(),
        ];
        for input in bad {
            assert!(Fragment::parse(&input).is_err(), "{input}");
        }
    }

    #[test]
    fn rejects_non_canonical_secret_and_standard_alphabet_key() {
        let (secret, key) = KAT_FRAGMENT.split_once('.').unwrap();
        let trailing = format!("{}B.{key}", &secret[..42]);
        assert_eq!(
            Fragment::parse(&trailing).unwrap_err(),
            FragmentError(Fault::Secret)
        );
        let plus = format!("{secret}.{}+", &key[..43]);
        assert_eq!(
            Fragment::parse(&plus).unwrap_err(),
            FragmentError(Fault::Key)
        );
        let slash = format!("{secret}.{}/", &key[..43]);
        assert_eq!(
            Fragment::parse(&slash).unwrap_err(),
            FragmentError(Fault::Key)
        );
    }

    #[test]
    fn rejects_bad_keys() {
        let (secret, _) = KAT_FRAGMENT.split_once('.').unwrap();
        let cases: [[u8; 33]; 4] = {
            let good = sample().recipient.to_compressed();
            let mut wrong_prefix = good;
            wrong_prefix[0] = 0x04;
            let mut off_curve = [0u8; 33];
            off_curve[0] = 0x02;
            off_curve[32] = 1;
            [wrong_prefix, off_curve, [0u8; 33], [0xffu8; 33]]
        };
        for bytes in cases {
            let input = format!("{secret}.{}", URL_SAFE_NO_PAD.encode(bytes));
            assert_eq!(
                Fragment::parse(&input).unwrap_err(),
                FragmentError(Fault::Key)
            );
        }
    }

    #[test]
    fn rejects_uncompressed_key_in_fragment() {
        let (secret, _) = KAT_FRAGMENT.split_once('.').unwrap();
        let input = format!(
            "{secret}.{}",
            URL_SAFE_NO_PAD.encode(sample().recipient.to_uncompressed())
        );
        assert!(Fragment::parse(&input).is_err());
    }

    #[test]
    fn debug_is_redacted() {
        assert_eq!(format!("{:?}", sample()), "Fragment(<redacted>)");
    }
}
