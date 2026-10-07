use std::fmt;

use crate::{DomainError, PlayerId};

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct PlayerToken(i32);

impl PlayerToken {
    pub const fn new(value: i32) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i32 {
        self.0
    }
}

impl fmt::Debug for PlayerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct PlayerCredentials {
    player_id: PlayerId,
    player_token: PlayerToken,
}

impl PlayerCredentials {
    pub const fn new(player_id: PlayerId, player_token: PlayerToken) -> Self {
        Self {
            player_id,
            player_token,
        }
    }

    pub const fn player_id(&self) -> PlayerId {
        self.player_id
    }

    pub const fn player_token(&self) -> PlayerToken {
        self.player_token
    }
}

impl fmt::Debug for PlayerCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlayerCredentials")
            .field("player_id", &self.player_id)
            .field("player_token", &"[REDACTED]")
            .finish()
    }
}

impl fmt::Display for PlayerCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "player {} with token [REDACTED]",
            self.player_id.value()
        )
    }
}

impl TryFrom<(PlayerId, i64)> for PlayerCredentials {
    type Error = DomainError;

    fn try_from((player_id, token): (PlayerId, i64)) -> Result<Self, Self::Error> {
        let token = i32::try_from(token).map_err(|_| DomainError::Invalid {
            field: "player_token",
        })?;
        Ok(Self::new(player_id, PlayerToken::new(token)))
    }
}
