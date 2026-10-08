//! Validated application configuration.

use std::fmt::{self, Debug, Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use url::Url;
use zeroize::Zeroizing;

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

/// Rust+ companion configuration. Opt-in: every variable in
/// [`RUSTPLUS_VARIABLES`] must be set together, or none of them.
/// Standalone migration settings required by `xiao-bot migrate`.
#[derive(Clone)]
pub struct MigrationConfig {
    database_url: String,
    database_tls: DatabaseTlsConfig,
}

impl MigrationConfig {
    /// Loads migration configuration from the process environment.
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
        let database_tls = load_database_tls(&mut lookup)?;
        let migration_url_opt = load_secret(
            &mut lookup,
            "MIGRATION_DATABASE_URL",
            "MIGRATION_DATABASE_URL_FILE",
        )?;
        let database_url = match migration_url_opt {
            Some(url) => {
                validate_database_url(&url, &database_tls)?;
                url
            }
            None => {
                if matches!(database_tls, DatabaseTlsConfig::Insecure) {
                    let runtime_url =
                        load_required_secret(&mut lookup, "DATABASE_URL", "DATABASE_URL_FILE")?;
                    validate_database_url(&runtime_url, &database_tls)?;
                    runtime_url
                } else {
                    return Err(ConfigError::MigrationDatabaseUrlRequiredInProduction);
                }
            }
        };

        Ok(Self {
            database_url,
            database_tls,
        })
    }

    /// Returns the database connection string to use for migrations.
    pub fn database_url(&self) -> &str {
        &self.database_url
    }

    /// Returns the PostgreSQL TLS configuration.
    pub fn database_tls(&self) -> &DatabaseTlsConfig {
        &self.database_tls
    }
}

impl Debug for MigrationConfig {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MigrationConfig")
            .field("database_url", &REDACTED)
            .field("database_tls", &self.database_tls)
            .finish()
    }
}

/// Optional Rust+ push receiver configuration.
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
        let fcm_sender_id = load_secret(
            lookup,
            "RUSTPLUS_FCM_SENDER_ID",
            "RUSTPLUS_FCM_SENDER_ID_FILE",
        )?;
        let steam_id = optional(lookup, "RUSTPLUS_STEAM_ID")?;
        let steam_auth_token_file = optional(lookup, "RUSTPLUS_STEAM_AUTH_TOKEN_FILE")?;
        let keyring_file = optional(lookup, "SECRET_KEYRING_FILE")?;
        let active_key_id = optional(lookup, "SECRET_ACTIVE_KEY_ID")?;
        let values = [
            fcm_sender_id,
            steam_id,
            steam_auth_token_file,
            keyring_file,
            active_key_id,
        ];
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
        let discord_token =
            load_required_secret(&mut lookup, "DISCORD_TOKEN", "DISCORD_TOKEN_FILE")?;
        let database_tls = load_database_tls(&mut lookup)?;
        let database_url = load_required_secret(&mut lookup, "DATABASE_URL", "DATABASE_URL_FILE")?;
        validate_database_url(&database_url, &database_tls)?;

        let migration_database_url = load_secret(
            &mut lookup,
            "MIGRATION_DATABASE_URL",
            "MIGRATION_DATABASE_URL_FILE",
        )?;
        if let Some(migration_url) = &migration_database_url {
            validate_database_url(migration_url, &database_tls)?;
        }

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

        let rustplus = RustPlusConfig::from_lookup(&mut lookup)?;

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
            .field("steam_id", &self.steam_id)
            .field("steam_auth_token_file", &self.steam_auth_token_file)
            .field("keyring_file", &self.keyring_file)
            .field("active_key_id", &self.active_key_id)
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
    /// Only some of the Rust+ variables were set; the group is all-or-nothing.
    PartialRustPlus { missing: &'static str },
    /// A Rust+ variable was set to an unusable value.
    InvalidRustPlus {
        variable: &'static str,
        reason: &'static str,
    },
    /// Both direct environment variable and _FILE variable were provided.
    SecretCollision {
        variable: &'static str,
        file_variable: &'static str,
    },
    /// A secret file is missing, not a regular file, unreadable, or invalid.
    InvalidSecretFile {
        file_variable: &'static str,
        reason: &'static str,
    },
    /// A required secret file was empty.
    EmptySecretFile { file_variable: &'static str },
    /// In production, migration database URL is required and does not fall back to runtime role.
    MigrationDatabaseUrlRequiredInProduction,
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
            Self::PartialRustPlus { missing } => write!(
                formatter,
                "incomplete Rust+ configuration: {missing} is required when any Rust+ variable is set"
            ),
            Self::InvalidRustPlus { variable, reason } => {
                write!(formatter, "invalid {variable}: {reason}")
            }
            Self::SecretCollision {
                variable,
                file_variable,
            } => {
                write!(
                    formatter,
                    "both {variable} and {file_variable} are set; specify only one"
                )
            }
            Self::InvalidSecretFile {
                file_variable,
                reason,
            } => {
                write!(
                    formatter,
                    "invalid secret file in {file_variable}: {reason}"
                )
            }
            Self::EmptySecretFile { file_variable } => {
                write!(
                    formatter,
                    "secret file referenced by {file_variable} is empty"
                )
            }
            Self::MigrationDatabaseUrlRequiredInProduction => {
                write!(
                    formatter,
                    "MIGRATION_DATABASE_URL or MIGRATION_DATABASE_URL_FILE is required in production (no fallback to runtime DATABASE_URL)"
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}

fn load_database_tls<F>(lookup: &mut F) -> Result<DatabaseTlsConfig, ConfigError>
where
    F: FnMut(&'static str) -> Result<Option<String>, ConfigError>,
{
    let tls_mode = optional(lookup, "DATABASE_TLS_MODE")?;
    match tls_mode.as_deref() {
        None | Some("verify-full") => {
            let ca_file = required(lookup, "DATABASE_CA_FILE")?;
            Ok(DatabaseTlsConfig::VerifyFull {
                ca_file: PathBuf::from(ca_file),
            })
        }
        Some("insecure") | Some("disable") => Ok(DatabaseTlsConfig::Insecure),
        Some(_) => Err(ConfigError::InvalidDatabaseTlsMode {
            value: "DATABASE_TLS_MODE",
        }),
    }
}

/// Reads and validates a secret file from disk.
///
/// On Unix, the file is opened once with no-follow semantics and all owner,
/// mode, type, and size checks use that descriptor, closing path-stat/open
/// races. The trimmed secret is zeroized on drop.
pub fn read_secret_file(
    path: impl AsRef<Path>,
    file_variable: &'static str,
) -> Result<Zeroizing<String>, ConfigError> {
    use std::io::Read;

    const MAX_SECRET_BYTES: u64 = 64 * 1024;
    let path = path.as_ref();
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    #[cfg(not(unix))]
    {
        if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(ConfigError::InvalidSecretFile {
                file_variable,
                reason: "secret file must not be a symbolic link",
            });
        }
    }
    let mut file = options
        .open(path)
        .map_err(|_| ConfigError::InvalidSecretFile {
            file_variable,
            reason: "cannot open secret file without following links",
        })?;
    let metadata = file
        .metadata()
        .map_err(|_| ConfigError::InvalidSecretFile {
            file_variable,
            reason: "cannot read opened secret file metadata",
        })?;
    if !metadata.is_file() {
        return Err(ConfigError::InvalidSecretFile {
            file_variable,
            reason: "path is not a regular file",
        });
    }
    if metadata.len() > MAX_SECRET_BYTES {
        return Err(ConfigError::InvalidSecretFile {
            file_variable,
            reason: "secret file exceeds 64 KiB",
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(ConfigError::InvalidSecretFile {
                file_variable,
                reason: "secret file must be owned by the current process user",
            });
        }
        if metadata.mode() & 0o077 != 0 {
            return Err(ConfigError::InvalidSecretFile {
                file_variable,
                reason: "secret file permissions must be 0400 or 0600",
            });
        }
    }
    let mut content = Zeroizing::new(String::with_capacity(metadata.len() as usize));
    file.read_to_string(&mut content)
        .map_err(|_| ConfigError::InvalidSecretFile {
            file_variable,
            reason: "cannot read secret file as valid UTF-8",
        })?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::EmptySecretFile { file_variable });
    }
    Ok(Zeroizing::new(trimmed.to_owned()))
}

fn load_secret<F>(
    lookup: &mut F,
    variable: &'static str,
    file_variable: &'static str,
) -> Result<Option<String>, ConfigError>
where
    F: FnMut(&'static str) -> Result<Option<String>, ConfigError>,
{
    let direct = optional(lookup, variable)?;
    let file = optional(lookup, file_variable)?;

    match (direct, file) {
        (Some(_), Some(_)) => Err(ConfigError::SecretCollision {
            variable,
            file_variable,
        }),
        (Some(value), None) => Ok(Some(value)),
        (None, Some(path_str)) => {
            let secret = read_secret_file(path_str, file_variable)?;
            Ok(Some((*secret).clone()))
        }
        (None, None) => Ok(None),
    }
}

fn load_required_secret<F>(
    lookup: &mut F,
    variable: &'static str,
    file_variable: &'static str,
) -> Result<String, ConfigError>
where
    F: FnMut(&'static str) -> Result<Option<String>, ConfigError>,
{
    match load_secret(lookup, variable, file_variable)? {
        Some(value) => Ok(value),
        None => Err(ConfigError::Missing { variable }),
    }
}

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

fn validate_database_url(value: &str, tls_config: &DatabaseTlsConfig) -> Result<(), ConfigError> {
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

    if matches!(tls_config, DatabaseTlsConfig::VerifyFull { .. }) {
        for (k, v) in parsed.query_pairs() {
            if k == "sslmode" {
                match v.to_ascii_lowercase().as_str() {
                    "disable" | "allow" | "prefer" | "require" | "verify-ca" => {
                        return Err(ConfigError::InvalidDatabaseUrl {
                            reason: "in verify-full TLS mode, sslmode in DATABASE_URL must not downgrade verification (omit sslmode or use verify-full)",
                        });
                    }
                    "verify-full" => {}
                    _ => {
                        return Err(ConfigError::InvalidDatabaseUrl {
                            reason: "unrecognized sslmode in DATABASE_URL",
                        });
                    }
                }
            }
        }
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
    use super::{Config, ConfigError, MigrationConfig, read_secret_file};
    use std::collections::HashMap;
    use std::fs;

    fn config(values: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let values: HashMap<String, String> = values
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Config::from_lookup(|name| Ok(values.get(name).cloned()))
    }

    fn migration_config(values: &[(&str, &str)]) -> Result<MigrationConfig, ConfigError> {
        let values: HashMap<String, String> = values
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        MigrationConfig::from_lookup(|name| Ok(values.get(name).cloned()))
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
    }

    #[test]
    fn database_tls_verify_full_rejects_empty_ca_file() {
        let result = config(&[
            ("DISCORD_TOKEN", "token"),
            ("DATABASE_URL", "postgres://localhost/bot"),
            ("DATABASE_TLS_MODE", "verify-full"),
            ("DATABASE_CA_FILE", "   "),
        ]);
        assert!(matches!(
            result,
            Err(ConfigError::Empty {
                variable: "DATABASE_CA_FILE"
            })
        ));
    }

    #[test]
    fn database_tls_verify_full_rejects_downgrade_sslmode_in_url() {
        for bad_mode in ["disable", "prefer", "allow", "require", "verify-ca"] {
            let url = format!("postgres://localhost/bot?sslmode={bad_mode}");
            let result = config(&[
                ("DISCORD_TOKEN", "token"),
                ("DATABASE_URL", &url),
                ("DATABASE_TLS_MODE", "verify-full"),
                ("DATABASE_CA_FILE", "/path/to/ca.crt"),
            ]);
            assert!(
                matches!(result, Err(ConfigError::InvalidDatabaseUrl { .. })),
                "expected rejection for sslmode={bad_mode}"
            );
        }
    }

    #[test]
    fn database_tls_verify_full_accepts_verify_full_sslmode_in_url() {
        let result = config(&[
            ("DISCORD_TOKEN", "token"),
            (
                "DATABASE_URL",
                "postgres://localhost/bot?sslmode=verify-full",
            ),
            ("DATABASE_TLS_MODE", "verify-full"),
            ("DATABASE_CA_FILE", "/path/to/ca.crt"),
        ]);
        assert!(result.is_ok());
    }

    #[test]
    fn secret_collision_is_rejected() {
        let result = config(&[
            ("DISCORD_TOKEN", "token"),
            ("DISCORD_TOKEN_FILE", "/some/path"),
            ("DATABASE_URL", "postgres://localhost/bot"),
            ("DATABASE_TLS_MODE", "insecure"),
        ]);
        assert!(matches!(
            result,
            Err(ConfigError::SecretCollision {
                variable: "DISCORD_TOKEN",
                file_variable: "DISCORD_TOKEN_FILE"
            })
        ));
    }

    #[test]
    fn secret_file_is_read_and_trimmed() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let token_path = dir.path().join("token.txt");
        fs::write(&token_path, "  my-secret-token \n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&token_path, fs::Permissions::from_mode(0o400))?;
        }

        let token_str = token_path.to_str().ok_or("non-utf8 path")?;
        let result = config(&[
            ("DISCORD_TOKEN_FILE", token_str),
            ("DATABASE_URL", "postgres://localhost/bot"),
            ("DATABASE_TLS_MODE", "insecure"),
        ])?;
        assert_eq!(result.discord_token(), "my-secret-token");
        Ok(())
    }

    #[test]
    fn empty_secret_file_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let token_path = dir.path().join("empty.txt");
        fs::write(&token_path, "   \n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&token_path, fs::Permissions::from_mode(0o400))?;
        }

        let token_str = token_path.to_str().ok_or("non-utf8 path")?;
        let result = config(&[
            ("DISCORD_TOKEN_FILE", token_str),
            ("DATABASE_URL", "postgres://localhost/bot"),
            ("DATABASE_TLS_MODE", "insecure"),
        ]);
        assert!(matches!(
            result,
            Err(ConfigError::EmptySecretFile {
                file_variable: "DISCORD_TOKEN_FILE"
            })
        ));
        Ok(())
    }

    #[test]
    fn secret_file_with_permissive_permissions_is_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let token_path = dir.path().join("insecure_token.txt");
        fs::write(&token_path, "secret-token")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&token_path, fs::Permissions::from_mode(0o644))?;
        }

        let token_str = token_path.to_str().ok_or("non-utf8 path")?;
        let result = config(&[
            ("DISCORD_TOKEN_FILE", token_str),
            ("DATABASE_URL", "postgres://localhost/bot"),
            ("DATABASE_TLS_MODE", "insecure"),
        ]);
        assert!(matches!(
            result,
            Err(ConfigError::InvalidSecretFile {
                file_variable: "DISCORD_TOKEN_FILE",
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn migration_config_requires_migration_url_in_production() {
        let result = migration_config(&[
            ("DATABASE_URL", "postgres://localhost/bot"),
            ("DATABASE_TLS_MODE", "verify-full"),
            ("DATABASE_CA_FILE", "/path/to/ca.crt"),
        ]);
        assert!(matches!(
            result,
            Err(ConfigError::MigrationDatabaseUrlRequiredInProduction)
        ));
    }

    #[test]
    fn migration_config_falls_back_to_database_url_in_insecure_mode() -> Result<(), ConfigError> {
        let conf = migration_config(&[
            ("DATABASE_URL", "postgres://localhost/bot"),
            ("DATABASE_TLS_MODE", "insecure"),
        ])?;
        assert_eq!(conf.database_url(), "postgres://localhost/bot");
        Ok(())
    }

    #[test]
    fn migration_config_uses_migration_database_url_when_provided() -> Result<(), ConfigError> {
        let conf = migration_config(&[
            (
                "MIGRATION_DATABASE_URL",
                "postgres://ddl_user@localhost/bot",
            ),
            ("DATABASE_TLS_MODE", "insecure"),
        ])?;
        assert_eq!(conf.database_url(), "postgres://ddl_user@localhost/bot");
        Ok(())
    }

    #[test]
    fn debug_redacts_secret_values() -> Result<(), ConfigError> {
        let conf = config(&[
            ("DISCORD_TOKEN", "token-secret"),
            ("DATABASE_URL", "postgres://user:pass@localhost/bot"),
            (
                "MIGRATION_DATABASE_URL",
                "postgres://ddl:ddlpass@localhost/bot",
            ),
            ("DATABASE_TLS_MODE", "insecure"),
        ])?;
        let debug = format!("{conf:?}");
        assert!(!debug.contains("token-secret"));
        assert!(!debug.contains("pass"));
        assert!(!debug.contains("ddlpass"));
        Ok(())
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
            ("DATABASE_TLS_MODE", "insecure"),
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

    #[test]
    fn rustplus_sender_id_can_be_loaded_from_owner_only_file()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let sender_path = dir.path().join("sender-id");
        fs::write(&sender_path, "  sender-from-file \n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&sender_path, fs::Permissions::from_mode(0o400))?;
        }
        let mut values = HashMap::from([
            ("DISCORD_TOKEN".to_owned(), "token".to_owned()),
            (
                "DATABASE_URL".to_owned(),
                "postgres://localhost/bot".to_owned(),
            ),
            ("DATABASE_TLS_MODE".to_owned(), "insecure".to_owned()),
            (
                "RUSTPLUS_FCM_SENDER_ID_FILE".to_owned(),
                sender_path.to_string_lossy().into_owned(),
            ),
            (
                "RUSTPLUS_STEAM_ID".to_owned(),
                "76561198000000001".to_owned(),
            ),
            (
                "RUSTPLUS_STEAM_AUTH_TOKEN_FILE".to_owned(),
                "/run/secrets/rustplus_token".to_owned(),
            ),
            (
                "SECRET_KEYRING_FILE".to_owned(),
                "/run/secrets/keyring".to_owned(),
            ),
            ("SECRET_ACTIVE_KEY_ID".to_owned(), "key-2026-10".to_owned()),
        ]);
        let config = Config::from_lookup(|name| Ok(values.remove(name)))?;
        assert_eq!(
            config.rustplus().map(super::RustPlusConfig::fcm_sender_id),
            Some("sender-from-file"),
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn secret_file_reader_rejects_symlink() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::{fs::PermissionsExt, fs::symlink};

        let dir = tempfile::tempdir()?;
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        fs::write(&target, "secret")?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o400))?;
        symlink(&target, &link)?;
        assert!(matches!(
            read_secret_file(&link, "TEST_SECRET_FILE"),
            Err(ConfigError::InvalidSecretFile { .. })
        ));
        Ok(())
    }
}
