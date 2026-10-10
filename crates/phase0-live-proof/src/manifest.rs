//! Redacted evidence manifest: fixed enums, fixed strings, and SHA-256
//! correlation hashes only. No row can carry a token, Steam ID, player ID,
//! address, port, or push payload byte because no field has a type that could
//! hold one.

use std::{
    fs::{DirBuilder, File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const MANIFEST_FILE_NAME: &str = "manifest.jsonl";
pub const DELIVERY_CONTRACT: &str = "at-least-once-unproven";

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("evidence directory could not be created exclusively")]
    CreateDirectory,
    #[error("manifest file could not be created exclusively")]
    CreateFile,
    #[error("manifest row could not be serialized")]
    Serialize,
    #[error("manifest row could not be written durably")]
    Write,
}

/// Stage identifiers in execution order, plus the terminal `no-go` marker.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    EnrollFcm,
    EnrollCompanion,
    McsReady,
    Pairing,
    SessionProductRoute,
    SessionWsCompare,
    EntityInfo,
    AlarmTrue,
    AlarmFalse,
    IndependentReceipt,
    RestartReconnect,
    NegativeAuth,
    ModernFisCompare,
    Teardown,
    NoGo,
}

impl Stage {
    /// Every live stage, in the order the harness runs them.
    pub const SEQUENCE: [Self; 14] = [
        Self::EnrollFcm,
        Self::EnrollCompanion,
        Self::McsReady,
        Self::Pairing,
        Self::SessionProductRoute,
        Self::SessionWsCompare,
        Self::EntityInfo,
        Self::AlarmTrue,
        Self::AlarmFalse,
        Self::IndependentReceipt,
        Self::RestartReconnect,
        Self::NegativeAuth,
        Self::ModernFisCompare,
        Self::Teardown,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EnrollFcm => "enroll-fcm",
            Self::EnrollCompanion => "enroll-companion",
            Self::McsReady => "mcs-ready",
            Self::Pairing => "pairing",
            Self::SessionProductRoute => "session-product-route",
            Self::SessionWsCompare => "session-ws-compare",
            Self::EntityInfo => "entity-info",
            Self::AlarmTrue => "alarm-true",
            Self::AlarmFalse => "alarm-false",
            Self::IndependentReceipt => "independent-receipt",
            Self::RestartReconnect => "restart-reconnect",
            Self::NegativeAuth => "negative-auth",
            Self::ModernFisCompare => "modern-fis-compare",
            Self::Teardown => "teardown",
            Self::NoGo => "no-go",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Fail,
    NotRun,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Route {
    None,
    FcmRegister,
    FacepunchRegister,
    McsPush,
    RustplusWssProduct,
    RustplusWsDirectNonproduct,
    Local,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusClass {
    Ok,
    Timeout,
    Cancelled,
    Transport,
    Rejected,
    UnrelatedRejection,
    RateLimited,
    ReenrollmentRequired,
    AuthTokenUnavailable,
    InvalidResponse,
    InvalidEndpoint,
    InvalidCredentials,
    FcmFailure,
    ProtocolFailure,
    UnexpectedEntityType,
    UnexpectedSuccess,
    HumanInputInvalid,
    Mismatch,
    Unsupported,
    NotApplicable,
    RequiredStageFailed,
    EventsLagged,
    HumanResetUnconfirmed,
    Internal,
}

/// The inputs hashed into `correlation_hash`. Identifiers enter only through
/// the one-way hash.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Correlation {
    pub entity_id: u32,
    pub value: &'static str,
    pub event_seq: u64,
}

impl Correlation {
    pub const NONE: Self = Self {
        entity_id: 0,
        value: "none",
        event_seq: 0,
    };
}

/// SHA-256 hex of `"{stage}|{entity_id}|{value}|{event_seq}"`.
#[must_use]
pub fn correlation_hash(stage: &str, entity_id: u32, value: &str, event_seq: u64) -> String {
    let preimage = format!("{stage}|{entity_id}|{value}|{event_seq}");
    let digest = Sha256::digest(preimage.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// One JSONL evidence row. `detail` and `delivery_contract` are `'static`
/// so only compile-time strings can reach the file.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ManifestRow {
    pub stage: Stage,
    pub route: Route,
    pub status: Status,
    pub started_at_utc: String,
    pub ended_at_utc: String,
    pub monotonic_ms: u64,
    pub correlation_hash: String,
    pub status_class: StatusClass,
    pub detail: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_contract: Option<&'static str>,
}

impl ManifestRow {
    /// Serializes the row as one line without the trailing newline. JSON
    /// string escaping makes embedded control characters inert.
    pub fn to_line(&self) -> Result<String, ManifestError> {
        serde_json::to_string(self).map_err(|_| ManifestError::Serialize)
    }
}

/// Destination for manifest rows; production appends to a file, tests collect.
pub trait RowSink {
    fn append(&mut self, row: &ManifestRow) -> Result<(), ManifestError>;
}

impl RowSink for Vec<ManifestRow> {
    fn append(&mut self, row: &ManifestRow) -> Result<(), ManifestError> {
        self.push(row.clone());
        Ok(())
    }
}

/// Append-only manifest file inside an exclusively created evidence directory.
pub struct ManifestWriter {
    file: File,
}

impl ManifestWriter {
    /// Creates `dir` (mode 0700, must not exist; the parent must) and the
    /// manifest (mode 0600, exclusive create).
    pub fn create(dir: &Path) -> Result<Self, ManifestError> {
        DirBuilder::new()
            .recursive(false)
            .mode(0o700)
            .create(dir)
            .map_err(|_| ManifestError::CreateDirectory)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(MANIFEST_FILE_NAME))
            .map_err(|_| ManifestError::CreateFile)?;
        Ok(Self { file })
    }
}

impl RowSink for ManifestWriter {
    fn append(&mut self, row: &ManifestRow) -> Result<(), ManifestError> {
        let mut line = row.to_line()?;
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.sync_data())
            .map_err(|_| ManifestError::Write)
    }
}

/// RFC 3339 UTC with millisecond precision (`2026-01-02T03:04:05.678Z`).
/// Instants before the Unix epoch clamp to the epoch.
#[must_use]
pub fn format_utc(time: SystemTime) -> String {
    let elapsed = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = elapsed.as_secs();
    let millis = elapsed.subsec_millis();
    let (year, month, day) = civil_from_days(seconds / 86_400);
    let of_day = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        of_day / 3_600,
        of_day % 3_600 / 60,
        of_day % 60
    )
}

/// Seconds since UTC midnight for `time`.
#[must_use]
pub fn seconds_of_day_utc(time: SystemTime) -> u32 {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    u32::try_from(seconds % 86_400).unwrap_or(0)
}

// Howard Hinnant's `civil_from_days`, restricted to days since 1970-01-01.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::{os::unix::fs::PermissionsExt, time::Duration};

    use super::*;

    fn row() -> ManifestRow {
        ManifestRow {
            stage: Stage::AlarmTrue,
            route: Route::RustplusWssProduct,
            status: Status::Pass,
            started_at_utc: "2026-01-02T03:04:05.000Z".to_owned(),
            ended_at_utc: "2026-01-02T03:04:06.000Z".to_owned(),
            monotonic_ms: 1_000,
            correlation_hash: correlation_hash("alarm-true", 7, "true", 1),
            status_class: StatusClass::Ok,
            detail: "entity_changed_observed",
            delivery_contract: Some(DELIVERY_CONTRACT),
        }
    }

    #[test]
    fn correlation_hash_matches_shasum_known_answer() {
        // printf '%s' 'stage|1|true|1' | shasum -a 256
        assert_eq!(
            correlation_hash("stage", 1, "true", 1),
            "19ea0720389699f3f53256ca5ed03b8e374888bf5720e5ff05a77013108e4c21"
        );
        // printf '%s' 'no-go|0|none|0' | shasum -a 256
        assert_eq!(
            correlation_hash("no-go", 0, "none", 0),
            "345ada4a2dfbc6e7f152a9277cd934ee634a30a7c64262b3b5d5c6cc26b5d1d1"
        );
    }

    #[test]
    fn row_has_exactly_the_specified_fields_and_no_secret_fields() {
        let value: serde_json::Value =
            serde_json::from_str(&row().to_line().expect("line")).expect("json");
        let object = value.as_object().expect("object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "correlation_hash",
                "delivery_contract",
                "detail",
                "ended_at_utc",
                "monotonic_ms",
                "route",
                "stage",
                "started_at_utc",
                "status",
                "status_class",
            ]
        );
        assert_eq!(object["stage"], "alarm-true");
        assert_eq!(object["route"], "rustplus-wss-product");
        assert_eq!(object["status"], "pass");
        assert_eq!(object["status_class"], "ok");
        assert_eq!(object["delivery_contract"], "at-least-once-unproven");
    }

    #[test]
    fn delivery_contract_is_omitted_when_absent() {
        let mut row = row();
        row.delivery_contract = None;
        assert!(!row.to_line().expect("line").contains("delivery_contract"));
    }

    #[test]
    fn control_characters_are_escaped_into_a_single_line() {
        let mut row = row();
        row.detail = "line\nbreak\u{1b}[31m\u{0}tab\t";
        let line = row.to_line().expect("line");
        assert!(!line.chars().any(char::is_control));
        let value: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(value["detail"], "line\nbreak\u{1b}[31m\u{0}tab\t");
    }

    #[test]
    fn stage_names_match_serialized_names_and_sequence_is_ordered() {
        for stage in Stage::SEQUENCE.into_iter().chain([Stage::NoGo]) {
            assert_eq!(
                serde_json::to_string(&stage).expect("stage"),
                format!("\"{}\"", stage.as_str())
            );
        }
        assert_eq!(Stage::SEQUENCE.first(), Some(&Stage::EnrollFcm));
        assert_eq!(Stage::SEQUENCE.last(), Some(&Stage::Teardown));
    }

    #[test]
    fn utc_formatting_known_instants() {
        assert_eq!(format_utc(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_utc(UNIX_EPOCH + Duration::from_millis(1_709_164_800_123)),
            "2024-02-29T00:00:00.123Z"
        );
        assert_eq!(
            format_utc(UNIX_EPOCH + Duration::from_secs(1_798_761_599)),
            "2026-12-31T23:59:59.000Z"
        );
        assert_eq!(
            seconds_of_day_utc(UNIX_EPOCH + Duration::from_secs(86_400 + 3_661)),
            3_661
        );
    }

    #[test]
    fn writer_creates_private_directory_and_file_exclusively() {
        let base = std::env::temp_dir().join(format!("p23-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&base).expect("base");
        let dir = base.join("evidence");
        let mut writer = ManifestWriter::create(&dir).expect("create");
        writer.append(&row()).expect("append");
        assert_eq!(
            std::fs::metadata(&dir).expect("dir").permissions().mode() & 0o777,
            0o700
        );
        let file = dir.join(MANIFEST_FILE_NAME);
        assert_eq!(
            std::fs::metadata(&file).expect("file").permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_to_string(&file)
                .expect("read")
                .lines()
                .count(),
            1
        );
        assert!(matches!(
            ManifestWriter::create(&dir),
            Err(ManifestError::CreateDirectory)
        ));
        std::fs::remove_dir_all(&base).expect("cleanup");
    }
}
