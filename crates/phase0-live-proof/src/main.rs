//! Isolated Rust+ live-proof harness for issue #23.
//!
//! `run` talks to real FCM, Expo, Facepunch, MCS, and Rust+ servers, but only
//! after the operator supplies a well-formed approval string and an evidence
//! directory that does not exist yet. Everything before that gate is offline.

mod args;
mod classify;
mod live;
mod manifest;
mod receipt;
mod sequence;

use std::{env, process::ExitCode};

use tokio_util::sync::CancellationToken;

use crate::{
    args::{Command, Refusal, RunConfig, USAGE, UsageError, parse_args},
    live::LiveExecutor,
    manifest::ManifestWriter,
    sequence::{Verdict, run_sequence},
};

const EXIT_NO_GO: u8 = 70;
const EXIT_USAGE: u8 = 64;
const EXIT_CANT_CREATE: u8 = 73;
const EXIT_NO_PERMISSION: u8 = 77;

fn main() -> ExitCode {
    let command = match parse_args(env::args_os().skip(1)) {
        Ok(command) => command,
        Err(error) => return usage_failure(error),
    };
    let Command::Run(config) = command;
    if let Err(refusal) = config.preflight() {
        eprintln!("phase0-live-proof: refused: {refusal}");
        return ExitCode::from(match refusal {
            Refusal::ApprovalFormat => EXIT_NO_PERMISSION,
            Refusal::EvidenceDirExists | Refusal::EvidenceParentMissing => EXIT_CANT_CREATE,
        });
    }
    ExitCode::from(run(config))
}

fn usage_failure(error: UsageError) -> ExitCode {
    eprintln!("phase0-live-proof: {error}\n{USAGE}");
    ExitCode::from(EXIT_USAGE)
}

fn run(config: RunConfig) -> u8 {
    let mut writer = match ManifestWriter::create(&config.evidence_dir) {
        Ok(writer) => writer,
        Err(error) => {
            eprintln!("phase0-live-proof: {error}");
            return EXIT_CANT_CREATE;
        }
    };
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
        let mut executor = LiveExecutor::new(config, cancel);
        let result = run_sequence(&mut executor, &mut writer).await;
        interrupt.abort();
        match result {
            Ok(Verdict::Go) => 0,
            Ok(Verdict::NoGo { failed }) => {
                eprintln!("phase0-live-proof: NO-GO at stage {}", failed.as_str());
                EXIT_NO_GO
            }
            Err(error) => {
                eprintln!("phase0-live-proof: {error}");
                EXIT_CANT_CREATE
            }
        }
    });
    drop(runtime);
    code
}
