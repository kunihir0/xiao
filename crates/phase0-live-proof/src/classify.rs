//! Maps typed upstream errors onto the manifest's fixed status classes and
//! fixed detail strings. No upstream message text is ever forwarded.

use bot_application::{RegistrationError, SessionError};

use crate::{
    manifest::{Route, StatusClass},
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
        SessionError::Protocol | SessionError::EventLagged { .. } | SessionError::Failed => {
            Failure::new(StatusClass::ProtocolFailure, "session_protocol")
        }
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
        rustplus_client::Error::Decode
        | rustplus_client::Error::Encode
        | rustplus_client::Error::AlreadyConnecting
        | rustplus_client::Error::QueueFull
        | rustplus_client::Error::PendingFull
        | rustplus_client::Error::SequenceExhausted
        | rustplus_client::Error::InvalidConfig(_)
        | rustplus_client::Error::RequestTooLarge
        | rustplus_client::Error::InvalidCamera(_) => {
            Failure::new(StatusClass::ProtocolFailure, "client_protocol")
        }
    }
}

/// Negative-auth stage verdict for a registration attempted with a
/// synthetic, invalid Steam token.
///
/// Only a rejection that can be attributed to Facepunch's validation of the
/// Steam token passes: 401/403 (`reenrollment_required`) or another explicit
/// Facepunch 4xx rejection (400-class, excluding 429 which is mapped to
/// `RateLimited`). Expo or any other service rejecting, Facepunch 5xx,
/// success, throttling, and transport errors prove nothing about token
/// validation and fail the stage.
#[must_use]
pub fn negative_auth_outcome(result: Result<(), &RegistrationError>) -> StageOutcome {
    const ROUTE: Route = Route::FacepunchRegister;
    match result {
        Err(RegistrationError::ReenrollmentRequired) => {
            StageOutcome::pass(ROUTE, "invalid_token_rejected_expiry_not_run")
                .with_status_class(StatusClass::ReenrollmentRequired)
        }
        Err(RegistrationError::Rejected {
            service: "facepunch",
            status: 400..=499,
        }) => StageOutcome::pass(ROUTE, "invalid_token_typed_rejection_expiry_not_run")
            .with_status_class(StatusClass::Rejected),
        Err(RegistrationError::Rejected { .. }) => StageOutcome::fail(
            ROUTE,
            StatusClass::UnrelatedRejection,
            "negative_auth_rejection_not_token_validation",
        ),
        Ok(()) => StageOutcome::fail(
            ROUTE,
            StatusClass::UnexpectedSuccess,
            "invalid_token_accepted",
        ),
        Err(error) => StageOutcome::fail(
            ROUTE,
            classify_registration(error).class,
            "negative_auth_inconclusive",
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::manifest::Status;

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

    fn rejected(service: &'static str, status: u16) -> RegistrationError {
        RegistrationError::Rejected { service, status }
    }

    #[test]
    fn negative_auth_passes_only_for_facepunch_token_rejections() {
        let reenroll = negative_auth_outcome(Err(&RegistrationError::ReenrollmentRequired));
        assert_eq!(reenroll.status, Status::Pass);
        assert_eq!(reenroll.status_class, StatusClass::ReenrollmentRequired);

        for status in [400, 404, 422, 499] {
            let outcome = negative_auth_outcome(Err(&rejected("facepunch", status)));
            assert_eq!(outcome.status, Status::Pass, "{status}");
            assert_eq!(outcome.status_class, StatusClass::Rejected);
        }
    }

    #[test]
    fn negative_auth_fails_for_expo_and_other_rejections_with_distinct_class() {
        for error in [
            rejected("expo", 400),
            rejected("expo", 500),
            rejected("expo", 401),
            rejected("expo", 403),
            rejected("facepunch", 500),
            rejected("facepunch", 503),
            rejected("facepunch", 399),
            rejected("other", 400),
        ] {
            let outcome = negative_auth_outcome(Err(&error));
            assert_eq!(outcome.status, Status::Fail, "{error:?}");
            assert_eq!(outcome.status_class, StatusClass::UnrelatedRejection);
            assert_ne!(outcome.status_class, StatusClass::Rejected);
        }
    }

    #[test]
    fn negative_auth_fails_for_success_and_inconclusive_errors() {
        let accepted = negative_auth_outcome(Ok(()));
        assert_eq!(accepted.status, Status::Fail);
        assert_eq!(accepted.status_class, StatusClass::UnexpectedSuccess);
        for error in [
            RegistrationError::RateLimited,
            RegistrationError::Timeout("facepunch"),
            RegistrationError::Transport("facepunch"),
            RegistrationError::AuthTokenUnavailable("missing"),
            RegistrationError::InvalidResponse("x"),
            RegistrationError::Fcm("x".to_owned()),
        ] {
            let outcome = negative_auth_outcome(Err(&error));
            assert_eq!(outcome.status, Status::Fail, "{error:?}");
            assert_eq!(outcome.detail, "negative_auth_inconclusive");
        }
    }

    // Exhaustive: the classifiers have no wildcard arm, so a new upstream
    // variant fails compilation there; this table pins each current mapping.
    #[test]
    fn every_session_error_has_a_pinned_classification() {
        let second = Duration::from_secs(1);
        let cases = [
            (
                SessionError::InvalidCredentials,
                StatusClass::InvalidCredentials,
                "session_rejected_credentials",
            ),
            (
                SessionError::ConnectTimeout(second),
                StatusClass::Timeout,
                "session_timeout",
            ),
            (
                SessionError::ShutdownTimeout(second),
                StatusClass::Timeout,
                "session_timeout",
            ),
            (
                SessionError::Transport,
                StatusClass::Transport,
                "session_transport",
            ),
            (
                SessionError::Disconnected,
                StatusClass::Transport,
                "session_transport",
            ),
            (
                SessionError::ShuttingDown,
                StatusClass::Transport,
                "session_transport",
            ),
            (
                SessionError::Protocol,
                StatusClass::ProtocolFailure,
                "session_protocol",
            ),
            (
                SessionError::EventLagged { skipped: 3 },
                StatusClass::ProtocolFailure,
                "session_protocol",
            ),
            (
                SessionError::Failed,
                StatusClass::ProtocolFailure,
                "session_protocol",
            ),
        ];
        for (error, class, detail) in cases {
            assert_eq!(
                classify_session(&error),
                Failure::new(class, detail),
                "{error:?}"
            );
        }
    }

    #[test]
    fn every_client_error_has_a_pinned_classification() {
        use rustplus_client::Error;
        let second = Duration::from_secs(1);
        let endpoint = (StatusClass::InvalidEndpoint, "client_endpoint_refused");
        let timeout = (StatusClass::Timeout, "client_timeout");
        let transport = (StatusClass::Transport, "client_transport");
        let protocol = (StatusClass::ProtocolFailure, "client_protocol");
        let cases = [
            (Error::InsecureEndpoint, endpoint),
            (Error::InvalidEndpoint, endpoint),
            (Error::Timeout(second), timeout),
            (Error::ShutdownTimeout(second), timeout),
            (
                Error::Api("server text".to_owned()),
                (StatusClass::Rejected, "client_api_error"),
            ),
            (Error::Transport, transport),
            (Error::Io, transport),
            (Error::Disconnected, transport),
            (Error::ShuttingDown, transport),
            (Error::Decode, protocol),
            (Error::Encode, protocol),
            (Error::AlreadyConnecting, protocol),
            (Error::QueueFull, protocol),
            (Error::PendingFull, protocol),
            (Error::SequenceExhausted, protocol),
            (Error::InvalidConfig("x"), protocol),
            (Error::RequestTooLarge, protocol),
            (Error::InvalidCamera("x"), protocol),
        ];
        for (error, (class, detail)) in cases {
            assert_eq!(
                classify_client(&error),
                Failure::new(class, detail),
                "{error:?}"
            );
        }
    }
}
