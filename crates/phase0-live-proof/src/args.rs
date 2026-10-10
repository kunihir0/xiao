//! Hand-parsed CLI and the pre-network gate.
//!
//! Error values never echo operator input: a mistyped flag value may be a
//! secret.

use std::{
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
};

use thiserror::Error;
use zeroize::Zeroizing;

pub const USAGE: &str = "usage: phase0-live-proof run --evidence-dir DIR --fcm-sender-id ID \
--steam-id U64 --steam-token-file PATH";

const MAX_SENDER_ID_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Error)]
pub enum UsageError {
    #[error("missing command")]
    MissingCommand,
    #[error("unknown command")]
    UnknownCommand,
    #[error("unknown or non-UTF-8 flag")]
    UnknownFlag,
    #[error("duplicate flag --{0}")]
    DuplicateFlag(&'static str),
    #[error("missing required flag --{0}")]
    MissingFlag(&'static str),
    #[error("flag --{0} requires a value")]
    MissingValue(&'static str),
    #[error("flag --{0} has an invalid value")]
    InvalidValue(&'static str),
}

/// Refusals raised after parsing but before any network or file creation.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Error)]
pub enum Refusal {
    #[error("evidence directory already exists")]
    EvidenceDirExists,
    #[error("evidence directory parent is not an existing directory")]
    EvidenceParentMissing,
}

pub struct RunConfig {
    pub evidence_dir: PathBuf,
    pub fcm_sender_id: Zeroizing<String>,
    pub steam_id: u64,
    pub steam_token_file: PathBuf,
}

/// Debug prints no values: every field is operator-supplied and some are
/// identifiers or credential locations.
impl fmt::Debug for RunConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RunConfig")
            .field("evidence_dir", &"<redacted>")
            .field("fcm_sender_id", &"<redacted>")
            .field("steam_id", &"<redacted>")
            .field("steam_token_file", &"<redacted>")
            .finish()
    }
}

#[derive(Debug)]
pub enum Command {
    Run(RunConfig),
}

#[derive(Default)]
struct Collected {
    evidence_dir: Option<PathBuf>,
    fcm_sender_id: Option<String>,
    steam_id: Option<u64>,
    steam_token_file: Option<PathBuf>,
}

fn set<T>(slot: &mut Option<T>, flag: &'static str, value: T) -> Result<(), UsageError> {
    if slot.is_some() {
        return Err(UsageError::DuplicateFlag(flag));
    }
    *slot = Some(value);
    Ok(())
}

fn utf8(value: OsString, flag: &'static str) -> Result<String, UsageError> {
    value
        .into_string()
        .map_err(|_| UsageError::InvalidValue(flag))
}

/// Parses everything after the program name.
pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Command, UsageError> {
    let mut args = args.into_iter();
    let command = args.next().ok_or(UsageError::MissingCommand)?;
    if command.to_str() != Some("run") {
        return Err(UsageError::UnknownCommand);
    }
    let mut collected = Collected::default();
    while let Some(flag) = args.next() {
        let flag = flag.to_str().ok_or(UsageError::UnknownFlag)?;
        match flag {
            "--evidence-dir" => {
                let value = args
                    .next()
                    .ok_or(UsageError::MissingValue("evidence-dir"))?;
                set(
                    &mut collected.evidence_dir,
                    "evidence-dir",
                    PathBuf::from(value),
                )?;
            }
            "--fcm-sender-id" => {
                let value = args
                    .next()
                    .ok_or(UsageError::MissingValue("fcm-sender-id"))?;
                let value = utf8(value, "fcm-sender-id")?;
                if value.is_empty()
                    || value.len() > MAX_SENDER_ID_BYTES
                    || value.chars().any(|c| c.is_control() || c.is_whitespace())
                {
                    return Err(UsageError::InvalidValue("fcm-sender-id"));
                }
                set(&mut collected.fcm_sender_id, "fcm-sender-id", value)?;
            }
            "--steam-id" => {
                let value = args.next().ok_or(UsageError::MissingValue("steam-id"))?;
                let parsed = utf8(value, "steam-id")?
                    .parse::<u64>()
                    .ok()
                    .filter(|id| *id != 0)
                    .ok_or(UsageError::InvalidValue("steam-id"))?;
                set(&mut collected.steam_id, "steam-id", parsed)?;
            }
            "--steam-token-file" => {
                let value = args
                    .next()
                    .ok_or(UsageError::MissingValue("steam-token-file"))?;
                set(
                    &mut collected.steam_token_file,
                    "steam-token-file",
                    PathBuf::from(value),
                )?;
            }
            _ => return Err(UsageError::UnknownFlag),
        }
    }
    Ok(Command::Run(RunConfig {
        evidence_dir: collected
            .evidence_dir
            .ok_or(UsageError::MissingFlag("evidence-dir"))?,
        fcm_sender_id: Zeroizing::new(
            collected
                .fcm_sender_id
                .ok_or(UsageError::MissingFlag("fcm-sender-id"))?,
        ),
        steam_id: collected
            .steam_id
            .ok_or(UsageError::MissingFlag("steam-id"))?,
        steam_token_file: collected
            .steam_token_file
            .ok_or(UsageError::MissingFlag("steam-token-file"))?,
    }))
}

impl RunConfig {
    /// Pre-network gate: the evidence directory must not exist yet (its
    /// parent must). Touches nothing on disk.
    pub fn preflight(&self) -> Result<(), Refusal> {
        preflight_evidence_dir(&self.evidence_dir)
    }
}

fn preflight_evidence_dir(dir: &Path) -> Result<(), Refusal> {
    // `symlink_metadata` so a dangling symlink also counts as existing.
    if std::fs::symlink_metadata(dir).is_ok() {
        return Err(Refusal::EvidenceDirExists);
    }
    let parent = dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if parent.is_dir() {
        Ok(())
    } else {
        Err(Refusal::EvidenceParentMissing)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    const FULL: [&str; 9] = [
        "run",
        "--evidence-dir",
        "/tmp/p23-unit-never-created",
        "--fcm-sender-id",
        "123456",
        "--steam-id",
        "76561198000000000",
        "--steam-token-file",
        "/nonexistent/token",
    ];

    fn parse(parts: &[&str]) -> Result<RunConfig, UsageError> {
        parse_args(argv(parts)).map(|Command::Run(config)| config)
    }

    #[test]
    fn full_command_line_parses() {
        let config = parse(&FULL).expect("parse");
        assert_eq!(config.steam_id, 76_561_198_000_000_000);
        assert_eq!(config.preflight(), Ok(()));
    }

    #[test]
    fn flags_may_appear_in_any_order() {
        let mut parts: Vec<&str> = vec!["run"];
        for pair in FULL[1..].chunks(2).rev() {
            parts.extend(pair);
        }
        assert!(parse(&parts).is_ok());
    }

    #[test]
    fn missing_command_and_unknown_command_are_usage_errors() {
        assert_eq!(parse(&[]).err(), Some(UsageError::MissingCommand));
        assert_eq!(parse(&["probe"]).err(), Some(UsageError::UnknownCommand));
        assert_eq!(
            parse(&["--evidence-dir", "x"]).err(),
            Some(UsageError::UnknownCommand)
        );
    }

    #[test]
    fn every_flag_is_required() {
        for (index, name) in [
            (1, "evidence-dir"),
            (3, "fcm-sender-id"),
            (5, "steam-id"),
            (7, "steam-token-file"),
        ] {
            let mut parts = FULL.to_vec();
            parts.drain(index..=index + 1);
            assert_eq!(parse(&parts).err(), Some(UsageError::MissingFlag(name)));
        }
    }

    #[test]
    fn duplicate_flags_are_rejected() {
        for (index, name) in [
            (1, "evidence-dir"),
            (3, "fcm-sender-id"),
            (5, "steam-id"),
            (7, "steam-token-file"),
        ] {
            let mut parts = FULL.to_vec();
            parts.extend_from_slice(&FULL[index..=index + 1]);
            assert_eq!(parse(&parts).err(), Some(UsageError::DuplicateFlag(name)));
        }
    }

    #[test]
    fn unknown_flags_and_stray_positionals_are_rejected() {
        let mut parts = FULL.to_vec();
        parts.extend(["--live-now", "1"]);
        assert_eq!(parse(&parts).err(), Some(UsageError::UnknownFlag));
        let mut parts = FULL.to_vec();
        parts.push("stray");
        assert_eq!(parse(&parts).err(), Some(UsageError::UnknownFlag));
    }

    #[test]
    fn flag_without_value_and_invalid_values_are_rejected() {
        let mut parts = FULL.to_vec();
        parts.truncate(8);
        assert_eq!(
            parse(&parts).err(),
            Some(UsageError::MissingValue("steam-token-file"))
        );
        for (index, bad, name) in [
            (4, "", "fcm-sender-id"),
            (4, "has space", "fcm-sender-id"),
            (6, "0", "steam-id"),
            (6, "-1", "steam-id"),
            (6, "abc", "steam-id"),
            (6, "18446744073709551616", "steam-id"),
        ] {
            let mut parts = FULL.to_vec();
            parts[index] = bad;
            assert_eq!(parse(&parts).err(), Some(UsageError::InvalidValue(name)));
        }
    }

    #[test]
    fn usage_errors_never_echo_operator_input() {
        let mut parts = FULL.to_vec();
        parts[6] = "hunter2-secret";
        let message = parse(&parts).expect_err("error").to_string();
        assert!(!message.contains("hunter2"));
        let mut parts = FULL.to_vec();
        parts.push("--hunter2-secret");
        let message = parse(&parts).expect_err("error").to_string();
        assert!(!message.contains("hunter2"));
    }

    #[test]
    fn approved_run_id_flag_is_gone() {
        let mut parts = FULL.to_vec();
        parts.extend(["--approved-run-id", "phase0-23-0123abcd"]);
        assert_eq!(parse(&parts).err(), Some(UsageError::UnknownFlag));
        assert!(!USAGE.contains("approved"));
    }

    #[test]
    fn existing_evidence_directory_is_refused() {
        let base = std::env::temp_dir().join(format!("p23-args-{}", std::process::id()));
        std::fs::create_dir_all(&base).expect("base");
        let mut parts = FULL.to_vec();
        let existing = base.to_str().expect("utf8 temp dir").to_owned();
        parts[2] = &existing;
        let config = parse(&parts).expect("parse");
        assert_eq!(config.preflight(), Err(Refusal::EvidenceDirExists));

        let missing_parent = base.join("no-such-parent").join("evidence");
        let missing = missing_parent.to_str().expect("utf8").to_owned();
        parts[2] = &missing;
        let config = parse(&parts).expect("parse");
        assert_eq!(config.preflight(), Err(Refusal::EvidenceParentMissing));

        let fresh = base.join("evidence");
        let fresh_text = fresh.to_str().expect("utf8").to_owned();
        parts[2] = &fresh_text;
        let config = parse(&parts).expect("parse");
        assert_eq!(config.preflight(), Ok(()));
        assert!(!fresh.exists(), "preflight must not create anything");
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn dangling_symlink_evidence_path_is_refused_as_existing() {
        let base = std::env::temp_dir().join(format!("p23-symlink-{}", std::process::id()));
        std::fs::create_dir_all(&base).expect("base");
        let link = base.join("evidence");
        std::os::unix::fs::symlink(base.join("no-such-target"), &link).expect("symlink");
        assert!(!link.exists(), "link must dangle");
        let mut parts = FULL.to_vec();
        let link_text = link.to_str().expect("utf8").to_owned();
        parts[2] = &link_text;
        let config = parse(&parts).expect("parse");
        assert_eq!(config.preflight(), Err(Refusal::EvidenceDirExists));
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn config_debug_contains_no_values() {
        let config = parse(&FULL).expect("parse");
        let debug = format!("{config:?}");
        for secret in [
            "p23-unit-never-created",
            "123456",
            "76561198000000000",
            "/nonexistent/token",
        ] {
            assert!(!debug.contains(secret), "{secret}");
        }
        assert!(debug.contains("<redacted>"));
    }
}
