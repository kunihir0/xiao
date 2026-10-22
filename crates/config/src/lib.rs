//! Validated application configuration.

use std::fmt::{self, Debug, Display, Formatter};
use std::fs;
use std::net::SocketAddr;
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
    login_public_host: Option<LoginPublicHost>,
}

/// Standalone migration settings required by `xiao-bot migrate`.
#[derive(Clone)]
pub struct MigrationConfig {
    database_url: String,
    database_tls: DatabaseTlsConfig,
}

impl MigrationConfig {
    /// Loads migration configuration from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(env_lookup)
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

/// Optional Rust+ companion configuration.
///
/// The core group ([`RUSTPLUS_CORE_VARIABLES`]) is all-or-none and enables the
/// Rust+ supervisor. The legacy group ([`RUSTPLUS_LEGACY_VARIABLES`]) is
/// both-or-neither and requires the core group; it names an operator-supplied
/// account that is used until an account enrolled through `/login` exists.
#[derive(Clone)]
pub struct RustPlusConfig {
    fcm_sender_id: String,
    keyring_file: PathBuf,
    active_key_id: String,
    legacy_account: Option<LegacyAccount>,
}

/// Group A: variables that together enable the Rust+ supervisor.
pub const RUSTPLUS_CORE_VARIABLES: [&str; 3] = [
    "RUSTPLUS_FCM_SENDER_ID",
    "SECRET_KEYRING_FILE",
    "SECRET_ACTIVE_KEY_ID",
];

/// Group B: variables that together name the legacy operator-supplied account.
/// Only valid alongside [`RUSTPLUS_CORE_VARIABLES`].
pub const RUSTPLUS_LEGACY_VARIABLES: [&str; 2] =
    ["RUSTPLUS_STEAM_ID", "RUSTPLUS_STEAM_AUTH_TOKEN_FILE"];

/// Legacy operator-supplied Rust+ account: a SteamID64 and the owner-only file
/// holding its companion auth token. `Debug` redacts the SteamID (ADR 0003:
/// logs never contain one).
#[derive(Clone, Eq, PartialEq)]
pub struct LegacyAccount {
    steam_id: u64,
    token_file: PathBuf,
}

impl fmt::Debug for LegacyAccount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LegacyAccount")
            .field("steam_id", &"[REDACTED]")
            .field("token_file", &self.token_file)
            .finish()
    }
}

impl LegacyAccount {
    /// Steam account (SteamID64) that owns the companion registration.
    pub const fn steam_id(&self) -> u64 {
        self.steam_id
    }

    /// Owner-only file holding the operator-supplied Rust+ companion auth
    /// token. Read only when a registration is required.
    pub fn token_file(&self) -> &Path {
        &self.token_file
    }

    fn parse(steam_id: &str, token_file: String) -> Result<Self, ConfigError> {
        let steam_id = steam_id
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|value| *value != 0)
            .ok_or(ConfigError::InvalidRustPlus {
                variable: "RUSTPLUS_STEAM_ID",
                reason: "expected a non-zero SteamID64",
            })?;
        let token_file = absolute_path("RUSTPLUS_STEAM_AUTH_TOKEN_FILE", token_file)?;
        Ok(Self {
            steam_id,
            token_file,
        })
    }
}

/// Returns the first missing variable when a group is partially set.
fn partial_group(values: &[Option<String>], names: &[&'static str]) -> Option<&'static str> {
    if values.iter().all(Option::is_none) {
        return None;
    }
    values
        .iter()
        .zip(names)
        .find_map(|(value, name)| value.is_none().then_some(*name))
}

impl RustPlusConfig {
    pub fn fcm_sender_id(&self) -> &str {
        &self.fcm_sender_id
    }

    /// Owner-only key-encryption keyring file (`key_id:hex32` lines).
    pub fn keyring_file(&self) -> &Path {
        &self.keyring_file
    }

    /// Key id used to wrap newly written data keys.
    pub fn active_key_id(&self) -> &str {
        &self.active_key_id
    }

    /// Legacy operator-supplied account, when group B is configured.
    pub fn legacy_account(&self) -> Option<&LegacyAccount> {
        self.legacy_account.as_ref()
    }

    fn from_lookup<F>(lookup: &mut F) -> Result<Option<Self>, ConfigError>
    where
        F: FnMut(&'static str) -> Result<Option<String>, ConfigError>,
    {
        let core = [
            load_secret(
                lookup,
                "RUSTPLUS_FCM_SENDER_ID",
                "RUSTPLUS_FCM_SENDER_ID_FILE",
            )?,
            optional(lookup, "SECRET_KEYRING_FILE")?,
            optional(lookup, "SECRET_ACTIVE_KEY_ID")?,
        ];
        let legacy = [
            optional(lookup, "RUSTPLUS_STEAM_ID")?,
            optional(lookup, "RUSTPLUS_STEAM_AUTH_TOKEN_FILE")?,
        ];
        if let Some(missing) = partial_group(&core, &RUSTPLUS_CORE_VARIABLES)
            .or_else(|| partial_group(&legacy, &RUSTPLUS_LEGACY_VARIABLES))
        {
            return Err(ConfigError::PartialRustPlus { missing });
        }
        let [Some(fcm_sender_id), Some(keyring_file), Some(active_key_id)] = core else {
            // Group A is entirely absent: group B alone is a configuration error.
            return if legacy.iter().any(Option::is_some) {
                Err(ConfigError::LegacyRustPlusWithoutCore)
            } else {
                Ok(None)
            };
        };
        let legacy_account = match legacy {
            [Some(steam_id), Some(token_file)] => {
                Some(LegacyAccount::parse(&steam_id, token_file)?)
            }
            _ => None,
        };
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
        let keyring_file = absolute_path("SECRET_KEYRING_FILE", keyring_file)?;
        Ok(Some(Self {
            fcm_sender_id,
            keyring_file,
            active_key_id,
            legacy_account,
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

fn absolute_login_path(variable: &'static str, value: String) -> Result<PathBuf, ConfigError> {
    let path = PathBuf::from(value.trim());
    if !path.is_absolute() {
        return Err(ConfigError::InvalidLogin {
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
        Self::from_lookup(env_lookup)
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

        let telemetry = load_telemetry(&mut lookup)?;

        let rustplus = RustPlusConfig::from_lookup(&mut lookup)?;
        let login_public_host = optional(&mut lookup, "LOGIN_PUBLIC_HOST")?
            .map(|value| LoginPublicHost::parse(&value))
            .transpose()?;

        Ok(Self {
            discord_token,
            database_url,
            migration_database_url,
            database_tls,
            database_migrate_on_startup,
            telemetry,
            rustplus,
            login_public_host,
        })
    }

    /// Returns the optional Rust+ push receiver settings.
    pub fn rustplus(&self) -> Option<&RustPlusConfig> {
        self.rustplus.as_ref()
    }

    /// Returns the public host `/login` links point at, when configured.
    pub fn login_public_host(&self) -> Option<&LoginPublicHost> {
        self.login_public_host.as_ref()
    }

    /// Returns whether the Discord `/login` command is enabled: the Rust+
    /// core group and `LOGIN_PUBLIC_HOST` must both be set. The Rust+
    /// supervisor itself needs only the core group.
    pub fn login_enabled(&self) -> bool {
        self.rustplus.is_some() && self.login_public_host.is_some()
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
            .field("login_public_host", &self.login_public_host)
            .finish()
    }
}

impl Debug for RustPlusConfig {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RustPlusConfig")
            .field("fcm_sender_id", &REDACTED)
            .field("keyring_file", &self.keyring_file)
            .field("active_key_id", &self.active_key_id)
            .field("legacy_account", &self.legacy_account)
            .finish()
    }
}

/// Public DNS host that `/login` links and the `xiao-login` service use.
///
/// Host-only: lowercase ASCII letters, digits and hyphens in at least two
/// dot-separated labels (each 1-63 characters, not starting or ending with a
/// hyphen, 253 characters in total). Schemes, ports, paths, queries,
/// fragments, userinfo, whitespace, uppercase letters, a trailing dot, IP
/// literals (the last label must not be all digits) and bare names such as
/// `localhost` are rejected rather than normalized, so the value is
/// byte-for-byte what appears in links and `Origin` checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoginPublicHost(String);

impl LoginPublicHost {
    /// Validates `value` as a [`LoginPublicHost`].
    pub fn parse(value: &str) -> Result<Self, ConfigError> {
        let reason = |reason| ConfigError::InvalidLoginPublicHost { reason };
        if value.is_empty() {
            return Err(reason("must not be empty"));
        }
        if value.chars().any(char::is_whitespace) {
            return Err(reason("must not contain whitespace"));
        }
        if value.contains(['/', '?', '#']) {
            return Err(reason(
                "must not include a scheme, path, query, or fragment",
            ));
        }
        if value.contains(':') {
            return Err(reason("must not include a scheme, port, or IPv6 literal"));
        }
        if value.contains('@') {
            return Err(reason("must not include userinfo"));
        }
        if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Err(reason("must be lowercase"));
        }
        if value.len() > 253 {
            return Err(reason("must be at most 253 characters"));
        }
        let mut labels = 0_usize;
        let mut last_label = "";
        for label in value.split('.') {
            labels += 1;
            last_label = label;
            if label.is_empty() || label.len() > 63 {
                return Err(reason(
                    "labels must be 1-63 characters (no leading, trailing, or doubled dots)",
                ));
            }
            if label.starts_with('-') || label.ends_with('-') {
                return Err(reason("labels must not start or end with a hyphen"));
            }
            if !label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            {
                return Err(reason(
                    "labels may contain only lowercase ASCII letters, digits, and hyphens",
                ));
            }
        }
        if labels < 2 {
            return Err(reason("must be a DNS name with at least two labels"));
        }
        if last_label.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(reason("must be a DNS name, not an IP address"));
        }
        // Final word: the WHATWG host parser (the `url` crate, as browsers and
        // the login service see it) must read the value back as exactly this
        // domain. That rejects hex/octal IPv4 forms such as `127.0.0.0x7f`
        // and invalid punycode such as `xn--a.com`.
        match url::Host::parse(value) {
            Ok(url::Host::Domain(domain)) if domain == value => {}
            _ => {
                return Err(reason(
                    "must be a DNS name that browsers read back unchanged",
                ));
            }
        }
        Ok(Self(value.to_owned()))
    }

    /// The bare host name, for example `login.example.com`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The HTTPS origin, for example `https://login.example.com`.
    pub fn origin(&self) -> String {
        format!("https://{}", self.0)
    }
}

impl Display for LoginPublicHost {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Settings for the `xiao-login` service. Separate from [`Config`] because
/// that binary has no Discord token and holds only a database credential
/// confined to the `login_*` functions.
#[derive(Clone)]
pub struct LoginConfig {
    public_host: LoginPublicHost,
    database_url_file: PathBuf,
    database_tls: DatabaseTlsConfig,
    listen_addr: SocketAddr,
    chromium_path: PathBuf,
    telemetry: TelemetryConfig,
}

const DEFAULT_LOGIN_LISTEN_ADDR: &str = "0.0.0.0:8080";
const DEFAULT_LOGIN_CHROMIUM_PATH: &str = "/usr/bin/chromium";

impl LoginConfig {
    /// Loads login service configuration from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(env_lookup)
    }

    fn from_lookup<F>(mut lookup: F) -> Result<Self, ConfigError>
    where
        F: FnMut(&'static str) -> Result<Option<String>, ConfigError>,
    {
        let public_host = LoginPublicHost::parse(&required(&mut lookup, "LOGIN_PUBLIC_HOST")?)?;
        let database_url_file = absolute_login_path(
            "LOGIN_DATABASE_URL_FILE",
            required(&mut lookup, "LOGIN_DATABASE_URL_FILE")?,
        )?;
        let database_tls = load_database_tls(&mut lookup)?;
        let listen_addr = optional(&mut lookup, "LOGIN_LISTEN_ADDR")?
            .unwrap_or_else(|| DEFAULT_LOGIN_LISTEN_ADDR.to_owned())
            .trim()
            .parse::<SocketAddr>()
            .map_err(|_| ConfigError::InvalidLogin {
                variable: "LOGIN_LISTEN_ADDR",
                reason: "expected an IP socket address such as 0.0.0.0:8080",
            })?;
        let chromium_path = absolute_login_path(
            "LOGIN_CHROMIUM_PATH",
            optional(&mut lookup, "LOGIN_CHROMIUM_PATH")?
                .unwrap_or_else(|| DEFAULT_LOGIN_CHROMIUM_PATH.to_owned()),
        )?;
        let telemetry = load_telemetry(&mut lookup)?;
        Ok(Self {
            public_host,
            database_url_file,
            database_tls,
            listen_addr,
            chromium_path,
            telemetry,
        })
    }

    /// Public host `Origin` and `Host` headers are checked against.
    pub fn public_host(&self) -> &LoginPublicHost {
        &self.public_host
    }

    /// Path of the owner-only file holding the `xiao_login` role's URL.
    pub fn database_url_file(&self) -> &Path {
        &self.database_url_file
    }

    /// Reads the database URL from [`Self::database_url_file`], failing closed
    /// unless the file is an owner-only regular file, and validates the URL
    /// against the configured TLS mode. The URL is zeroized on drop.
    pub fn read_database_url(&self) -> Result<Zeroizing<String>, ConfigError> {
        let url = read_secret_file(&self.database_url_file, "LOGIN_DATABASE_URL_FILE")?;
        validate_database_url(&url, &self.database_tls)?;
        Ok(url)
    }

    /// Returns the PostgreSQL TLS configuration.
    pub fn database_tls(&self) -> &DatabaseTlsConfig {
        &self.database_tls
    }

    /// Address the plain-HTTP listener binds to (behind the TLS proxy).
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// Absolute path of the Chromium binary.
    pub fn chromium_path(&self) -> &Path {
        &self.chromium_path
    }

    /// Returns telemetry settings.
    pub fn telemetry(&self) -> &TelemetryConfig {
        &self.telemetry
    }
}

impl Debug for LoginConfig {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        // Only the credential's file path is held; the URL is read on demand.
        formatter
            .debug_struct("LoginConfig")
            .field("public_host", &self.public_host)
            .field("database_url_file", &self.database_url_file)
            .field("database_tls", &self.database_tls)
            .field("listen_addr", &self.listen_addr)
            .field("chromium_path", &self.chromium_path)
            .field("telemetry", &self.telemetry)
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
    /// Only some of a Rust+ variable group was set; each group is all-or-nothing.
    PartialRustPlus { missing: &'static str },
    /// The legacy Rust+ account variables were set without the core group.
    LegacyRustPlusWithoutCore,
    /// `LOGIN_PUBLIC_HOST` is not a valid host-only DNS name.
    InvalidLoginPublicHost { reason: &'static str },
    /// A `xiao-login` variable was set to an unusable value.
    InvalidLogin {
        variable: &'static str,
        reason: &'static str,
    },
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
                "incomplete Rust+ configuration: {missing} is required when any variable in its group is set"
            ),
            Self::LegacyRustPlusWithoutCore => formatter.write_str(
                "RUSTPLUS_STEAM_ID and RUSTPLUS_STEAM_AUTH_TOKEN_FILE require RUSTPLUS_FCM_SENDER_ID, SECRET_KEYRING_FILE, and SECRET_ACTIVE_KEY_ID to be set",
            ),
            Self::InvalidLoginPublicHost { reason } => {
                write!(formatter, "invalid LOGIN_PUBLIC_HOST: {reason}")
            }
            Self::InvalidLogin { variable, reason } => {
                write!(formatter, "invalid {variable}: {reason}")
            }
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

fn env_lookup(name: &'static str) -> Result<Option<String>, ConfigError> {
    std::env::var(name).map(Some).or_else(|error| match error {
        std::env::VarError::NotPresent => Ok(None),
        std::env::VarError::NotUnicode(_) => Err(ConfigError::InvalidUnicode { variable: name }),
    })
}

fn load_telemetry<F>(lookup: &mut F) -> Result<TelemetryConfig, ConfigError>
where
    F: FnMut(&'static str) -> Result<Option<String>, ConfigError>,
{
    let filter = optional(lookup, "RUST_LOG")?.unwrap_or_else(|| "info".to_owned());
    if filter.trim().is_empty() {
        return Err(ConfigError::Empty {
            variable: "RUST_LOG",
        });
    }
    let format = optional(lookup, "TELEMETRY_FORMAT")?
        .unwrap_or_else(|| "compact".to_owned())
        .parse()
        .map_err(|_| ConfigError::InvalidTelemetryFormat {
            value: "TELEMETRY_FORMAT",
        })?;
    Ok(TelemetryConfig { filter, format })
}

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
#[allow(clippy::expect_used)]
mod tests {
    use super::{
        Config, ConfigError, DatabaseTlsConfig, LegacyAccount, LoginConfig, LoginPublicHost,
        MigrationConfig, TelemetryFormat, read_secret_file,
    };
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

    const CORE: [(&str, &str); 3] = [
        ("RUSTPLUS_FCM_SENDER_ID", "sender-secret"),
        ("SECRET_KEYRING_FILE", "/run/secrets/keyring"),
        ("SECRET_ACTIVE_KEY_ID", "key-2026-10"),
    ];
    const LEGACY: [(&str, &str); 2] = [
        ("RUSTPLUS_STEAM_ID", "76561198000000001"),
        (
            "RUSTPLUS_STEAM_AUTH_TOKEN_FILE",
            "/run/secrets/rustplus_token",
        ),
    ];

    /// Builds a bot config from the base settings, the listed groups,
    /// `overrides` (which replace or add variables), minus `omit`.
    fn with_groups(
        core: bool,
        legacy: bool,
        overrides: &[(&'static str, &'static str)],
        omit: &[&str],
    ) -> Result<Config, ConfigError> {
        let mut values = vec![
            ("DISCORD_TOKEN", "token"),
            ("DATABASE_URL", "postgres://localhost/bot"),
            ("DATABASE_TLS_MODE", "insecure"),
        ];
        let groups = [(core, CORE.as_slice()), (legacy, LEGACY.as_slice())];
        for (name, value) in groups
            .into_iter()
            .filter_map(|(enabled, group)| enabled.then_some(group))
            .flatten()
        {
            if omit.contains(name) {
                continue;
            }
            values.push((name, value));
        }
        for (name, value) in overrides {
            values.retain(|(existing, _)| existing != name);
            values.push((name, value));
        }
        config(&values)
    }

    fn with_rustplus(
        overrides: &[(&'static str, &'static str)],
        omit: Option<&str>,
    ) -> Result<Config, ConfigError> {
        let omit: Vec<&str> = omit.into_iter().collect();
        with_groups(true, true, overrides, &omit)
    }

    #[test]
    fn complete_rustplus_configuration_is_typed_and_redacted() {
        let result = with_rustplus(&[], None);
        assert!(result.is_ok());
        if let Ok(config) = result {
            let rustplus = config.rustplus();
            assert!(rustplus.is_some());
            if let Some(rustplus) = rustplus {
                assert_eq!(rustplus.fcm_sender_id(), "sender-secret");
                assert_eq!(rustplus.active_key_id(), "key-2026-10");
                assert_eq!(
                    rustplus.keyring_file(),
                    std::path::Path::new("/run/secrets/keyring")
                );
                let legacy = rustplus.legacy_account();
                assert_eq!(
                    legacy.map(LegacyAccount::steam_id),
                    Some(76_561_198_000_000_001)
                );
                assert_eq!(
                    legacy.map(LegacyAccount::token_file),
                    Some(std::path::Path::new("/run/secrets/rustplus_token"))
                );
            }
            let debug = format!("{config:?}");
            assert!(!debug.contains("sender-secret"));
            assert_eq!(debug.matches("[REDACTED]").count(), 3);
        }
    }

    #[test]
    fn rustplus_group_matrix() {
        // (core, legacy, expected)
        enum Expect {
            Disabled,
            CoreOnly,
            CoreAndLegacy,
            LegacyWithoutCore,
        }
        let cases = [
            (false, false, Expect::Disabled),
            (true, false, Expect::CoreOnly),
            (true, true, Expect::CoreAndLegacy),
            (false, true, Expect::LegacyWithoutCore),
        ];
        for (core, legacy, expect) in cases {
            let result = with_groups(core, legacy, &[], &[]);
            match expect {
                Expect::Disabled => {
                    assert!(result.is_ok_and(|config| config.rustplus().is_none()));
                }
                Expect::CoreOnly => {
                    let config = result.expect("core-only configuration parses");
                    let rustplus = config.rustplus().expect("core group enables rustplus");
                    assert!(rustplus.legacy_account().is_none());
                    assert_eq!(rustplus.active_key_id(), "key-2026-10");
                }
                Expect::CoreAndLegacy => {
                    let config = result.expect("both groups parse");
                    let rustplus = config.rustplus().expect("core group enables rustplus");
                    assert!(rustplus.legacy_account().is_some());
                }
                Expect::LegacyWithoutCore => {
                    assert_eq!(result.err(), Some(ConfigError::LegacyRustPlusWithoutCore));
                }
            }
        }
    }

    #[test]
    fn partial_groups_combined_report_the_core_group_first() {
        // Partial legacy group with the core group wholly absent is still a
        // partial-group error, not "legacy without core".
        let result = with_groups(false, true, &[], &["RUSTPLUS_STEAM_AUTH_TOKEN_FILE"]);
        assert!(matches!(
            result.err(),
            Some(ConfigError::PartialRustPlus {
                missing: "RUSTPLUS_STEAM_AUTH_TOKEN_FILE"
            })
        ));
        // A partial core group with a complete legacy group names the core gap.
        let result = with_groups(true, true, &[], &["SECRET_ACTIVE_KEY_ID"]);
        assert!(matches!(
            result.err(),
            Some(ConfigError::PartialRustPlus {
                missing: "SECRET_ACTIVE_KEY_ID"
            })
        ));
    }

    #[test]
    fn legacy_account_debug_redacts_the_steam_id() {
        let config = with_groups(true, true, &[], &[]).expect("both groups parse");
        let legacy = config
            .rustplus()
            .and_then(super::RustPlusConfig::legacy_account)
            .expect("legacy account present");
        let steam_id = legacy.steam_id().to_string();
        for text in [format!("{legacy:?}"), format!("{:?}", config.rustplus())] {
            assert!(!text.contains(&steam_id), "SteamID leaked: {text}");
        }
    }

    #[test]
    fn legacy_without_core_error_names_variables_without_values() {
        let message = ConfigError::LegacyRustPlusWithoutCore.to_string();
        for name in super::RUSTPLUS_CORE_VARIABLES
            .iter()
            .chain(&super::RUSTPLUS_LEGACY_VARIABLES)
        {
            assert!(message.contains(name), "{message} should name {name}");
        }
        assert!(!message.contains("76561198000000001"));
    }

    #[test]
    fn partial_core_group_reports_each_missing_variable() {
        for (missing, _) in CORE {
            for legacy in [false, true] {
                assert_eq!(
                    with_groups(true, legacy, &[], &[missing]).err(),
                    Some(ConfigError::PartialRustPlus { missing }),
                    "core variable {missing} missing, legacy={legacy}"
                );
            }
        }
    }

    #[test]
    fn partial_legacy_group_reports_each_missing_variable() {
        for (missing, _) in LEGACY {
            assert_eq!(
                with_groups(true, true, &[], &[missing]).err(),
                Some(ConfigError::PartialRustPlus { missing }),
                "legacy variable {missing} missing"
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
    fn login_public_host_accepts_valid_hosts() {
        for host in [
            "login.example.com",
            "a-b.c.example",
            "xn--bcher-kva.example",
            "a1.b2",
            "x.example",
        ] {
            let parsed = LoginPublicHost::parse(host);
            assert!(parsed.is_ok(), "{host} should be accepted");
            if let Ok(parsed) = parsed {
                assert_eq!(parsed.as_str(), host);
                assert_eq!(parsed.origin(), format!("https://{host}"));
            }
        }
        let max_label = "a".repeat(63);
        assert!(LoginPublicHost::parse(&format!("{max_label}.example")).is_ok());
        // 253 characters in total: 3 * 63 + 3 dots (192) + a 61 character label.
        let longest = format!("{max_label}.{max_label}.{max_label}.{}", "b".repeat(61));
        assert_eq!(longest.len(), 253);
        assert!(LoginPublicHost::parse(&longest).is_ok());
    }

    #[test]
    fn login_public_host_rejects_invalid_hosts() {
        let too_long_label = format!("{}.example", "a".repeat(64));
        let too_long_total = format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "a".repeat(63),
            "a".repeat(63),
            "b".repeat(62)
        );
        assert_eq!(too_long_total.len(), 254);
        let invalid = [
            "",
            " ",
            "login.example.com ",
            " login.example.com",
            "login .example.com",
            "login.example.com\n",
            "https://x.example",
            "http://x.example",
            "x.example/path",
            "x.example?query=1",
            "x.example#fragment",
            "x.example:443",
            "user@x.example",
            "localhost",
            "1.2.3.4",
            "0x7f.1",
            "127.0.0.0x7f",
            "1.0x",
            "0x.0x1",
            "a.0x7f",
            "xn--a.com",
            "xn--.com",
            "[::1]",
            "::1",
            "X.Example.com",
            "x.example.",
            ".x.example",
            "x..example",
            "-x.example",
            "x-.example",
            "x.-example",
            "x.example-",
            "x_y.example",
            "b\u{fc}cher.example",
            too_long_label.as_str(),
            too_long_total.as_str(),
        ];
        for host in invalid {
            assert!(
                matches!(
                    LoginPublicHost::parse(host),
                    Err(ConfigError::InvalidLoginPublicHost { .. })
                ),
                "{host:?} should be rejected"
            );
        }
    }

    #[test]
    fn login_public_host_error_does_not_echo_the_value() {
        let error = LoginPublicHost::parse("https://secret-host.example/path").err();
        assert!(error.is_some_and(|error| !error.to_string().contains("secret-host")));
    }

    #[test]
    fn bot_config_parses_optional_login_public_host() {
        let absent = with_groups(true, false, &[], &[]).expect("parses");
        assert!(absent.login_public_host().is_none());

        let present = with_groups(
            true,
            false,
            &[("LOGIN_PUBLIC_HOST", "login.example.com")],
            &[],
        )
        .expect("parses");
        assert_eq!(
            present.login_public_host().map(LoginPublicHost::as_str),
            Some("login.example.com")
        );

        assert!(matches!(
            with_groups(
                true,
                false,
                &[("LOGIN_PUBLIC_HOST", "https://x.example")],
                &[]
            ),
            Err(ConfigError::InvalidLoginPublicHost { .. })
        ));
        assert_eq!(
            with_groups(true, false, &[("LOGIN_PUBLIC_HOST", "  ")], &[]).err(),
            Some(ConfigError::Empty {
                variable: "LOGIN_PUBLIC_HOST"
            })
        );
    }

    #[test]
    fn login_enabled_requires_core_group_and_public_host() {
        let host = [("LOGIN_PUBLIC_HOST", "login.example.com")];
        // (core group, legacy group, host, login_enabled)
        let cases = [
            (false, false, false, false),
            (false, false, true, false),
            (true, false, false, false),
            (true, false, true, true),
            (true, true, false, false),
            (true, true, true, true),
        ];
        for (core, legacy, with_host, enabled) in cases {
            let overrides: &[(&'static str, &'static str)] = if with_host { &host } else { &[] };
            let config = with_groups(core, legacy, overrides, &[]).expect("parses");
            assert_eq!(
                config.login_enabled(),
                enabled,
                "core={core} legacy={legacy} host={with_host}"
            );
        }
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

    fn login_config(values: &[(&str, &str)]) -> Result<LoginConfig, ConfigError> {
        let values: HashMap<String, String> = values
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        LoginConfig::from_lookup(|name| Ok(values.get(name).cloned()))
    }

    const LOGIN_BASE: [(&str, &str); 3] = [
        ("LOGIN_PUBLIC_HOST", "login.example.com"),
        ("LOGIN_DATABASE_URL_FILE", "/run/secrets/login_database_url"),
        ("DATABASE_TLS_MODE", "insecure"),
    ];

    #[test]
    fn login_config_applies_defaults() {
        let config = login_config(&LOGIN_BASE).expect("parses");
        assert_eq!(config.public_host().as_str(), "login.example.com");
        assert_eq!(
            config.database_url_file(),
            std::path::Path::new("/run/secrets/login_database_url")
        );
        assert_eq!(config.listen_addr(), "0.0.0.0:8080".parse().expect("addr"));
        assert_eq!(
            config.chromium_path(),
            std::path::Path::new("/usr/bin/chromium")
        );
        assert_eq!(config.telemetry().filter, "info");
        assert_eq!(config.telemetry().format, TelemetryFormat::Compact);
        assert_eq!(config.database_tls(), &DatabaseTlsConfig::Insecure);
    }

    #[test]
    fn login_config_honours_overrides() {
        let mut values = LOGIN_BASE.to_vec();
        values.extend([
            ("LOGIN_LISTEN_ADDR", "127.0.0.1:9000"),
            ("LOGIN_CHROMIUM_PATH", "/opt/chromium/chrome"),
            ("RUST_LOG", "debug"),
            ("TELEMETRY_FORMAT", "json"),
        ]);
        let config = login_config(&values).expect("parses");
        assert_eq!(
            config.listen_addr(),
            "127.0.0.1:9000".parse().expect("addr")
        );
        assert_eq!(
            config.chromium_path(),
            std::path::Path::new("/opt/chromium/chrome")
        );
        assert_eq!(config.telemetry().filter, "debug");
        assert_eq!(config.telemetry().format, TelemetryFormat::Json);
    }

    #[test]
    fn login_config_reports_missing_required_variables() {
        for missing in ["LOGIN_PUBLIC_HOST", "LOGIN_DATABASE_URL_FILE"] {
            let values: Vec<_> = LOGIN_BASE
                .iter()
                .copied()
                .filter(|(name, _)| *name != missing)
                .collect();
            assert_eq!(
                login_config(&values).err(),
                Some(ConfigError::Missing { variable: missing })
            );
        }
        let values: Vec<_> = LOGIN_BASE
            .iter()
            .copied()
            .filter(|(name, _)| *name != "DATABASE_TLS_MODE")
            .collect();
        assert_eq!(
            login_config(&values).err(),
            Some(ConfigError::Missing {
                variable: "DATABASE_CA_FILE"
            })
        );
    }

    #[test]
    fn login_config_rejects_invalid_values() {
        let with = |name: &'static str, value: &'static str| {
            let mut values: Vec<_> = LOGIN_BASE
                .iter()
                .copied()
                .filter(|(existing, _)| *existing != name)
                .collect();
            values.push((name, value));
            login_config(&values)
        };
        assert!(matches!(
            with("LOGIN_PUBLIC_HOST", "https://login.example.com"),
            Err(ConfigError::InvalidLoginPublicHost { .. })
        ));
        for (variable, value) in [
            ("LOGIN_DATABASE_URL_FILE", "relative/url"),
            ("LOGIN_CHROMIUM_PATH", "chromium"),
            ("LOGIN_LISTEN_ADDR", "not-an-address"),
            ("LOGIN_LISTEN_ADDR", "localhost:8080"),
        ] {
            assert!(
                matches!(
                    with(variable, value),
                    Err(ConfigError::InvalidLogin { variable: v, .. }) if v == variable
                ),
                "{variable}={value} should be rejected"
            );
        }
    }

    #[test]
    fn login_config_shares_database_tls_parsing_with_the_bot() {
        let with_tls = |extra: &[(&str, &str)]| {
            let mut values: Vec<_> = LOGIN_BASE
                .iter()
                .copied()
                .filter(|(name, _)| *name != "DATABASE_TLS_MODE")
                .collect();
            values.extend_from_slice(extra);
            login_config(&values)
        };
        let verify_full = with_tls(&[
            ("DATABASE_TLS_MODE", "verify-full"),
            ("DATABASE_CA_FILE", "/run/secrets/ca.pem"),
        ])
        .expect("parses");
        assert_eq!(
            verify_full.database_tls(),
            &DatabaseTlsConfig::VerifyFull {
                ca_file: "/run/secrets/ca.pem".into()
            }
        );
        let insecure = with_tls(&[("DATABASE_TLS_MODE", "insecure")]).expect("parses");
        assert_eq!(insecure.database_tls(), &DatabaseTlsConfig::Insecure);
        assert_eq!(
            with_tls(&[("DATABASE_TLS_MODE", "verify-full")]).err(),
            Some(ConfigError::Missing {
                variable: "DATABASE_CA_FILE"
            })
        );
        assert_eq!(
            with_tls(&[("DATABASE_TLS_MODE", "bogus")]).err(),
            Some(ConfigError::InvalidDatabaseTlsMode {
                value: "DATABASE_TLS_MODE"
            })
        );
    }

    #[test]
    fn login_config_debug_does_not_expose_the_database_url()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let url_path = dir.path().join("login_database_url");
        fs::write(&url_path, "postgres://xiao_login:hunter2@db.internal/bot\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&url_path, fs::Permissions::from_mode(0o400))?;
        }
        let url_path_str = url_path.to_str().ok_or("non-utf8 path")?;
        let config = login_config(&[
            ("LOGIN_PUBLIC_HOST", "login.example.com"),
            ("LOGIN_DATABASE_URL_FILE", url_path_str),
            ("DATABASE_TLS_MODE", "insecure"),
        ])?;
        let url = config.read_database_url()?;
        assert_eq!(&*url, "postgres://xiao_login:hunter2@db.internal/bot");
        let debug = format!("{config:?}");
        assert!(!debug.contains("hunter2"));
        assert!(!debug.contains("postgres://"));
        assert!(debug.contains("login.example.com"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn login_database_url_file_must_be_owner_only() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir()?;
        let url_path = dir.path().join("login_database_url");
        fs::write(&url_path, "postgres://xiao_login@db.internal/bot")?;
        fs::set_permissions(&url_path, fs::Permissions::from_mode(0o644))?;
        let url_path_str = url_path.to_str().ok_or("non-utf8 path")?;
        let config = login_config(&[
            ("LOGIN_PUBLIC_HOST", "login.example.com"),
            ("LOGIN_DATABASE_URL_FILE", url_path_str),
            ("DATABASE_TLS_MODE", "insecure"),
        ])?;
        assert!(matches!(
            config.read_database_url(),
            Err(ConfigError::InvalidSecretFile {
                file_variable: "LOGIN_DATABASE_URL_FILE",
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn login_database_url_is_validated_against_tls_mode() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let url_path = dir.path().join("login_database_url");
        fs::write(
            &url_path,
            "postgres://xiao_login@db.internal/bot?sslmode=disable",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&url_path, fs::Permissions::from_mode(0o400))?;
        }
        let url_path_str = url_path.to_str().ok_or("non-utf8 path")?;
        let config = login_config(&[
            ("LOGIN_PUBLIC_HOST", "login.example.com"),
            ("LOGIN_DATABASE_URL_FILE", url_path_str),
            ("DATABASE_TLS_MODE", "verify-full"),
            ("DATABASE_CA_FILE", "/run/secrets/ca.pem"),
        ])?;
        assert!(matches!(
            config.read_database_url(),
            Err(ConfigError::InvalidDatabaseUrl { .. })
        ));
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
