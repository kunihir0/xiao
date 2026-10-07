//! Validated application configuration.

use std::fmt::{self, Debug, Display, Formatter};
use std::str::FromStr;

const REDACTED: &str = "[REDACTED]";

/// Application settings used by runtime adapters.
#[derive(Clone)]
pub struct Config {
    discord_token: String,
    database_url: String,
    telemetry: TelemetryConfig,
}

impl Config {
    /// Loads and validates configuration from the process environment.
    ///
    /// This intentionally does not read `.env` files or otherwise mutate the
    /// process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| {
            std::env::var(name)
                .map(Some)
                .or_else(|error| match error {
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
            return Err(ConfigError::Empty { variable: "RUST_LOG" });
        }
        let format = optional(&mut lookup, "TELEMETRY_FORMAT")?
            .unwrap_or_else(|| "compact".to_owned())
            .parse()
            .map_err(|_| ConfigError::InvalidTelemetryFormat {
                value: "TELEMETRY_FORMAT",
            })?;

        Ok(Self {
            discord_token,
            database_url,
            telemetry: TelemetryConfig { filter, format },
        })
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
}

impl Display for ConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { variable } => write!(formatter, "missing environment variable {variable}"),
            Self::Empty { variable } => write!(formatter, "environment variable {variable} must not be empty"),
            Self::InvalidUnicode { variable } => {
                write!(formatter, "environment variable {variable} is not valid UTF-8")
            }
            Self::InvalidDatabaseUrl { reason } => write!(formatter, "invalid DATABASE_URL: {reason}"),
            Self::InvalidTelemetryFormat { value } => {
                write!(formatter, "unsupported telemetry format in {value}")
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
    if value.chars().any(char::is_control) || value.chars().any(char::is_whitespace) {
        return Err(ConfigError::InvalidDatabaseUrl {
            reason: "contains whitespace or control characters",
        });
    }

    let authority = value
        .strip_prefix("postgres://")
        .or_else(|| value.strip_prefix("postgresql://"))
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .filter(|part| !part.is_empty())
        .ok_or(ConfigError::InvalidDatabaseUrl {
            reason: "expected postgres:// or postgresql:// with a host",
        })?;

    let host = match authority.rsplit('@').next() {
        Some(host) => host,
        None => authority,
    };
    if host.is_empty() || host == ":" || host.starts_with(':') {
        return Err(ConfigError::InvalidDatabaseUrl {
            reason: "database host is missing",
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
            Err(ConfigError::Missing { variable: "DISCORD_TOKEN" })
        ));
    }

    #[test]
    fn invalid_database_url_is_rejected() {
        let result = config(&[("DISCORD_TOKEN", "token"), ("DATABASE_URL", "sqlite://db")]);
        assert!(matches!(result, Err(ConfigError::InvalidDatabaseUrl { .. })));
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
}
