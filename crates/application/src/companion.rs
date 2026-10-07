//! Rust+ companion push-credential lifecycle.
//!
//! The use case owns the ordering contract: restore an active registration
//! without network calls; otherwise persist fresh FCM material as `pending`
//! before calling Facepunch, then promote it atomically. A crash or rejected
//! registration leaves the pending row for the next attempt, so a retry reuses
//! the same device identity and keys instead of minting a new one.

use std::fmt;

use bot_domain::SteamId;
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::{BoxFuture, StoreError};

pub const MAX_REGISTRATION_TOKEN_BYTES: usize = 4096;
pub const MAX_DEVICE_ID_BYTES: usize = 128;
pub const MAX_STEAM_AUTH_TOKEN_BYTES: usize = 8192;

/// Identity of one persisted credential generation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CredentialId(Uuid);

impl CredentialId {
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }

    #[must_use]
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl fmt::Display for CredentialId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

/// Stable companion device identifier sent to Expo/Facepunch. Facepunch keys
/// push routing by Steam account and device ID, so reusing it makes
/// registration replace the existing route instead of adding another one.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DeviceId(String);

impl DeviceId {
    pub fn new(value: impl Into<String>) -> Result<Self, CredentialError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_DEVICE_ID_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(CredentialError::Invalid("device_id"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn generate() -> Self {
        Self(format!("xiao-{}", Uuid::new_v4().simple()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// FCM check-in, registration token, and Web Push key material. Every field
/// is secret; the value is zeroized on drop and never printed.
#[derive(Clone, Eq, PartialEq, Zeroize, ZeroizeOnDrop)]
pub struct FcmSecrets {
    android_id: u64,
    security_token: u64,
    registration_token: String,
    web_push_private_key: [u8; 32],
    web_push_auth_secret: [u8; 16],
}

impl FcmSecrets {
    pub fn new(
        android_id: u64,
        security_token: u64,
        registration_token: String,
        web_push_private_key: [u8; 32],
        web_push_auth_secret: [u8; 16],
    ) -> Result<Self, CredentialError> {
        // Construct first so rejected inputs are still zeroized on drop.
        let secrets = Self {
            android_id,
            security_token,
            registration_token,
            web_push_private_key,
            web_push_auth_secret,
        };
        if secrets.android_id == 0 {
            return Err(CredentialError::Invalid("android_id"));
        }
        if secrets.security_token == 0 {
            return Err(CredentialError::Invalid("security_token"));
        }
        let token = &secrets.registration_token;
        if token.is_empty()
            || token.len() > MAX_REGISTRATION_TOKEN_BYTES
            || token
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err(CredentialError::Invalid("registration_token"));
        }
        Ok(secrets)
    }

    #[must_use]
    pub const fn android_id(&self) -> u64 {
        self.android_id
    }

    #[must_use]
    pub const fn security_token(&self) -> u64 {
        self.security_token
    }

    #[must_use]
    pub fn registration_token(&self) -> &str {
        &self.registration_token
    }

    #[must_use]
    pub const fn web_push_private_key(&self) -> &[u8; 32] {
        &self.web_push_private_key
    }

    #[must_use]
    pub const fn web_push_auth_secret(&self) -> &[u8; 16] {
        &self.web_push_auth_secret
    }
}

impl fmt::Debug for FcmSecrets {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("FcmSecrets([REDACTED])")
    }
}

/// Bearer token issued by the Rust+ companion Steam login. Supplied by the
/// operator out-of-band and never persisted by this application.
pub struct SteamAuthToken(Zeroizing<String>);

impl SteamAuthToken {
    pub fn new(value: String) -> Result<Self, CredentialError> {
        let value = Zeroizing::new(value);
        if value.is_empty()
            || value.len() > MAX_STEAM_AUTH_TOKEN_BYTES
            || value
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err(CredentialError::Invalid("steam_auth_token"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SteamAuthToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SteamAuthToken([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialState {
    /// Persisted FCM material whose Facepunch registration is not confirmed.
    Pending,
    /// Registered with Facepunch; restored on startup without network calls.
    Active,
}

impl CredentialState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
        }
    }
}

/// One companion credential generation. Debug output contains only
/// non-secret metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushCredential {
    pub id: CredentialId,
    pub steam_id: SteamId,
    pub device_id: DeviceId,
    pub state: CredentialState,
    pub fcm: FcmSecrets,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialError {
    Invalid(&'static str),
}

impl fmt::Display for CredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(field) => write!(formatter, "invalid credential field {field}"),
        }
    }
}

impl std::error::Error for CredentialError {}

/// Encrypted persistence of companion push credentials.
///
/// Implementations must never store plaintext secrets and must fail closed
/// (`StoreError::Decryption`) when a row cannot be authenticated.
pub trait PushCredentialStore: Send + Sync {
    fn load_active<'a>(
        &'a self,
        steam_id: SteamId,
    ) -> BoxFuture<'a, Result<Option<PushCredential>, StoreError>>;
    fn load_pending<'a>(
        &'a self,
        steam_id: SteamId,
    ) -> BoxFuture<'a, Result<Option<PushCredential>, StoreError>>;
    /// Inserts a new pending generation. At most one pending generation may
    /// exist per Steam account; a second insert returns `StoreError::Conflict`.
    fn save_pending<'a>(
        &'a self,
        credential: &'a PushCredential,
    ) -> BoxFuture<'a, Result<(), StoreError>>;
    /// Atomically promotes a pending generation and deletes any previously
    /// active generation for the same Steam account.
    fn promote_active<'a>(&'a self, id: CredentialId) -> BoxFuture<'a, Result<(), StoreError>>;
    /// Deletes a generation's local ciphertext. Remote unregistration is not
    /// available from Facepunch; see the operations runbook.
    fn retire<'a>(&'a self, id: CredentialId) -> BoxFuture<'a, Result<(), StoreError>>;
}

/// Typed outbound-registration failures. Variants never carry URLs, headers,
/// bodies, or tokens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistrationError {
    /// Facepunch rejected the Steam auth token (HTTP 401/403). The operator
    /// must supply a fresh token; no new identity is minted automatically.
    ReenrollmentRequired,
    /// The Steam auth token source is missing or unreadable.
    AuthTokenUnavailable(&'static str),
    RateLimited,
    Rejected {
        service: &'static str,
        status: u16,
    },
    Timeout(&'static str),
    Transport(&'static str),
    InvalidResponse(&'static str),
    /// FCM check-in/register/subscribe failed.
    Fcm(String),
}

impl fmt::Display for RegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReenrollmentRequired => formatter.write_str(
                "Rust+ companion auth token rejected; re-enrollment with a fresh token is required",
            ),
            Self::AuthTokenUnavailable(reason) => {
                write!(formatter, "Steam auth token unavailable: {reason}")
            }
            Self::RateLimited => formatter.write_str("companion registration rate limited"),
            Self::Rejected { service, status } => {
                write!(
                    formatter,
                    "{service} rejected registration with HTTP {status}"
                )
            }
            Self::Timeout(service) => write!(formatter, "{service} request timed out"),
            Self::Transport(service) => write!(formatter, "{service} transport failure"),
            Self::InvalidResponse(reason) => {
                write!(formatter, "invalid registration response: {reason}")
            }
            Self::Fcm(message) => write!(formatter, "FCM registration failed: {message}"),
        }
    }
}

impl std::error::Error for RegistrationError {}

/// Outbound FCM + Expo + Facepunch registration.
pub trait CompanionRegistrar: Send + Sync {
    /// Performs a fresh FCM check-in/register/subscribe and returns its secrets.
    fn create_fcm_credentials<'a>(&'a self)
    -> BoxFuture<'a, Result<FcmSecrets, RegistrationError>>;
    /// Exchanges the FCM token for an Expo push token and registers it with
    /// Facepunch for `device_id`. Must be idempotent for identical inputs.
    fn register<'a>(
        &'a self,
        auth_token: &'a SteamAuthToken,
        device_id: &'a DeviceId,
        fcm: &'a FcmSecrets,
    ) -> BoxFuture<'a, Result<(), RegistrationError>>;
}

/// Supplies the operator-provided Steam auth token on demand. It is read only
/// when a registration is actually required.
pub trait SteamAuthTokenSource: Send + Sync {
    fn load<'a>(&'a self) -> BoxFuture<'a, Result<SteamAuthToken, RegistrationError>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnrollmentKind {
    /// An active credential was restored; no network calls were made.
    Restored,
    /// A previously persisted pending credential was registered.
    Resumed,
    /// Fresh FCM credentials were created and registered.
    Registered,
}

#[derive(Debug)]
pub struct EnrollmentOutcome {
    pub kind: EnrollmentKind,
    pub credential: PushCredential,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnrollmentError {
    Store(StoreError),
    Registration(RegistrationError),
}

impl fmt::Display for EnrollmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "credential store failure: {error}"),
            Self::Registration(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for EnrollmentError {}

impl From<StoreError> for EnrollmentError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<RegistrationError> for EnrollmentError {
    fn from(error: RegistrationError) -> Self {
        Self::Registration(error)
    }
}

/// Restores the active companion credential or completes registration.
///
/// Callers must serialize invocations per Steam account (one process owns the
/// account); the store's unique pending/active constraints reject races.
pub async fn ensure_companion_registration(
    store: &dyn PushCredentialStore,
    registrar: &dyn CompanionRegistrar,
    tokens: &dyn SteamAuthTokenSource,
    steam_id: SteamId,
) -> Result<EnrollmentOutcome, EnrollmentError> {
    if let Some(credential) = store.load_active(steam_id).await? {
        return Ok(EnrollmentOutcome {
            kind: EnrollmentKind::Restored,
            credential,
        });
    }
    let (kind, mut credential) = match store.load_pending(steam_id).await? {
        Some(pending) => (EnrollmentKind::Resumed, pending),
        None => {
            let fcm = registrar.create_fcm_credentials().await?;
            let credential = PushCredential {
                id: CredentialId::generate(),
                steam_id,
                device_id: DeviceId::generate(),
                state: CredentialState::Pending,
                fcm,
            };
            store.save_pending(&credential).await?;
            (EnrollmentKind::Registered, credential)
        }
    };
    let auth_token = tokens.load().await?;
    registrar
        .register(&auth_token, &credential.device_id, &credential.fcm)
        .await?;
    store.promote_active(credential.id).await?;
    credential.state = CredentialState::Active;
    Ok(EnrollmentOutcome { kind, credential })
}

#[cfg(test)]
mod tests {
    use super::{DeviceId, FcmSecrets, SteamAuthToken};

    #[test]
    fn secrets_debug_is_redacted() -> Result<(), super::CredentialError> {
        let secrets = FcmSecrets::new(11, 22, "fcm-secret-token".to_owned(), [3; 32], [4; 16])?;
        let debug = format!("{secrets:?}");
        assert!(!debug.contains("fcm-secret-token"));
        assert!(!debug.contains("22"));
        let token = SteamAuthToken::new("steam-bearer".to_owned())?;
        assert!(!format!("{token:?}").contains("steam-bearer"));
        Ok(())
    }

    #[test]
    fn secrets_and_device_ids_are_validated() {
        assert!(FcmSecrets::new(0, 1, "t".to_owned(), [0; 32], [0; 16]).is_err());
        assert!(FcmSecrets::new(1, 0, "t".to_owned(), [0; 32], [0; 16]).is_err());
        assert!(FcmSecrets::new(1, 1, "a b".to_owned(), [0; 32], [0; 16]).is_err());
        assert!(FcmSecrets::new(1, 1, String::new(), [0; 32], [0; 16]).is_err());
        assert!(SteamAuthToken::new(String::new()).is_err());
        assert!(SteamAuthToken::new("x\n".to_owned()).is_err());
        assert!(DeviceId::new("bad id").is_err());
        assert!(DeviceId::new("x".repeat(129)).is_err());
        assert!(DeviceId::new(DeviceId::generate().as_str()).is_ok());
    }
}
