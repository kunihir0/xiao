//! Use cases and ports; independent of infrastructure.

mod companion;
mod future;
mod ports;
mod session;

pub use companion::{
    CompanionRegistrar, CredentialError, CredentialId, CredentialState, DeviceId, EnrollmentError,
    EnrollmentKind, EnrollmentOutcome, FcmSecrets, MAX_DEVICE_ID_BYTES,
    MAX_REGISTRATION_TOKEN_BYTES, MAX_STEAM_AUTH_TOKEN_BYTES, PushCredential, PushCredentialStore,
    RegistrationError, SteamAuthToken, SteamAuthTokenSource, ensure_companion_registration,
};
pub use future::BoxFuture;
pub use ports::*;
pub use session::{
    PairingService, PairingServiceHandle, SessionConfig, SessionSnapshot, SessionState,
};
