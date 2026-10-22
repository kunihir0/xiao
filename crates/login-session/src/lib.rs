//! Contract shared by the bot and the `xiao-login` service (ADR 0003).
//!
//! This leaf crate holds only what both processes must agree on: the link
//! secret and the identifiers derived from it, the session state machine, the
//! sealed result format, and the service-side broker port. It has no async
//! runtime and no database dependency, so it can sit under both
//! `bot-application` and `xiao-login`.

mod broker;
mod confirm;
mod fragment;
mod keys;
mod result;
mod seal;
mod secret;
mod state;

use std::time::Duration;

pub use broker::{BoxFuture, BrokerError, ClaimedSession, LoginSessionBroker};
pub use confirm::ConfirmCode;
pub use fragment::{Fragment, FragmentError};
pub use keys::{KeyError, RecipientKey, RecipientSecret};
pub use result::{CompanionLoginResult, ResultError};
pub use seal::{
    MAX_CIPHERTEXT_BYTES, MIN_CIPHERTEXT_BYTES, OpenError, RESULT_ALGORITHM, RESULT_FORMAT_VERSION,
    SealError, SealedResult, open, seal,
};
pub use secret::{LINK_SECRET_BASE64_LEN, LinkSecret, LoginSessionId, LookupHash, SecretError};
pub use state::{FailureCode, LoginState, ParseFailureCodeError, ParseLoginStateError};

/// How long a login session stays live. The database clock decides expiry.
pub const SESSION_TTL: Duration = Duration::from_secs(10 * 60);
/// Largest accepted companion token, in bytes.
pub const MAX_TOKEN_BYTES: usize = 8192;
/// Largest accepted `ReactNativeWebView.postMessage` payload, in bytes.
pub const MAX_BRIDGE_PAYLOAD_BYTES: usize = 16 * 1024;
/// Number of characters in the human-checked confirm code.
pub const CONFIRM_CODE_LEN: usize = 6;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_constants_match_adr_0003() {
        assert_eq!(SESSION_TTL, Duration::from_secs(600));
        assert_eq!(MAX_TOKEN_BYTES, 8192);
        assert_eq!(MAX_BRIDGE_PAYLOAD_BYTES, 16 * 1024);
        assert_eq!(CONFIRM_CODE_LEN, 6);
    }
}
