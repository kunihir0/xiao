//! Tracing and operational telemetry wiring.

use bot_config::{Config, TelemetryConfig, TelemetryFormat};
use std::fmt::{self, Display, Formatter};
use tracing_subscriber::{EnvFilter, fmt as tracing_fmt};

/// Lifetime marker for the process telemetry setup.
///
/// The global subscriber cannot be replaced after installation. Keeping this
/// guard in the composition root makes the ownership and intended lifetime
/// explicit even though dropping it does not uninstall the subscriber.
pub struct TelemetryGuard {
    _private: (),
}

impl std::fmt::Debug for TelemetryGuard {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("TelemetryGuard")
    }
}

/// Telemetry initialization failures.
#[derive(Debug, Eq, PartialEq)]
pub enum TelemetryError {
    /// The configured filter directives could not be parsed.
    InvalidFilter,
    /// A global subscriber has already been installed.
    AlreadyInitialized,
}

impl Display for TelemetryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFilter => formatter.write_str("invalid telemetry filter"),
            Self::AlreadyInitialized => formatter.write_str("telemetry is already initialized"),
        }
    }
}

impl std::error::Error for TelemetryError {}

/// Installs the process-wide tracing subscriber exactly once.
pub fn init(config: TelemetryConfig) -> Result<TelemetryGuard, TelemetryError> {
    let filter = EnvFilter::try_new(config.filter).map_err(|_| TelemetryError::InvalidFilter)?;
    let result = match config.format {
        TelemetryFormat::Compact => tracing_fmt().with_env_filter(filter).compact().try_init(),
        TelemetryFormat::Pretty => tracing_fmt().with_env_filter(filter).pretty().try_init(),
        TelemetryFormat::Json => tracing_fmt().with_env_filter(filter).json().try_init(),
    };

    result.map_err(|_| TelemetryError::AlreadyInitialized)?;
    Ok(TelemetryGuard { _private: () })
}

/// Installs telemetry using the validated application configuration.
pub fn init_from_config(config: &Config) -> Result<TelemetryGuard, TelemetryError> {
    init(config.telemetry().clone())
}

#[cfg(test)]
mod tests {
    use super::{TelemetryError, init};
    use bot_config::{TelemetryConfig, TelemetryFormat};

    #[test]
    fn initialization_is_one_time() {
        let config = TelemetryConfig {
            filter: "off".to_owned(),
            format: TelemetryFormat::Compact,
        };
        let first = init(config.clone());
        assert!(first.is_ok());
        let second = init(config);
        assert_eq!(second.err(), Some(TelemetryError::AlreadyInitialized));
    }
}
