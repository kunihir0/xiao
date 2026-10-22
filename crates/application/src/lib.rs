//! Use cases and ports; independent of infrastructure.

mod companion;
mod future;
mod login;
mod ports;
mod secret_file;
mod session;

pub use companion::{
    CompanionRegistrar, CredentialError, CredentialId, CredentialState, DeviceId, EnrollmentError,
    EnrollmentKind, EnrollmentOutcome, FcmSecrets, MAX_DEVICE_ID_BYTES,
    MAX_REGISTRATION_TOKEN_BYTES, MAX_STEAM_AUTH_TOKEN_BYTES, PushCredential, PushCredentialStore,
    RegistrationError, SteamAuthToken, SteamAuthTokenSource, ensure_companion_registration,
    reregister_companion,
};
pub use future::BoxFuture;
pub use login::{
    AccountSwitch, AuditOutcome, CompanionLoginService, Decision, DiscordUserId,
    InvalidDiscordUserId, LoginAudit, LoginEvent, LoginFailure, LoginOutcome, LoginSessions,
    LoginStartError, LoginStatus, LoginTicket, NewLoginSession, OneShotToken, OutcomeCode,
    RegistrationFailure, SwitchError, steam_token_from_login,
};
pub use ports::*;
pub use secret_file::{SecretFileError, load_owner_only_secret};
pub use session::{
    PairingService, PairingServiceHandle, SessionConfig, SessionSnapshot, SessionState,
};
