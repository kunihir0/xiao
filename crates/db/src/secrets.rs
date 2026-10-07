use aes_gcm_siv::{
    Aes256GcmSiv, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use rand::{RngExt, rng};
use std::{collections::HashMap, fmt, path::Path};
use thiserror::Error;
use zeroize::Zeroizing;

use bot_application::{BoxFuture, SecretFileError, load_owner_only_secret};

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const MAX_WRAPPED: usize = 1024;
const MAX_CIPHERTEXT: usize = 64 * 1024;
const MAX_KEYRING_BYTES: u64 = 64 * 1024;

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct KeyId(String);

impl KeyId {
    pub fn new(value: impl Into<String>) -> Result<Self, SecretError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(SecretError::InvalidKeyId);
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl AsRef<str> for KeyId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}
impl fmt::Debug for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("KeyId").field(&self.0).finish()
    }
}
impl fmt::Display for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

pub struct DataKey(Zeroizing<[u8; 32]>);
impl DataKey {
    pub fn random() -> Self {
        let mut key = [0; 32];
        rng().fill(&mut key);
        Self(Zeroizing::new(key))
    }
    fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }
    fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}
impl Clone for DataKey {
    fn clone(&self) -> Self {
        Self::from_bytes(*self.bytes())
    }
}
impl fmt::Debug for DataKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DataKey([REDACTED])")
    }
}

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("invalid key id")]
    InvalidKeyId,
    #[error("invalid secret envelope")]
    InvalidEnvelope,
    #[error("unsupported secret envelope")]
    UnsupportedEnvelope,
    #[error("secret encryption failed")]
    Encryption,
    #[error("secret decryption failed")]
    Decryption,
    #[error("secret key unavailable")]
    KeyUnavailable,
}

#[derive(Debug, Error)]
pub enum KeyringError {
    #[error("keyring file could not be read")]
    Read,
    #[error("keyring file is too large")]
    TooLarge,
    #[error("keyring file has insecure permissions")]
    InsecurePermissions,
    #[error("keyring line is malformed")]
    Malformed,
    #[error("keyring contains duplicate key id")]
    Duplicate,
    #[error("keyring contains invalid key material")]
    InvalidKey,
    #[error("active key id is not present")]
    UnknownActive,
    #[error("keyring is empty")]
    Empty,
    #[error("invalid key id")]
    InvalidKeyId,
}

pub trait KeyProvider: Send + Sync {
    fn active_key_id(&self) -> &KeyId;
    fn wrap<'a>(
        &'a self,
        key_id: &'a KeyId,
        dek: &'a DataKey,
        aad: &'a [u8],
    ) -> BoxFuture<'a, Result<Vec<u8>, SecretError>>;
    fn unwrap<'a>(
        &'a self,
        key_id: &'a KeyId,
        wrapped: &'a [u8],
        aad: &'a [u8],
    ) -> BoxFuture<'a, Result<DataKey, SecretError>>;
}

pub struct FileKeyring {
    active: KeyId,
    keys: HashMap<KeyId, Zeroizing<[u8; 32]>>,
}
fn map_keyring_file_error(error: SecretFileError) -> KeyringError {
    match error {
        SecretFileError::Missing | SecretFileError::Read | SecretFileError::Unsupported => {
            KeyringError::Read
        }
        SecretFileError::TooLarge => KeyringError::TooLarge,
        SecretFileError::NotRegular
        | SecretFileError::InsecurePermissions
        | SecretFileError::WrongOwner => KeyringError::InsecurePermissions,
    }
}

impl FileKeyring {
    pub fn load(path: &Path, active_key_id: &str) -> Result<Self, KeyringError> {
        let contents = load_owner_only_secret(path, MAX_KEYRING_BYTES as usize)
            .map_err(map_keyring_file_error)?;
        let mut entries = Vec::new();
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (id, hex) = line.split_once(':').ok_or(KeyringError::Malformed)?;
            let id = KeyId::new(id.trim()).map_err(|_| KeyringError::InvalidKeyId)?;
            let hex = hex.trim();
            if hex.len() != 64 {
                return Err(KeyringError::InvalidKey);
            }
            let mut bytes = Zeroizing::new([0u8; 32]);
            let (pairs, remainder) = hex.as_bytes().as_chunks::<2>();
            if !remainder.is_empty() {
                return Err(KeyringError::InvalidKey);
            }
            for (index, pair) in pairs.iter().enumerate() {
                bytes[index] = decode_hex(pair).ok_or(KeyringError::InvalidKey)?;
            }
            entries.push((id, bytes));
        }
        Self::from_zeroizing_entries(active_key_id, entries)
    }
    pub fn from_keys(
        active: impl AsRef<str>,
        keys: Vec<(KeyId, [u8; 32])>,
    ) -> Result<Self, KeyringError> {
        Self::from_zeroizing_entries(
            active.as_ref(),
            keys.into_iter()
                .map(|(id, key)| (id, Zeroizing::new(key)))
                .collect(),
        )
    }
    fn from_zeroizing_entries(
        active: &str,
        entries: Vec<(KeyId, Zeroizing<[u8; 32]>)>,
    ) -> Result<Self, KeyringError> {
        if entries.is_empty() {
            return Err(KeyringError::Empty);
        }
        let active = KeyId::new(active).map_err(|_| KeyringError::InvalidKeyId)?;
        let mut keys = HashMap::with_capacity(entries.len());
        for (id, bytes) in entries {
            if keys.insert(id, bytes).is_some() {
                return Err(KeyringError::Duplicate);
            }
        }
        if !keys.contains_key(&active) {
            return Err(KeyringError::UnknownActive);
        }
        Ok(Self { active, keys })
    }
}
impl fmt::Debug for FileKeyring {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut ids: Vec<_> = self.keys.keys().map(KeyId::as_str).collect();
        ids.sort_unstable();
        f.debug_struct("FileKeyring")
            .field("active", &self.active)
            .field("key_ids", &ids)
            .finish()
    }
}
impl KeyProvider for FileKeyring {
    fn active_key_id(&self) -> &KeyId {
        &self.active
    }
    fn wrap<'a>(
        &'a self,
        key_id: &'a KeyId,
        dek: &'a DataKey,
        aad: &'a [u8],
    ) -> BoxFuture<'a, Result<Vec<u8>, SecretError>> {
        Box::pin(async move {
            let kek = self.keys.get(key_id).ok_or(SecretError::KeyUnavailable)?;
            let mut nonce = [0u8; NONCE_LEN];
            rng().fill(&mut nonce);
            let encrypted = cipher(kek)?
                .encrypt(
                    &Nonce::from(nonce),
                    Payload {
                        msg: dek.bytes(),
                        aad,
                    },
                )
                .map_err(|_| SecretError::Encryption)?;
            let mut out = Vec::with_capacity(NONCE_LEN + encrypted.len());
            out.extend_from_slice(&nonce);
            out.extend_from_slice(&encrypted);
            Ok(out)
        })
    }
    fn unwrap<'a>(
        &'a self,
        key_id: &'a KeyId,
        wrapped: &'a [u8],
        aad: &'a [u8],
    ) -> BoxFuture<'a, Result<DataKey, SecretError>> {
        Box::pin(async move {
            if wrapped.len() != NONCE_LEN + 32 + TAG_LEN {
                return Err(SecretError::Decryption);
            }
            let kek = self.keys.get(key_id).ok_or(SecretError::KeyUnavailable)?;
            let plain = Zeroizing::new(
                cipher(kek)?
                    .decrypt(
                        nonce_ref(&wrapped[..NONCE_LEN])?,
                        Payload {
                            msg: &wrapped[NONCE_LEN..],
                            aad,
                        },
                    )
                    .map_err(|_| SecretError::Decryption)?,
            );
            let bytes: [u8; 32] = plain
                .as_slice()
                .try_into()
                .map_err(|_| SecretError::Decryption)?;
            Ok(DataKey::from_bytes(bytes))
        })
    }
}
fn decode_hex(pair: &[u8]) -> Option<u8> {
    Some((hex_digit(pair[0])? << 4) | hex_digit(pair[1])?)
}
fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
fn cipher(key: &[u8; 32]) -> Result<Aes256GcmSiv, SecretError> {
    Aes256GcmSiv::new_from_slice(key).map_err(|_| SecretError::KeyUnavailable)
}
fn nonce_ref(bytes: &[u8]) -> Result<&Nonce, SecretError> {
    <&Nonce>::try_from(bytes).map_err(|_| SecretError::UnsupportedEnvelope)
}

#[allow(dead_code)]
pub(crate) struct Envelope {
    pub format_version: i16,
    #[allow(dead_code)]
    pub algorithm: String,
    pub key_id: KeyId,
    pub wrapped_dek: Vec<u8>,
    pub nonce: [u8; NONCE_LEN],
    pub ciphertext: Vec<u8>,
}
pub(crate) fn aad(table: &str, record_id: &str, kind: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for field in ["xiao.secret", table, record_id, kind] {
        out.extend_from_slice(&(field.len() as u32).to_be_bytes());
        out.extend_from_slice(field.as_bytes());
    }
    out.extend_from_slice(&1u16.to_be_bytes());
    put_str(&mut out, "aes-256-gcm-siv");
    out
}
pub(crate) fn wrap_aad(data_aad: &[u8], key_id: &KeyId) -> Vec<u8> {
    let mut out = Vec::with_capacity(data_aad.len() + 4 + key_id.as_str().len());
    out.extend_from_slice(data_aad);
    put_str(&mut out, key_id.as_str());
    out
}
fn put_str(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
}
pub(crate) async fn seal(
    provider: &dyn KeyProvider,
    table: &str,
    record_id: &str,
    kind: &str,
    plaintext: &[u8],
) -> Result<Envelope, SecretError> {
    let data_key = DataKey::random();
    let key_id = provider.active_key_id().clone();
    let data_aad = aad(table, record_id, kind);
    let wrap_aad = wrap_aad(&data_aad, &key_id);
    let wrapped_dek = provider.wrap(&key_id, &data_key, &wrap_aad).await?;
    if wrapped_dek.len() > MAX_WRAPPED {
        return Err(SecretError::Encryption);
    }
    let mut nonce = [0; NONCE_LEN];
    rng().fill(&mut nonce);
    let ciphertext = cipher(data_key.bytes())?
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: plaintext,
                aad: &data_aad,
            },
        )
        .map_err(|_| SecretError::Encryption)?;
    Ok(Envelope {
        format_version: 1,
        algorithm: "aes-256-gcm-siv".to_owned(),
        key_id,
        wrapped_dek,
        nonce,
        ciphertext,
    })
}
#[allow(clippy::too_many_arguments)]
pub(crate) async fn open(
    provider: &dyn KeyProvider,
    table: &str,
    record_id: &str,
    kind: &str,
    format_version: i16,
    algorithm: &str,
    key_id: &str,
    wrapped_dek: &[u8],
    nonce: &[u8],
    ciphertext: &[u8],
) -> Result<Zeroizing<Vec<u8>>, SecretError> {
    if format_version != 1
        || algorithm != "aes-256-gcm-siv"
        || wrapped_dek.len() > MAX_WRAPPED
        || nonce.len() != NONCE_LEN
        || ciphertext.len() > MAX_CIPHERTEXT
        || ciphertext.len() < TAG_LEN
    {
        return Err(SecretError::UnsupportedEnvelope);
    }
    let key_id = KeyId::new(key_id).map_err(|_| SecretError::InvalidEnvelope)?;
    let data_aad = aad(table, record_id, kind);
    let wrap = wrap_aad(&data_aad, &key_id);
    let dek = provider.unwrap(&key_id, wrapped_dek, &wrap).await?;
    cipher(dek.bytes())?
        .decrypt(
            nonce_ref(nonce)?,
            Payload {
                msg: ciphertext,
                aad: &data_aad,
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| SecretError::Decryption)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use std::io::Write;

    fn keyring() -> FileKeyring {
        FileKeyring::from_keys("active", vec![(KeyId::new("active").expect("id"), [7; 32])])
            .expect("keyring")
    }

    #[tokio::test]
    async fn envelope_round_trip_and_tamper_detection() {
        let provider = keyring();
        let mut envelope = seal(&provider, "table", "record", "kind", b"secret")
            .await
            .expect("seal");
        let plain = open(
            &provider,
            "table",
            "record",
            "kind",
            envelope.format_version,
            &envelope.algorithm,
            envelope.key_id.as_str(),
            &envelope.wrapped_dek,
            &envelope.nonce,
            &envelope.ciphertext,
        )
        .await
        .expect("open");
        assert_eq!(&*plain, b"secret");
        envelope.ciphertext[0] ^= 1;
        assert!(matches!(
            open(
                &provider,
                "table",
                "record",
                "kind",
                envelope.format_version,
                &envelope.algorithm,
                envelope.key_id.as_str(),
                &envelope.wrapped_dek,
                &envelope.nonce,
                &envelope.ciphertext
            )
            .await,
            Err(SecretError::Decryption)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn keyring_parser_and_debug_redact_material() {
        let path = std::env::temp_dir().join(format!("xiao-keyring-{}", uuid::Uuid::new_v4()));
        let mut file = std::fs::File::create(&path).expect("create");
        writeln!(file, "# comment\n\n active:{}", "ab".repeat(32)).expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
        }
        let loaded = FileKeyring::load(&path, "active").expect("load");
        assert_eq!(loaded.active_key_id().as_str(), "active");
        assert!(!format!("{loaded:?}").contains(&"ab".repeat(32)));
        assert!(!format!("{:?}", DataKey::random()).contains("0000"));
        assert!(!format!("{:?}", KeyId::new("active").expect("id")).contains("secret"));
        std::fs::remove_file(path).expect("remove");
    }

    #[test]
    fn keyring_rejects_duplicates_and_unknown_active() {
        let id = KeyId::new("active").expect("id");
        assert!(matches!(
            FileKeyring::from_keys("active", vec![(id.clone(), [1; 32]), (id, [2; 32])]),
            Err(KeyringError::Duplicate)
        ));
        assert!(matches!(
            FileKeyring::from_keys(
                "missing",
                vec![(KeyId::new("active").expect("id"), [1; 32])]
            ),
            Err(KeyringError::UnknownActive)
        ));
        assert!(matches!(
            FileKeyring::from_keys("active", Vec::new()),
            Err(KeyringError::Empty)
        ));
    }
}
