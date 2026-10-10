//! Isolated Rust+ live-proof harness for issue #23.
//!
//! `run` talks to real FCM, Expo, Facepunch, MCS, and Rust+ servers, but only
//! after the operator supplies a well-formed approval string and an evidence
//! directory that does not exist yet. Everything before that gate is offline.

mod args;
mod classify;
mod helpers;
mod live;
mod manifest;
mod receipt;
mod sequence;

use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use tokio_util::sync::CancellationToken;

use crate::{
    args::{Command, Refusal, RunConfig, USAGE, parse_args},
    helpers::validate_token_file,
    live::LiveExecutor,
    manifest::{ManifestError, ManifestWriter, RowSink},
    sequence::{StageExecutor, Verdict, run_sequence},
};

const EXIT_OK: u8 = 0;
const EXIT_USAGE: u8 = 64;
const EXIT_NO_INPUT: u8 = 66;
const EXIT_NO_GO: u8 = 70;
const EXIT_CANT_CREATE: u8 = 73;
const EXIT_NO_PERMISSION: u8 = 77;

fn main() -> ExitCode {
    ExitCode::from(cli(env::args_os().skip(1), run_live))
}

/// Parses arguments and applies the pre-network gate. `run` is invoked only
/// after both pass, so a refusal can never reach a network call or create
/// any file.
fn cli(args: impl IntoIterator<Item = OsString>, run: impl FnOnce(RunConfig) -> u8) -> u8 {
    let config = match parse_args(args) {
        Ok(Command::Run(config)) => config,
        Err(error) => {
            eprintln!("phase0-live-proof: {error}\n{USAGE}");
            return EXIT_USAGE;
        }
    };
    if let Err(refusal) = config.preflight() {
        eprintln!("phase0-live-proof: refused: {refusal}");
        return match refusal {
            Refusal::ApprovalFormat => EXIT_NO_PERMISSION,
            Refusal::EvidenceDirExists | Refusal::EvidenceParentMissing => EXIT_CANT_CREATE,
        };
    }
    run(config)
}

fn exit_code(result: Result<Verdict, ManifestError>) -> u8 {
    match result {
        Ok(Verdict::Go) => EXIT_OK,
        Ok(Verdict::NoGo { failed }) => {
            eprintln!("phase0-live-proof: NO-GO at stage {}", failed.as_str());
            EXIT_NO_GO
        }
        Err(error) => {
            eprintln!("phase0-live-proof: {error}");
            EXIT_CANT_CREATE
        }
    }
}

async fn execute<E: StageExecutor, S: RowSink>(executor: &mut E, sink: &mut S) -> u8 {
    exit_code(run_sequence(executor, sink).await)
}

/// Validates the Steam token file locally first (a malformed file never
/// reaches the irreversible FCM enrollment; token acceptance is proven only
/// by Facepunch later), then creates the evidence directory and runs the
/// stages.
async fn run_gated<E: StageExecutor>(
    token_file: PathBuf,
    evidence_dir: &Path,
    executor: &mut E,
) -> u8 {
    if let Err(failure) = validate_token_file(token_file).await {
        eprintln!(
            "phase0-live-proof: Steam token file unusable: {}",
            failure.detail
        );
        return EXIT_NO_INPUT;
    }
    let mut writer = match ManifestWriter::create(evidence_dir) {
        Ok(writer) => writer,
        Err(error) => {
            eprintln!("phase0-live-proof: {error}");
            return EXIT_CANT_CREATE;
        }
    };
    execute(executor, &mut writer).await
}

fn run_live(config: RunConfig) -> u8 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("phase0-live-proof: async runtime could not start");
            return EXIT_CANT_CREATE;
        }
    };
    let code = runtime.block_on(async move {
        let cancel = CancellationToken::new();
        let interrupt = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    cancel.cancel();
                }
            })
        };
        let token_file = config.steam_token_file.clone();
        let evidence_dir = config.evidence_dir.clone();
        let mut executor = LiveExecutor::new(config, cancel);
        let code = run_gated(token_file, &evidence_dir, &mut executor).await;
        interrupt.abort();
        code
    });
    // A timed-out stdin read leaves a blocking thread parked on stdin;
    // do not let it hold the process open once the run is finished.
    runtime.shutdown_timeout(Duration::from_secs(2));
    code
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::{
        manifest::{ManifestRow, Route, Stage, Status, StatusClass},
        sequence::StageOutcome,
    };

    const VALID_APPROVAL: &str = "phase0-23-0123abcd";

    fn argv(evidence_dir: &str, approval: &str) -> Vec<OsString> {
        [
            "run",
            "--evidence-dir",
            evidence_dir,
            "--fcm-sender-id",
            "1",
            "--steam-id",
            "1",
            "--steam-token-file",
            "/nonexistent",
            "--approved-run-id",
            approval,
        ]
        .iter()
        .map(OsString::from)
        .collect()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("p23-main-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    /// Runs `cli` with a runner that records whether it was reached.
    fn cli_with_probe(args: Vec<OsString>) -> (u8, bool) {
        let reached = Cell::new(false);
        let code = cli(args, |_| {
            reached.set(true);
            42
        });
        (code, reached.get())
    }

    #[test]
    fn usage_errors_exit_64_before_the_runner() {
        for args in [
            Vec::new(),
            vec![OsString::from("probe")],
            vec![OsString::from("run"), OsString::from("--bogus")],
            vec![OsString::from("run")],
        ] {
            assert_eq!(cli_with_probe(args), (64, false));
        }
        let mut duplicate = argv("/tmp/p23-main-never", VALID_APPROVAL);
        duplicate.extend(["--steam-id", "2"].map(OsString::from));
        assert_eq!(cli_with_probe(duplicate), (64, false));
    }

    #[test]
    fn approval_refusal_exits_77_before_the_runner_and_creates_nothing() {
        let dir = scratch("approval");
        let evidence = dir.join("evidence");
        let evidence_text = evidence.to_str().expect("utf8").to_owned();
        for approval in ["wrong", "phase0-23-0123ABCD", "phase0-23-"] {
            assert_eq!(cli_with_probe(argv(&evidence_text, approval)), (77, false));
        }
        assert!(!evidence.exists());
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn existing_or_unreachable_evidence_dir_exits_73_before_the_runner() {
        let dir = scratch("evidence");
        let existing = dir.to_str().expect("utf8").to_owned();
        assert_eq!(cli_with_probe(argv(&existing, VALID_APPROVAL)), (73, false));
        let orphan = dir.join("missing-parent").join("evidence");
        let orphan = orphan.to_str().expect("utf8").to_owned();
        assert_eq!(cli_with_probe(argv(&orphan, VALID_APPROVAL)), (73, false));
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn approved_fresh_run_reaches_the_runner_and_returns_its_code() {
        let dir = scratch("fresh");
        let fresh = dir.join("evidence");
        let fresh_text = fresh.to_str().expect("utf8").to_owned();
        assert_eq!(
            cli_with_probe(argv(&fresh_text, VALID_APPROVAL)),
            (42, true)
        );
        assert!(!fresh.exists(), "only the runner may create the directory");
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    struct Fake {
        failing: Option<Stage>,
        finished: bool,
    }

    impl StageExecutor for Fake {
        async fn execute(&mut self, stage: Stage) -> StageOutcome {
            if self.failing == Some(stage) {
                StageOutcome::fail(Route::Local, StatusClass::Timeout, "fake_failure")
            } else {
                StageOutcome::pass(Route::Local, "fake_pass")
            }
        }

        async fn finish(&mut self) {
            self.finished = true;
        }
    }

    #[tokio::test]
    async fn blocking_failure_exits_70_with_no_go_row_and_cleanup() {
        let mut executor = Fake {
            failing: Some(Stage::EntityInfo),
            finished: false,
        };
        let mut rows: Vec<ManifestRow> = Vec::new();
        assert_eq!(execute(&mut executor, &mut rows).await, 70);
        let tail: Vec<_> = rows
            .iter()
            .rev()
            .take(2)
            .map(|row| (row.stage, row.status))
            .collect();
        assert_eq!(
            tail,
            [(Stage::Teardown, Status::Pass), (Stage::NoGo, Status::Fail)]
        );
        assert!(executor.finished);
    }

    #[tokio::test]
    async fn full_pass_exits_0() {
        let mut executor = Fake {
            failing: None,
            finished: false,
        };
        let mut rows: Vec<ManifestRow> = Vec::new();
        assert_eq!(execute(&mut executor, &mut rows).await, 0);
        assert!(rows.iter().all(|row| row.stage != Stage::NoGo));
    }

    #[tokio::test]
    async fn unusable_token_file_stops_before_any_stage_or_file() {
        let dir = scratch("gate-token");
        let evidence = dir.join("evidence");
        let mut executor = Fake {
            failing: None,
            finished: false,
        };
        let code = run_gated(dir.join("absent-token"), &evidence, &mut executor).await;
        assert_eq!(code, 66);
        assert!(!executor.finished, "no stage, and so no enrollment, ran");
        assert!(!evidence.exists(), "no evidence directory was created");
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[tokio::test]
    async fn usable_token_file_lets_the_run_proceed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("gate-ok");
        let token = dir.join("token");
        std::fs::write(&token, "steam-token-value\n").expect("write");
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let evidence = dir.join("evidence");
        let mut executor = Fake {
            failing: None,
            finished: false,
        };
        assert_eq!(run_gated(token, &evidence, &mut executor).await, 0);
        assert!(executor.finished);
        assert!(evidence.join("manifest.jsonl").is_file());
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[tokio::test]
    async fn product_route_failure_is_not_go_and_exits_70() {
        let mut executor = Fake {
            failing: Some(Stage::SessionProductRoute),
            finished: false,
        };
        let mut rows: Vec<ManifestRow> = Vec::new();
        assert_eq!(execute(&mut executor, &mut rows).await, 70);
        assert_eq!(rows.last().map(|row| row.stage), Some(Stage::NoGo));
        assert_eq!(rows.len(), Stage::SEQUENCE.len() + 1);
    }

    #[test]
    fn manifest_failure_maps_to_73() {
        assert_eq!(exit_code(Err(ManifestError::Write)), 73);
        assert_eq!(exit_code(Ok(Verdict::Go)), 0);
        assert_eq!(
            exit_code(Ok(Verdict::NoGo {
                failed: Stage::Teardown
            })),
            70
        );
    }
}
