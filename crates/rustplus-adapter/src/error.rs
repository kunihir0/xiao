use std::fmt;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ParseError {
    TooLarge,
    Malformed,
    Missing(&'static str),
    Invalid(&'static str),
    Numeric(&'static str),
    Domain(&'static str),
    HostNotIpLiteral,
    SpecialUseIp,
}
impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => formatter.write_str("pairing value exceeds parser limit"),
            Self::Malformed => formatter.write_str("pairing JSON is malformed"),
            Self::Missing(field) => write!(formatter, "pairing field {field} is missing"),
            Self::Invalid(field) => write!(formatter, "pairing field {field} is invalid"),
            Self::Numeric(field) => write!(formatter, "pairing field {field} is out of range"),
            Self::Domain(field) => write!(formatter, "pairing field {field} violates domain rules"),
            Self::HostNotIpLiteral => formatter.write_str("pairing host is not an IP literal"),
            Self::SpecialUseIp => {
                formatter.write_str("pairing host uses a special-purpose IP range")
            }
        }
    }
}
impl std::error::Error for ParseError {}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum AdapterError {
    InvalidReconnectPolicy(&'static str),
    InvalidEndpoint,
    Client,
    Shutdown,
}
impl fmt::Display for AdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidReconnectPolicy(reason) => {
                write!(formatter, "invalid reconnect policy: {reason}")
            }
            Self::InvalidEndpoint => formatter.write_str("invalid Rust+ endpoint"),
            Self::Client => formatter.write_str("Rust+ client initialization failed"),
            Self::Shutdown => formatter.write_str("Rust+ session shutdown failed"),
        }
    }
}
impl std::error::Error for AdapterError {}
