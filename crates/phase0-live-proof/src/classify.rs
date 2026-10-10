//! Maps typed upstream errors onto the manifest's fixed status classes and
//! fixed detail strings. No upstream message text is ever forwarded.

use bot_application::{RegistrationError, SessionError};

use crate::{
    manifest::{Route, Status, StatusClass},
    sequence::StageOutcome,
};

/// A stage failure: only a fixed class and a fixed detail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Failure {
    pub class: StatusClass,
    pub detail: &'static str,
}

impl Failure {
    #[must_use]
    pub const fn new(class: StatusClass, detail: &'static str) -> Self {
        Self { class, detail }
    }

    #[must_use]
    pub const fn outcome(self, route: Route) -> StageOutcome {
        StageOutcome::fail(route, self.class, self.detail)
    }
}

#[must_use]
pub fn classify_registration(error: &RegistrationError) -> Failure {
    match error {
        RegistrationError::ReenrollmentRequired => {
            Failure::new(StatusClass::ReenrollmentRequired, "auth_token_rejected")
        }
        RegistrationError::AuthTokenUnavailable(_) => {
            Failure::new(StatusClass::AuthTokenUnavailable, "auth_token_unavailable")
        }
        RegistrationError::RateLimited => Failure::new(StatusClass::RateLimited, "rate_limited"),
        RegistrationError::Rejected { .. } => {
            Failure::new(StatusClass::Rejected, "registration_rejected")
        }
        RegistrationError::Timeout(_) => Failure::new(StatusClass::Timeout, "registration_timeout"),
        RegistrationError::Transport(_) => {
            Failure::new(StatusClass::Transport, "registration_transport")
        }
        RegistrationError::InvalidResponse(_) => Failure::new(
            StatusClass::InvalidResponse,
            "registration_invalid_response",
        ),
        RegistrationError::Fcm(_) => Failure::new(StatusClass::FcmFailure, "fcm_failure"),
    }
}

#[must_use]
pub fn classify_session(error: &SessionError) -> Failure {
    match error {
        SessionError::InvalidCredentials => Failure::new(
            StatusClass::InvalidCredentials,
            "session_rejected_credentials",
        ),
        SessionError::ConnectTimeout(_) | SessionError::ShutdownTimeout(_) => {
            Failure::new(StatusClass::Timeout, "session_timeout")
        }
        SessionError::Transport | SessionError::Disconnected | SessionError::ShuttingDown => {
            Failure::new(StatusClass::Transport, "session_transport")
        }
        _ => Failure::new(StatusClass::ProtocolFailure, "session_protocol"),
    }
}

#[must_use]
pub fn classify_client(error: &rustplus_client::Error) -> Failure {
    match error {
        rustplus_client::Error::InsecureEndpoint | rustplus_client::Error::InvalidEndpoint => {
            Failure::new(StatusClass::InvalidEndpoint, "client_endpoint_refused")
        }
        rustplus_client::Error::Timeout(_) | rustplus_client::Error::ShutdownTimeout(_) => {
            Failure::new(StatusClass::Timeout, "client_timeout")
        }
        rustplus_client::Error::Api(_) => Failure::new(StatusClass::Rejected, "client_api_error"),
        rustplus_client::Error::Transport
        | rustplus_client::Error::Io
        | rustplus_client::Error::Disconnected
        | rustplus_client::Error::ShuttingDown => {
            Failure::new(StatusClass::Transport, "client_transport")
        }
        _ => Failure::new(StatusClass::ProtocolFailure, "client_protocol"),
    }
}

/// Negative-auth stage verdict for a registration attempted with a
/// synthetic, invalid Steam token.
///
/// A 401/403 maps to `reenrollment_required`; any other typed service
/// rejection is recorded as its actual class. Success, throttling, and
/// transport-level errors prove nothing about token validation and fail the
/// stage.
#[must_use]
pub fn negative_auth_outcome(result: Result<(), &RegistrationError>) -> StageOutcome {
    const ROUTE: Route = Route::FacepunchRegister;
    match result {
        Err(RegistrationError::ReenrollmentRequired) => {
            StageOutcome::pass(ROUTE, "invalid_token_rejected_expiry_not_run")
                .with_status_class(StatusClass::ReenrollmentRequired)
        }
        Err(error @ RegistrationError::Rejected { .. }) => {
            StageOutcome::pass(ROUTE, "invalid_token_typed_rejection_expiry_not_run")
                .with_status_class(classify_registration(error).class)
        }
        Ok(()) => StageOutcome::fail(
            ROUTE,
            StatusClass::UnexpectedSuccess,
            "invalid_token_accepted",
        ),
        Err(error) => {
            let failure = classify_registration(error);
            StageOutcome {
                status: Status::Fail,
                status_class: failure.class,
                detail: "negative_auth_inconclusive",
                ..StageOutcome::pass(ROUTE, "")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_errors_map_to_fixed_classes() {
        assert_eq!(
            classify_registration(&RegistrationError::ReenrollmentRequired).class,
            StatusClass::ReenrollmentRequired
        );
        assert_eq!(
            classify_registration(&RegistrationError::Fcm("secret-token-text".to_owned())),
            Failure::new(StatusClass::FcmFailure, "fcm_failure")
        );
        assert_eq!(
            classify_registration(&RegistrationError::Rejected {
                service: "expo",
                status: 500
            })
            .class,
            StatusClass::Rejected
        );
    }

    #[test]
    fn negative_auth_requires_a_typed_rejection() {
        let reenroll = negative_auth_outcome(Err(&RegistrationError::ReenrollmentRequired));
        assert_eq!(reenroll.status, Status::Pass);
        assert_eq!(reenroll.status_class, StatusClass::ReenrollmentRequired);

        let rejected = negative_auth_outcome(Err(&RegistrationError::Rejected {
            service: "facepunch",
            status: 400,
        }));
        assert_eq!(rejected.status, Status::Pass);
        assert_eq!(rejected.status_class, StatusClass::Rejected);

        assert_eq!(negative_auth_outcome(Ok(())).status, Status::Fail);
        for error in [
            RegistrationError::RateLimited,
            RegistrationError::Timeout("facepunch"),
            RegistrationError::Transport("facepunch"),
        ] {
            let outcome = negative_auth_outcome(Err(&error));
            assert_eq!(outcome.status, Status::Fail);
            assert_eq!(outcome.detail, "negative_auth_inconclusive");
        }
    }
}
