//! Validated application configuration.

use std::fmt::{self, Debug, Display, Formatter};
use std::path::PathBuf;
use std::str::FromStr;
use url::Url;

const REDACTED: &str = "[REDACTED]";

/// Application settings used by runtime adapters.
#[derive(Clone)]
pub struct Config {
    discord_token: String,
    database_url: String,
    migration_database_url: Option<String>,
    database_tls: DatabaseTlsConfig,
    database_migrate_on_startup: bool,
    telemetry: TelemetryConfig,
    rustplus: Option<RustPlusConfig>,
}

/// Optional Rust+ push receiver configuration.
#[derive(Clone)]
pub struct RustPlusConfig {
    fcm_sender_id: String,
}

impl RustPlusConfig {
    pub fn fcm_sender_id(&self) -> &str {
        &self.fcm_sender_id
    }
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

        let migration_database_url = optional(&mut lookup, "MIGRATION_DATABASE_URL")?;
        if let Some(migration_url) = &migration_database_url {
            validate_database_url(migration_url)?;
        }

        let tls_mode = optional(&mut lookup, "DATABASE_TLS_MODE")?;
        let database_tls = match tls_mode.as_deref() {
            None | Some("verify-full") => {
                let ca_file = required(&mut lookup, "DATABASE_CA_FILE")?;
                DatabaseTlsConfig::VerifyFull {
                    ca_file: PathBuf::from(ca_file),
                }
            }
            Some("insecure") | Some("disable") => DatabaseTlsConfig::Insecure,
            Some(_) => {
                return Err(ConfigError::InvalidDatabaseTlsMode {
                    value: "DATABASE_TLS_MODE",
                });
            }
        };

        let database_migrate_on_startup = optional(&mut lookup, "DATABASE_MIGRATE_ON_STARTUP")?
            .map(|value| parse_bool(&value, "DATABASE_MIGRATE_ON_STARTUP"))
            .transpose()?
            .unwrap_or(true);

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

        let rustplus = optional(&mut lookup, "RUSTPLUS_FCM_SENDER_ID")?
            .map(|fcm_sender_id| RustPlusConfig { fcm_sender_id });

        Ok(Self {
            discord_token,
            database_url,
            migration_database_url,
            database_tls,
            database_migrate_on_startup,
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

    /// Returns the optional migration database connection string for DDL operations.
    pub fn migration_database_url(&self) -> Option<&str> {
        self.migration_database_url.as_deref()
    }

    /// Returns the PostgreSQL TLS configuration.
    pub fn database_tls(&self) -> &DatabaseTlsConfig {
        &self.database_tls
    }

    /// Returns whether pending migrations should be applied during bot startup.
    pub fn database_migrate_on_startup(&self) -> bool {
        self.database_migrate_on_startup
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
            .field(
                "migration_database_url",
                &self.migration_database_url.as_ref().map(|_| REDACTED),
            )
            .field("database_tls", &self.database_tls)
            .field(
                "database_migrate_on_startup",
                &self.database_migrate_on_startup,
            )
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
            .finish()
    }
}

/// Validated PostgreSQL TLS configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DatabaseTlsConfig {
    /// Strict verify-full TLS mode requiring a verified CA certificate file.
    VerifyFull { ca_file: PathBuf },
    /// Explicit local development opt-in for unencrypted database connections.
    Insecure,
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
    /// The database TLS mode is not supported.
    InvalidDatabaseTlsMode { value: &'static str },
    /// A boolean configuration variable was neither true nor false.
    InvalidBoolean { variable: &'static str },
    /// The telemetry output format is not supported.
    InvalidTelemetryFormat { value: &'static str },
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
                write!(formatter, "invalid database URL: {reason}")
            }
            Self::InvalidDatabaseTlsMode { value } => {
                write!(
                    formatter,
                    "unsupported database TLS mode in {value}: expected 'verify-full' or 'insecure'"
                )
            }
            Self::InvalidBoolean { variable } => {
                write!(
                    formatter,
                    "invalid boolean in {variable}: expected 'true' or 'false'"
                )
            }
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

fn parse_bool(value: &str, variable: &'static str) -> Result<bool, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(ConfigError::InvalidBoolean { variable }),
    }
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
        let result = config(&[
            ("DISCORD_TOKEN", "token"),
            ("DATABASE_URL", "sqlite://db"),
            ("DATABASE_TLS_MODE", "insecure"),
        ]);
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
            let result = config(&[
                ("DISCORD_TOKEN", "token"),
                ("DATABASE_URL", database_url),
                ("DATABASE_TLS_MODE", "insecure"),
            ]);
            assert!(matches!(
                result,
                Err(ConfigError::InvalidDatabaseUrl { .. })
            ));
        }
    }

    #[test]
    fn database_tls_defaults_to_verify_full_and_requires_ca_file() {
        let result = config(&[
            ("DISCORD_TOKEN", "token"),
            ("DATABASE_URL", "postgres://localhost/bot"),
        ]);
        assert!(matches!(
            result,
            Err(ConfigError::Missing {
                variable: "DATABASE_CA_FILE"
            })
        ));

        let result = config(&[
            ("DISCORD_TOKEN", "token"),
            ("DATABASE_URL", "postgres://localhost/bot"),
            ("DATABASE_CA_FILE", "   "),
        ]);
        assert!(matches!(
            result,
            Err(ConfigError::Empty {
                variable: "DATABASE_CA_FILE"
            })
        ));

        let result = config(&[
            ("DISCORD_TOKEN", "token"),
            ("DATABASE_URL", "postgres://localhost/bot"),
            ("DATABASE_CA_FILE", "/run/secrets/database-ca.crt"),
        ]);
        assert!(result.is_ok());
        if let Ok(config) = result {
            assert_eq!(
                config.database_tls(),
                &super::DatabaseTlsConfig::VerifyFull {
                    ca_file: std::path::PathBuf::from("/run/secrets/database-ca.crt"),
                }
            );
        }
    }

    #[test]
    fn database_tls_explicit_insecure_mode_does_not_require_ca_file() {
        for mode in ["insecure", "disable"] {
            let result = config(&[
                ("DISCORD_TOKEN", "token"),
                ("DATABASE_URL", "postgres://localhost/bot"),
                ("DATABASE_TLS_MODE", mode),
            ]);
            assert!(result.is_ok());
            if let Ok(config) = result {
                assert_eq!(config.database_tls(), &super::DatabaseTlsConfig::Insecure);
            }
        }
    }

    #[test]
    fn database_tls_invalid_mode_is_rejected() {
        let result = config(&[
            ("DISCORD_TOKEN", "token"),
            ("DATABASE_URL", "postgres://localhost/bot"),
            ("DATABASE_TLS_MODE", "invalid-mode"),
            ("DATABASE_CA_FILE", "/run/secrets/database-ca.crt"),
        ]);
        assert!(matches!(
            result,
            Err(ConfigError::InvalidDatabaseTlsMode { .. })
        ));
    }

    #[test]
    fn migration_database_url_is_optional_validated_and_redacted() {
        let result = config(&[
            ("DISCORD_TOKEN", "token"),
            ("DATABASE_URL", "postgres://localhost/bot"),
            ("DATABASE_TLS_MODE", "insecure"),
            ("MIGRATION_DATABASE_URL", "sqlite://invalid"),
        ]);
        assert!(matches!(
            result,
            Err(ConfigError::InvalidDatabaseUrl { .. })
        ));

        let result = config(&[
            ("DISCORD_TOKEN", "token"),
            ("DATABASE_URL", "postgres://bot_user:secret@localhost/bot"),
            ("DATABASE_TLS_MODE", "insecure"),
            (
                "MIGRATION_DATABASE_URL",
                "postgres://ddl_user:secret_ddl@localhost/bot",
            ),
        ]);
        assert!(result.is_ok());
        if let Ok(config) = result {
            assert_eq!(
                config.migration_database_url(),
                Some("postgres://ddl_user:secret_ddl@localhost/bot")
            );
            let debug = format!("{config:?}");
            assert!(!debug.contains("secret"));
            assert!(!debug.contains("secret_ddl"));
            assert!(!debug.contains("ddl_user"));
        }
    }

    #[test]
    fn database_migrate_on_startup_is_typed() {
        let result = config(&[
            ("DISCORD_TOKEN", "token"),
            ("DATABASE_URL", "postgres://localhost/bot"),
            ("DATABASE_TLS_MODE", "insecure"),
        ]);
        assert!(result.is_ok());
        if let Ok(config) = result {
            assert!(config.database_migrate_on_startup());
        }

        for false_val in ["false", "0", "no", "FALSE"] {
            let result = config(&[
                ("DISCORD_TOKEN", "token"),
                ("DATABASE_URL", "postgres://localhost/bot"),
                ("DATABASE_TLS_MODE", "insecure"),
                ("DATABASE_MIGRATE_ON_STARTUP", false_val),
            ]);
            assert!(result.is_ok());
            if let Ok(config) = result {
                assert!(!config.database_migrate_on_startup());
            }
        }

        let result = config(&[
            ("DISCORD_TOKEN", "token"),
            ("DATABASE_URL", "postgres://localhost/bot"),
            ("DATABASE_TLS_MODE", "insecure"),
            ("DATABASE_MIGRATE_ON_STARTUP", "not-a-bool"),
        ]);
        assert!(matches!(result, Err(ConfigError::InvalidBoolean { .. })));
    }

    #[test]
    fn telemetry_settings_are_typed() {
        let result = config(&[
            ("DISCORD_TOKEN", "token"),
            ("DATABASE_URL", "postgres://localhost/bot"),
            ("DATABASE_TLS_MODE", "insecure"),
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
            ("DATABASE_TLS_MODE", "insecure"),
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
            ("DATABASE_TLS_MODE", "insecure"),
        ]);
        assert!(result.is_ok());
        if let Ok(config) = result {
            assert!(config.rustplus().is_none());
        }
    }

    #[test]
    fn invalid_rustplus_configuration_is_typed_and_redacted() {
        let result = config(&[
            ("DISCORD_TOKEN", "token"),
            ("DATABASE_URL", "postgres://localhost/bot"),
            ("DATABASE_TLS_MODE", "insecure"),
            ("RUSTPLUS_FCM_SENDER_ID", " "),
        ]);
        assert!(matches!(
            result,
            Err(ConfigError::Empty {
                variable: "RUSTPLUS_FCM_SENDER_ID"
            })
        ));

        let result = config(&[
            ("DISCORD_TOKEN", "token"),
            ("DATABASE_URL", "postgres://localhost/bot"),
            ("DATABASE_TLS_MODE", "insecure"),
            ("RUSTPLUS_FCM_SENDER_ID", "sender-secret"),
        ]);
        assert!(result.is_ok());
        if let Ok(config) = result {
            let debug = format!("{config:?}");
            assert!(!debug.contains("sender-secret"));
            assert_eq!(debug.matches("[REDACTED]").count(), 3);
        }
    }
}
