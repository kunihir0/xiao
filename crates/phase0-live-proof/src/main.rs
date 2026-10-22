//! Isolated Rust+ live-proof harness for issue #23.
//!
//! `run` talks to real FCM, Expo, Facepunch, MCS, and Rust+ servers, but only
//! after a human at an interactive terminal types back a fresh one-time code
//! and the evidence directory does not exist yet. Everything before that
//! confirmation is offline and creates no files.

mod args;
mod classify;
mod confirm;
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
    confirm::{ConfirmationIo, StdTerminal, confirm_interruptible},
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
/// The interactive one-time confirmation was refused.
const EXIT_CONFIRMATION_REFUSED: u8 = 77;

fn main() -> ExitCode {
    ExitCode::from(cli(env::args_os().skip(1), run_live))
}

/// Parses arguments and applies the offline evidence-directory check. `run`
/// is invoked only after both pass; a refusal can never reach a network call
/// or create any file.
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
/// by Facepunch later), then requires the human's one-time confirmation, and
/// only then creates the evidence directory and runs the stages. A refusal
/// creates nothing and never reaches the executor.
async fn run_gated<E: StageExecutor, C: ConfirmationIo + Send + 'static>(
    token_file: PathBuf,
    evidence_dir: &Path,
    executor: &mut E,
    confirmation: C,
    cancel: &CancellationToken,
) -> u8 {
    if let Err(failure) = validate_token_file(token_file).await {
        eprintln!(
            "phase0-live-proof: Steam token file unusable: {}",
            failure.detail
        );
        return EXIT_NO_INPUT;
    }
    if confirm_interruptible(confirmation, cancel).await.is_err() {
        eprintln!(
            "phase0-live-proof: confirmation refused; nothing was started \
(an interactive terminal and the one-time code are required)"
        );
        return EXIT_CONFIRMATION_REFUSED;
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
        let mut executor = LiveExecutor::new(config, cancel.clone());
        let code = run_gated(
            token_file,
            &evidence_dir,
            &mut executor,
            StdTerminal::new(),
            &cancel,
        )
        .await;
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
    use std::{
        cell::Cell,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    use zeroize::Zeroizing;

    use super::*;
    use crate::{
        confirm::LineFailure,
        manifest::{ManifestRow, Route, Stage, Status, StatusClass},
        sequence::StageOutcome,
    };

    fn argv(evidence_dir: &str) -> Vec<OsString> {
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
        let mut duplicate = argv("/tmp/p23-main-never");
        duplicate.extend(["--steam-id", "2"].map(OsString::from));
        assert_eq!(cli_with_probe(duplicate), (64, false));
    }

    #[test]
    fn retired_approval_flag_is_a_usage_error() {
        let mut args = argv("/tmp/p23-main-never");
        args.extend(["--approved-run-id", "phase0-23-0123abcd"].map(OsString::from));
        assert_eq!(cli_with_probe(args), (64, false));
    }

    #[test]
    fn existing_or_unreachable_evidence_dir_exits_73_before_the_runner() {
        let dir = scratch("evidence");
        let existing = dir.to_str().expect("utf8").to_owned();
        assert_eq!(cli_with_probe(argv(&existing)), (73, false));
        let orphan = dir.join("missing-parent").join("evidence");
        let orphan = orphan.to_str().expect("utf8").to_owned();
        assert_eq!(cli_with_probe(argv(&orphan)), (73, false));
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn fresh_run_reaches_the_runner_and_returns_its_code() {
        let dir = scratch("fresh");
        let fresh = dir.join("evidence");
        let fresh_text = fresh.to_str().expect("utf8").to_owned();
        assert_eq!(cli_with_probe(argv(&fresh_text)), (42, true));
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

    /// Scripted terminal that records whether it was ever consulted.
    struct Typist {
        terminal: bool,
        reply: Result<&'static str, LineFailure>,
        consulted: Arc<AtomicBool>,
    }

    impl Typist {
        fn new(
            terminal: bool,
            reply: Result<&'static str, LineFailure>,
        ) -> (Self, Arc<AtomicBool>) {
            let consulted = Arc::new(AtomicBool::new(false));
            let typist = Self {
                terminal,
                reply,
                consulted: Arc::clone(&consulted),
            };
            (typist, consulted)
        }
    }

    impl ConfirmationIo for Typist {
        fn is_interactive_terminal(&self) -> bool {
            self.consulted.store(true, Ordering::SeqCst);
            self.terminal
        }

        fn new_code(&mut self) -> Zeroizing<String> {
            Zeroizing::new("ABCD2345".to_owned())
        }

        fn show_prompt(&mut self, _text: &str) -> std::io::Result<()> {
            Ok(())
        }

        fn read_line(&mut self, _timeout: Duration) -> Result<Zeroizing<String>, LineFailure> {
            self.reply.map(|line| Zeroizing::new(line.to_owned()))
        }
    }

    fn write_token(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let token = dir.join("token");
        std::fs::write(&token, "steam-token-value\n").expect("write");
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        token
    }

    fn fake() -> Fake {
        Fake {
            failing: None,
            finished: false,
        }
    }

    #[tokio::test]
    async fn unusable_token_file_stops_before_confirmation_any_stage_or_file() {
        let dir = scratch("gate-token");
        let evidence = dir.join("evidence");
        let mut executor = fake();
        let (typist, consulted) = Typist::new(true, Ok("ABCD2345\n"));
        let cancel = CancellationToken::new();
        let code = run_gated(
            dir.join("absent-token"),
            &evidence,
            &mut executor,
            typist,
            &cancel,
        )
        .await;
        assert_eq!(code, 66);
        assert!(!consulted.load(Ordering::SeqCst), "no prompt was shown");
        assert!(!executor.finished, "no stage, and so no enrollment, ran");
        assert!(!evidence.exists(), "no evidence directory was created");
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[tokio::test]
    async fn every_confirmation_refusal_exits_77_and_creates_and_runs_nothing() {
        let refusals = [
            (false, Ok("ABCD2345\n")),
            (true, Ok("WRONG234\n")),
            (true, Ok("abcd2345\n")),
            (true, Ok("ABCD2345")),
            (true, Err(LineFailure::EndOfInput)),
            (true, Err(LineFailure::Unterminated)),
            (true, Err(LineFailure::TimedOut)),
            (true, Err(LineFailure::Unreadable)),
        ];
        for (terminal, reply) in refusals {
            let dir = scratch("gate-refused");
            let token = write_token(&dir);
            let evidence = dir.join("evidence");
            let mut executor = fake();
            let (typist, _) = Typist::new(terminal, reply);
            let cancel = CancellationToken::new();
            let code = run_gated(token, &evidence, &mut executor, typist, &cancel).await;
            assert_eq!(code, 77, "{terminal} {reply:?}");
            assert!(!executor.finished, "the executor was never reached");
            assert!(!evidence.exists(), "no evidence directory was created");
            std::fs::remove_dir_all(dir).expect("cleanup");
        }
    }

    #[tokio::test]
    async fn interrupt_during_confirmation_exits_77_and_creates_nothing() {
        let dir = scratch("gate-interrupt");
        let token = write_token(&dir);
        let evidence = dir.join("evidence");
        let mut executor = fake();
        let (typist, _) = Typist::new(true, Ok("ABCD2345\n"));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let code = run_gated(token, &evidence, &mut executor, typist, &cancel).await;
        assert_eq!(code, 77);
        assert!(!executor.finished);
        assert!(!evidence.exists());
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[tokio::test]
    async fn confirmed_run_proceeds() {
        let dir = scratch("gate-ok");
        let token = write_token(&dir);
        let evidence = dir.join("evidence");
        let mut executor = fake();
        let (typist, _) = Typist::new(true, Ok("ABCD2345\n"));
        let cancel = CancellationToken::new();
        let code = run_gated(token, &evidence, &mut executor, typist, &cancel).await;
        assert_eq!(code, 0);
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
