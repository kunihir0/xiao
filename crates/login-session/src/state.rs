use std::str::FromStr;

use thiserror::Error;

/// Lifecycle of one `rustplus_login_sessions` row.
///
/// The database enforces the same transitions; this is the shared source of
/// truth for the bot, the service and the tests.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LoginState {
    Pending,
    Claimed,
    Completed,
    Failed,
    Cancelled,
    Expired,
    Consumed,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("unknown login session state")]
pub struct ParseLoginStateError;

impl LoginState {
    pub const ALL: [Self; 7] = [
        Self::Pending,
        Self::Claimed,
        Self::Completed,
        Self::Failed,
        Self::Cancelled,
        Self::Expired,
        Self::Consumed,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Claimed => "claimed",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
            Self::Consumed => "consumed",
        }
    }

    /// `pending` and `claimed` rows occupy the single live-session slot.
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Pending | Self::Claimed)
    }

    /// Whether `self -> to` is allowed:
    /// - pending -> claimed
    /// - claimed -> completed | failed
    /// - pending | claimed -> cancelled | expired
    /// - completed -> consumed
    /// - completed -> expired (the bot's restart sweep nulls the ciphertext)
    ///
    /// Everything else, including self-transitions, is rejected.
    #[must_use]
    pub const fn can_transition(self, to: Self) -> bool {
        matches!(
            (self, to),
            (Self::Pending, Self::Claimed)
                | (Self::Claimed, Self::Completed | Self::Failed)
                | (
                    Self::Pending | Self::Claimed,
                    Self::Cancelled | Self::Expired
                )
                | (Self::Completed, Self::Consumed | Self::Expired)
        )
    }
}

impl FromStr for LoginState {
    type Err = ParseLoginStateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|state| state.as_str() == value)
            .ok_or(ParseLoginStateError)
    }
}

/// Why a login session ended in `failed`. Stored in a CHECKed column.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FailureCode {
    BrowserUnavailable,
    NavigationBlocked,
    BridgeRejected,
    PayloadInvalid,
    SteamSignoutFailed,
    Timeout,
    Cancelled,
    Orphaned,
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("unknown login failure code")]
pub struct ParseFailureCodeError;

impl FailureCode {
    pub const ALL: [Self; 9] = [
        Self::BrowserUnavailable,
        Self::NavigationBlocked,
        Self::BridgeRejected,
        Self::PayloadInvalid,
        Self::SteamSignoutFailed,
        Self::Timeout,
        Self::Cancelled,
        Self::Orphaned,
        Self::Internal,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BrowserUnavailable => "browser_unavailable",
            Self::NavigationBlocked => "navigation_blocked",
            Self::BridgeRejected => "bridge_rejected",
            Self::PayloadInvalid => "payload_invalid",
            Self::SteamSignoutFailed => "steam_signout_failed",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Orphaned => "orphaned",
            Self::Internal => "internal",
        }
    }
}

impl FromStr for FailureCode {
    type Err = ParseFailureCodeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|code| code.as_str() == value)
            .ok_or(ParseFailureCodeError)
    }
}

#[cfg(test)]
mod tests {
    use super::LoginState::{Cancelled, Claimed, Completed, Consumed, Expired, Failed, Pending};
    use super::*;

    const ALLOWED: [(LoginState, LoginState); 9] = [
        (Pending, Claimed),
        (Claimed, Completed),
        (Claimed, Failed),
        (Pending, Cancelled),
        (Pending, Expired),
        (Claimed, Cancelled),
        (Claimed, Expired),
        (Completed, Consumed),
        (Completed, Expired),
    ];

    #[test]
    fn full_transition_table_has_exactly_the_allowed_pairs() {
        let mut checked = 0;
        let mut allowed_count = 0;
        for from in LoginState::ALL {
            for to in LoginState::ALL {
                checked += 1;
                let expected = ALLOWED.contains(&(from, to));
                assert_eq!(
                    from.can_transition(to),
                    expected,
                    "{} -> {}",
                    from.as_str(),
                    to.as_str()
                );
                allowed_count += usize::from(expected);
            }
        }
        assert_eq!(checked, 49);
        assert_eq!(allowed_count, 9);
    }

    #[test]
    fn terminal_states_have_no_exits_and_no_self_transitions() {
        for terminal in [Failed, Cancelled, Expired, Consumed] {
            for to in LoginState::ALL {
                assert!(!terminal.can_transition(to));
            }
        }
        for state in LoginState::ALL {
            assert!(!state.can_transition(state));
        }
    }

    #[test]
    fn live_states_are_pending_and_claimed() {
        for state in LoginState::ALL {
            assert_eq!(state.is_live(), matches!(state, Pending | Claimed));
        }
    }

    #[test]
    fn state_strings_round_trip_and_are_unique() {
        let mut seen = Vec::new();
        for state in LoginState::ALL {
            assert_eq!(state.as_str().parse::<LoginState>(), Ok(state));
            assert!(!seen.contains(&state.as_str()));
            seen.push(state.as_str());
        }
        assert_eq!(seen.len(), 7);
    }

    #[test]
    fn unknown_state_strings_are_rejected() {
        for bad in [
            "",
            "Pending",
            "PENDING",
            " pending",
            "pending ",
            "done",
            "canceled",
            "consumed\n",
        ] {
            assert_eq!(
                bad.parse::<LoginState>(),
                Err(ParseLoginStateError),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn failure_codes_round_trip_and_match_spec() {
        let expected = [
            "browser_unavailable",
            "navigation_blocked",
            "bridge_rejected",
            "payload_invalid",
            "steam_signout_failed",
            "timeout",
            "cancelled",
            "orphaned",
            "internal",
        ];
        assert_eq!(FailureCode::ALL.len(), expected.len());
        for (code, text) in FailureCode::ALL.into_iter().zip(expected) {
            assert_eq!(code.as_str(), text);
            assert_eq!(text.parse::<FailureCode>(), Ok(code));
        }
    }

    #[test]
    fn unknown_failure_codes_are_rejected() {
        for bad in [
            "",
            "Timeout",
            "TIMEOUT",
            "time_out",
            "unknown",
            " internal",
            "internal\0",
        ] {
            assert_eq!(
                bad.parse::<FailureCode>(),
                Err(ParseFailureCodeError),
                "{bad:?}"
            );
        }
    }
}
