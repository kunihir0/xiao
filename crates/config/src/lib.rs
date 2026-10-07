//! Validated application configuration.

use std::fmt::{self, Debug, Display, Formatter};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use url::Url;

const REDACTED: &str = "[REDACTED]";

/// Application settings used by runtime adapters.
#[derive(Clone)]
pub struct Config {
    discord_token: String,
    database_url: String,
    telemetry: TelemetryConfig,
    rustplus: Option<RustPlusConfig>,
}

/// Rust+ companion configuration. Opt-in: every variable in
/// [`RUSTPLUS_VARIABLES`] must be set together, or none of them.
#[derive(Clone)]
pub struct RustPlusConfig {
    fcm_sender_id: String,
    steam_id: u64,
    steam_auth_token_file: PathBuf,
    keyring_file: PathBuf,
    active_key_id: String,
}

/// Environment variables that together enable Rust+ pairing.
pub const RUSTPLUS_VARIABLES: [&str; 5] = [
    "RUSTPLUS_FCM_SENDER_ID",
    "RUSTPLUS_STEAM_ID",
    "RUSTPLUS_STEAM_AUTH_TOKEN_FILE",
    "SECRET_KEYRING_FILE",
    "SECRET_ACTIVE_KEY_ID",
];

impl RustPlusConfig {
    pub fn fcm_sender_id(&self) -> &str {
        &self.fcm_sender_id
    }

    /// Steam account (SteamID64) that owns the companion registration.
    pub const fn steam_id(&self) -> u64 {
        self.steam_id
    }

    /// Owner-only file holding the operator-supplied Rust+ companion auth
    /// token. Read only when a registration is required.
    pub fn steam_auth_token_file(&self) -> &Path {
        &self.steam_auth_token_file
    }

    /// Owner-only key-encryption keyring file (`key_id:hex32` lines).
    pub fn keyring_file(&self) -> &Path {
        &self.keyring_file
    }

    /// Key id used to wrap newly written data keys.
    pub fn active_key_id(&self) -> &str {
        &self.active_key_id
    }

    fn from_lookup<F>(lookup: &mut F) -> Result<Option<Self>, ConfigError>
    where
        F: FnMut(&'static str) -> Result<Option<String>, ConfigError>,
    {
        let mut values = Vec::with_capacity(RUSTPLUS_VARIABLES.len());
        for variable in RUSTPLUS_VARIABLES {
            values.push(optional(lookup, variable)?);
        }
        if values.iter().all(Option::is_none) {
            return Ok(None);
        }
        if let Some(index) = values.iter().position(Option::is_none) {
            return Err(ConfigError::PartialRustPlus {
                missing: RUSTPLUS_VARIABLES[index],
            });
        }
        let mut values = values.into_iter().flatten();
        let mut next = || values.next().unwrap_or_default();
        let fcm_sender_id = next();
        let steam_id = next();
        let steam_auth_token_file = next();
        let keyring_file = next();
        let active_key_id = next();

        let steam_id = steam_id
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|value| *value != 0)
            .ok_or(ConfigError::InvalidRustPlus {
                variable: "RUSTPLUS_STEAM_ID",
                reason: "expected a non-zero SteamID64",
            })?;
        let active_key_id = active_key_id.trim().to_owned();
        if active_key_id.len() > 64
            || !active_key_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(ConfigError::InvalidRustPlus {
                variable: "SECRET_ACTIVE_KEY_ID",
                reason: "expected 1-64 characters from [A-Za-z0-9._-]",
            });
        }
        let steam_auth_token_file =
            absolute_path("RUSTPLUS_STEAM_AUTH_TOKEN_FILE", steam_auth_token_file)?;
        let keyring_file = absolute_path("SECRET_KEYRING_FILE", keyring_file)?;
        Ok(Some(Self {
            fcm_sender_id,
            steam_id,
            steam_auth_token_file,
            keyring_file,
            active_key_id,
        }))
    }
}

fn absolute_path(variable: &'static str, value: String) -> Result<PathBuf, ConfigError> {
    let path = PathBuf::from(value.trim());
    if !path.is_absolute() {
        return Err(ConfigError::InvalidRustPlus {
            variable,
            reason: "expected an absolute file path",
        });
    }
    Ok(path)
}

impl Config {
    /// Loads and validates configuration from the process environment.
    ///
    /// This intentionally does not read `.env` files or otherwise mutate the
    /// process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| {
            std::env::var(name).map(Some).or_else(|error| match error {
                std::env::VarError::NotPresent => Ok(None),
                std::env::VarError::NotUnicode(_) => {
                    Err(ConfigError::InvalidUnicode { variable: name })
                }
            })
        })
    }

    fn from_lookup<F>(mut lookup: F) -> Result<Self, ConfigError>
    where
        F: FnMut(&'static str) -> Result<Option<String>, ConfigError>,
    {
        let discord_token = required(&mut lookup, "DISCORD_TOKEN")?;
        let database_url = required(&mut lookup, "DATABASE_URL")?;
        validate_database_url(&database_url)?;

        let filter = optional(&mut lookup, "RUST_LOG")?.unwrap_or_else(|| "info".to_owned());
        if filter.trim().is_empty() {
            return Err(ConfigError::Empty {
                variable: "RUST_LOG",
            });
        }
        let format = optional(&mut lookup, "TELEMETRY_FORMAT")?
            .unwrap_or_else(|| "compact".to_owned())
            .parse()
            .map_err(|_| ConfigError::InvalidTelemetryFormat {
                value: "TELEMETRY_FORMAT",
            })?;

        let rustplus = RustPlusConfig::from_lookup(&mut lookup)?;

        Ok(Self {
            discord_token,
            database_url,
            telemetry: TelemetryConfig { filter, format },
            rustplus,
        })
    }

    /// Returns the optional Rust+ push receiver settings.
    pub fn rustplus(&self) -> Option<&RustPlusConfig> {
        self.rustplus.as_ref()
    }

    /// Returns the Discord credential for the Discord adapter.
    pub fn discord_token(&self) -> &str {
        &self.discord_token
    }

    /// Returns the database connection string for the database adapter.
    pub fn database_url(&self) -> &str {
        &self.database_url
    }

    /// Returns telemetry settings.
    pub fn telemetry(&self) -> &TelemetryConfig {
        &self.telemetry
    }
}

impl Debug for Config {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("discord_token", &REDACTED)
            .field("database_url", &REDACTED)
            .field("telemetry", &self.telemetry)
            .field("rustplus", &self.rustplus.as_ref().map(|_| REDACTED))
            .finish()
    }
}

impl Debug for RustPlusConfig {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RustPlusConfig")
            .field("fcm_sender_id", &REDACTED)
            .field("steam_id", &self.steam_id)
            .field("steam_auth_token_file", &self.steam_auth_token_file)
            .field("keyring_file", &self.keyring_file)
            .field("active_key_id", &self.active_key_id)
            .finish()
    }
}

/// Tracing output settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TelemetryConfig {
    /// `tracing-subscriber` filter directives, typically from `RUST_LOG`.
    pub filter: String,
    /// Output encoding selected by `TELEMETRY_FORMAT`.
    pub format: TelemetryFormat,
}

/// Supported telemetry encodings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelemetryFormat {
    /// Human-readable, one-line output.
    Compact,
    /// Human-readable pretty output.
    Pretty,
    /// Structured JSON output.
    Json,
}

impl FromStr for TelemetryFormat {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "compact" => Ok(Self::Compact),
            "pretty" => Ok(Self::Pretty),
            "json" => Ok(Self::Json),
            _ => Err(()),
        }
    }
}

/// Configuration loading and validation failures.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigError {
    /// A required environment variable was not set.
    Missing { variable: &'static str },
    /// An environment variable was set but had no usable value.
    Empty { variable: &'static str },
    /// An environment variable was not valid UTF-8.
    InvalidUnicode { variable: &'static str },
    /// The database URL is not a supported PostgreSQL connection string.
    InvalidDatabaseUrl { reason: &'static str },
    /// The telemetry output format is not supported.
    InvalidTelemetryFormat { value: &'static str },
    /// Only some of the Rust+ variables were set; the group is all-or-nothing.
    PartialRustPlus { missing: &'static str },
    /// A Rust+ variable was set to an unusable value.
    InvalidRustPlus {
        variable: &'static str,
        reason: &'static str,
    },
}

impl Display for ConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { variable } => {
                write!(formatter, "missing environment variable {variable}")
            }
            Self::Empty { variable } => write!(
                formatter,
                "environment variable {variable} must not be empty"
            ),
            Self::InvalidUnicode { variable } => {
                write!(
                    formatter,
                    "environment variable {variable} is not valid UTF-8"
                )
            }
            Self::InvalidDatabaseUrl { reason } => {
                write!(formatter, "invalid DATABASE_URL: {reason}")
            }
            Self::InvalidTelemetryFormat { value } => {
                write!(formatter, "unsupported telemetry format in {value}")
            }
            Self::PartialRustPlus { missing } => write!(
                formatter,
                "incomplete Rust+ configuration: {missing} is required when any Rust+ variable is set"
            ),
            Self::InvalidRustPlus { variable, reason } => {
                write!(formatter, "invalid {variable}: {reason}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

fn required<F>(lookup: &mut F, variable: &'static str) -> Result<String, ConfigError>
where
    F: FnMut(&'static str) -> Result<Option<String>, ConfigError>,
{
    match lookup(variable)? {
        None => Err(ConfigError::Missing { variable }),
        Some(value) if value.trim().is_empty() => Err(ConfigError::Empty { variable }),
        Some(value) => Ok(value),
    }
}

fn optional<F>(lookup: &mut F, variable: &'static str) -> Result<Option<String>, ConfigError>
where
    F: FnMut(&'static str) -> Result<Option<String>, ConfigError>,
{
    match lookup(variable)? {
        Some(value) if value.trim().is_empty() => Err(ConfigError::Empty { variable }),
        value => Ok(value),
    }
}

fn validate_database_url(value: &str) -> Result<(), ConfigError> {
    let parsed = Url::parse(value).map_err(|_| ConfigError::InvalidDatabaseUrl {
        reason: "URL syntax is invalid",
    })?;
    if !matches!(parsed.scheme(), "postgres" | "postgresql") {
        return Err(ConfigError::InvalidDatabaseUrl {
            reason: "expected postgres:// or postgresql://",
        });
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(ConfigError::InvalidDatabaseUrl {
            reason: "database host is missing",
        });
    }
    if parsed.path().trim_matches('/').is_empty() {
        return Err(ConfigError::InvalidDatabaseUrl {
            reason: "database name is missing",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Config, ConfigError, TelemetryFormat};
    use std::collections::HashMap;

    fn config(values: &[(&'static str, &'static str)]) -> Result<Config, ConfigError> {
        let values: HashMap<_, _> = values.iter().copied().collect();
        Config::from_lookup(|name| Ok(values.get(name).map(|value| (*value).to_owned())))
    }

    #[test]
    fn missing_required_environment_is_reported() {
        let result = config(&[]);
        assert!(matches!(
            result,
            Err(ConfigError::Missing {
                variable: "DISCORD_TOKEN"
            })
        ));
    }

    #[test]
    fn invalid_database_url_is_rejected() {
        let result = config(&[("DISCORD_TOKEN", "token"), ("DATABASE_URL", "sqlite://db")]);
        assert!(matches!(
            result,
            Err(ConfigError::InvalidDatabaseUrl { .. })
        ));
    }

    #[test]
    fn malformed_database_authority_and_path_are_rejected() {
        for database_url in [
            "postgres://localhost:not-a-port/bot",
            "postgres://localhost/",
        ] {
            let result = config(&[("DISCORD_TOKEN", "token"), ("DATABASE_URL", database_url)]);
            assert!(matches!(
                result,
                Err(ConfigError::InvalidDatabaseUrl { .. })
            ));
        }
    }

    #[test]
    fn telemetry_settings_are_typed() {
        let result = config(&[
            ("DISCORD_TOKEN", "token"),
            ("DATABASE_URL", "postgres://localhost/bot"),
            ("RUST_LOG", "bot=debug"),
            ("TELEMETRY_FORMAT", "json"),
        ]);
        assert!(result.is_ok());
        if let Ok(config) = result {
            assert_eq!(config.telemetry().filter, "bot=debug");
            assert_eq!(config.telemetry().format, TelemetryFormat::Json);
        }
    }

    #[test]
    fn debug_redacts_secret_values() {
        let result = config(&[
            ("DISCORD_TOKEN", "super-secret-token"),
            ("DATABASE_URL", "postgres://user:password@localhost/bot"),
        ]);
        assert!(result.is_ok());
        if let Ok(config) = result {
            let output = format!("{config:?}");
            assert!(!output.contains("super-secret-token"));
            assert!(!output.contains("password@"));
            assert_eq!(output.matches("[REDACTED]").count(), 2);
        }
    }

    #[test]
    fn absent_rustplus_configuration_is_disabled() {
        let result = config(&[
            ("DISCORD_TOKEN", "token"),
            ("DATABASE_URL", "postgres://localhost/bot"),
        ]);
        assert!(result.is_ok());
        if let Ok(config) = result {
            assert!(config.rustplus().is_none());
        }
    }

    const RUSTPLUS: [(&str, &str); 5] = [
        ("RUSTPLUS_FCM_SENDER_ID", "sender-secret"),
        ("RUSTPLUS_STEAM_ID", "76561198000000001"),
        (
            "RUSTPLUS_STEAM_AUTH_TOKEN_FILE",
            "/run/secrets/rustplus_token",
        ),
        ("SECRET_KEYRING_FILE", "/run/secrets/keyring"),
        ("SECRET_ACTIVE_KEY_ID", "key-2026-10"),
    ];

    fn with_rustplus(
        overrides: &[(&'static str, &'static str)],
        omit: Option<&str>,
    ) -> Result<Config, ConfigError> {
        let mut values = vec![
            ("DISCORD_TOKEN", "token"),
            ("DATABASE_URL", "postgres://localhost/bot"),
        ];
        for (name, value) in RUSTPLUS {
            if Some(name) == omit {
                continue;
            }
            let value = overrides
                .iter()
                .find(|(key, _)| *key == name)
                .map_or(value, |(_, value)| *value);
            values.push((name, value));
        }
        config(&values)
    }

    #[test]
    fn complete_rustplus_configuration_is_typed_and_redacted() {
        let result = with_rustplus(&[], None);
        assert!(result.is_ok());
        if let Ok(config) = result {
            let rustplus = config.rustplus();
            assert!(rustplus.is_some());
            if let Some(rustplus) = rustplus {
                assert_eq!(rustplus.steam_id(), 76_561_198_000_000_001);
                assert_eq!(rustplus.active_key_id(), "key-2026-10");
                assert_eq!(
                    rustplus.keyring_file(),
                    std::path::Path::new("/run/secrets/keyring")
                );
            }
            let debug = format!("{config:?}");
            assert!(!debug.contains("sender-secret"));
            assert_eq!(debug.matches("[REDACTED]").count(), 3);
        }
    }

    #[test]
    fn partial_rustplus_configuration_is_a_typed_error() {
        for (missing, _) in RUSTPLUS {
            assert_eq!(
                with_rustplus(&[], Some(missing)).err(),
                Some(ConfigError::PartialRustPlus { missing }),
            );
        }
    }

    #[test]
    fn invalid_rustplus_values_are_rejected() {
        let cases: [(&'static str, &'static str); 6] = [
            ("RUSTPLUS_STEAM_ID", "0"),
            ("RUSTPLUS_STEAM_ID", "steam"),
            ("SECRET_ACTIVE_KEY_ID", "bad key"),
            ("SECRET_ACTIVE_KEY_ID", "k\u{e9}y"),
            ("SECRET_KEYRING_FILE", "relative/keyring"),
            ("RUSTPLUS_STEAM_AUTH_TOKEN_FILE", "token.txt"),
        ];
        for (variable, value) in cases {
            assert!(matches!(
                with_rustplus(&[(variable, value)], None),
                Err(ConfigError::InvalidRustPlus { variable: v, .. }) if v == variable
            ));
        }
        assert_eq!(
            with_rustplus(&[("RUSTPLUS_FCM_SENDER_ID", " ")], None).err(),
            Some(ConfigError::Empty {
                variable: "RUSTPLUS_FCM_SENDER_ID"
            })
        );
    }
}
