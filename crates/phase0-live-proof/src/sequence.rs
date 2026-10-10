//! Stage sequencing: ordering, one manifest row per stage, and the NO-GO stop.
//!
//! The sequencer is independent of any network code; the live executor and
//! the test fakes both implement [`StageExecutor`].

use std::time::{Instant, SystemTime};

use crate::manifest::{
    Correlation, DELIVERY_CONTRACT, ManifestError, ManifestRow, Route, RowSink, Stage, Status,
    StatusClass, correlation_hash, format_utc,
};

/// Result of one stage. Strings are `'static` so nothing dynamic can leak.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StageOutcome {
    pub route: Route,
    pub status: Status,
    pub status_class: StatusClass,
    pub detail: &'static str,
    pub correlation: Correlation,
    /// Marks receipt rows, which carry `delivery_contract`.
    pub receipt: bool,
}

impl StageOutcome {
    #[must_use]
    pub const fn pass(route: Route, detail: &'static str) -> Self {
        Self {
            route,
            status: Status::Pass,
            status_class: StatusClass::Ok,
            detail,
            correlation: Correlation::NONE,
            receipt: false,
        }
    }

    #[must_use]
    pub const fn fail(route: Route, status_class: StatusClass, detail: &'static str) -> Self {
        Self {
            route,
            status: Status::Fail,
            status_class,
            detail,
            correlation: Correlation::NONE,
            receipt: false,
        }
    }

    #[must_use]
    pub const fn not_run(route: Route, detail: &'static str) -> Self {
        Self {
            route,
            status: Status::NotRun,
            status_class: StatusClass::NotApplicable,
            detail,
            correlation: Correlation::NONE,
            receipt: false,
        }
    }

    #[must_use]
    pub const fn unsupported(route: Route, detail: &'static str) -> Self {
        Self {
            route,
            status: Status::Unsupported,
            status_class: StatusClass::Unsupported,
            detail,
            correlation: Correlation::NONE,
            receipt: false,
        }
    }

    #[must_use]
    pub const fn with_correlation(mut self, correlation: Correlation) -> Self {
        self.correlation = correlation;
        self
    }

    #[must_use]
    pub const fn with_status_class(mut self, status_class: StatusClass) -> Self {
        self.status_class = status_class;
        self
    }

    #[must_use]
    pub const fn as_receipt(mut self) -> Self {
        self.receipt = true;
        self
    }
}

/// Executes individual stages. `finish` always runs exactly once after the
/// last row so secrets and owned tasks are released on both GO and NO-GO.
pub trait StageExecutor {
    async fn execute(&mut self, stage: Stage) -> StageOutcome;
    async fn finish(&mut self);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    Go,
    NoGo { failed: Stage },
}

/// Whether a failed stage ends the run. A failed product route only records
/// the finding; the ws comparison stage then decides whether any route
/// exists.
#[must_use]
pub fn is_blocking_failure(stage: Stage, status: Status) -> bool {
    status == Status::Fail && stage != Stage::SessionProductRoute
}

/// Runs every stage in order, writing exactly one row per stage and stopping
/// at the first blocking failure with a `no-go` row.
pub async fn run_sequence<E: StageExecutor, S: RowSink>(
    executor: &mut E,
    sink: &mut S,
) -> Result<Verdict, ManifestError> {
    let result = drive(executor, sink).await;
    executor.finish().await;
    result
}

async fn drive<E: StageExecutor, S: RowSink>(
    executor: &mut E,
    sink: &mut S,
) -> Result<Verdict, ManifestError> {
    let run_start = Instant::now();
    let mut product_route_passed = false;
    for (index, stage) in Stage::SEQUENCE.into_iter().enumerate() {
        let started_at = SystemTime::now();
        let outcome = if stage == Stage::SessionWsCompare && product_route_passed {
            StageOutcome::not_run(Route::RustplusWsDirectNonproduct, "product_route_passed")
        } else {
            tracing::debug!(stage = stage.as_str(), "stage start");
            executor.execute(stage).await
        };
        let ended_at = SystemTime::now();
        tracing::debug!(stage = stage.as_str(), status = ?outcome.status, "stage end");
        sink.append(&row_for(stage, &outcome, started_at, ended_at, run_start))?;
        if stage == Stage::SessionProductRoute {
            product_route_passed = outcome.status == Status::Pass;
        }
        if is_blocking_failure(stage, outcome.status) {
            let now = SystemTime::now();
            let marker = StageOutcome::fail(
                Route::None,
                StatusClass::RequiredStageFailed,
                "required_stage_failed",
            )
            .with_correlation(Correlation {
                entity_id: 0,
                value: stage.as_str(),
                event_seq: index as u64,
            });
            sink.append(&row_for(Stage::NoGo, &marker, now, now, run_start))?;
            return Ok(Verdict::NoGo { failed: stage });
        }
    }
    Ok(Verdict::Go)
}

fn row_for(
    stage: Stage,
    outcome: &StageOutcome,
    started_at: SystemTime,
    ended_at: SystemTime,
    run_start: Instant,
) -> ManifestRow {
    ManifestRow {
        stage,
        route: outcome.route,
        status: outcome.status,
        started_at_utc: format_utc(started_at),
        ended_at_utc: format_utc(ended_at),
        monotonic_ms: u64::try_from(run_start.elapsed().as_millis()).unwrap_or(u64::MAX),
        correlation_hash: correlation_hash(
            stage.as_str(),
            outcome.correlation.entity_id,
            outcome.correlation.value,
            outcome.correlation.event_seq,
        ),
        status_class: outcome.status_class,
        detail: outcome.detail,
        delivery_contract: outcome.receipt.then_some(DELIVERY_CONTRACT),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    struct Scripted {
        outcomes: HashMap<&'static str, StageOutcome>,
        executed: Vec<Stage>,
        finished: u32,
    }

    impl Scripted {
        fn passing() -> Self {
            Self {
                outcomes: HashMap::new(),
                executed: Vec::new(),
                finished: 0,
            }
        }

        fn with(mut self, stage: Stage, outcome: StageOutcome) -> Self {
            self.outcomes.insert(stage.as_str(), outcome);
            self
        }
    }

    impl StageExecutor for Scripted {
        async fn execute(&mut self, stage: Stage) -> StageOutcome {
            self.executed.push(stage);
            self.outcomes
                .get(stage.as_str())
                .copied()
                .unwrap_or_else(|| StageOutcome::pass(Route::Local, "scripted_pass"))
        }

        async fn finish(&mut self) {
            self.finished += 1;
        }
    }

    fn stages(rows: &[ManifestRow]) -> Vec<Stage> {
        rows.iter().map(|row| row.stage).collect()
    }

    #[tokio::test]
    async fn all_stages_run_in_order_with_one_row_each() {
        let mut executor = Scripted::passing();
        let mut rows = Vec::new();
        let verdict = run_sequence(&mut executor, &mut rows).await.expect("run");
        assert_eq!(verdict, Verdict::Go);
        assert_eq!(stages(&rows), Stage::SEQUENCE);
        // Stage 6 is not run because the product route passed.
        let mut expected = Stage::SEQUENCE.to_vec();
        expected.retain(|stage| *stage != Stage::SessionWsCompare);
        assert_eq!(executor.executed, expected);
        assert_eq!(rows[5].status, Status::NotRun);
        assert_eq!(rows[5].route, Route::RustplusWsDirectNonproduct);
        assert_eq!(executor.finished, 1);
    }

    #[tokio::test]
    async fn first_required_failure_stops_with_no_go_row() {
        let mut executor = Scripted::passing().with(
            Stage::Pairing,
            StageOutcome::fail(Route::McsPush, StatusClass::Timeout, "pairing_timeout"),
        );
        let mut rows = Vec::new();
        let verdict = run_sequence(&mut executor, &mut rows).await.expect("run");
        assert_eq!(
            verdict,
            Verdict::NoGo {
                failed: Stage::Pairing
            }
        );
        assert_eq!(
            stages(&rows),
            [
                Stage::EnrollFcm,
                Stage::EnrollCompanion,
                Stage::McsReady,
                Stage::Pairing,
                Stage::NoGo
            ]
        );
        assert_eq!(executor.executed.last(), Some(&Stage::Pairing));
        let no_go = rows.last().expect("no-go row");
        assert_eq!(no_go.status, Status::Fail);
        assert_eq!(no_go.status_class, StatusClass::RequiredStageFailed);
        assert_eq!(
            no_go.correlation_hash,
            correlation_hash("no-go", 0, "pairing", 3)
        );
        assert_eq!(executor.finished, 1, "cleanup runs on NO-GO");
    }

    #[tokio::test]
    async fn failed_product_route_is_recorded_and_ws_comparison_runs() {
        let mut executor = Scripted::passing()
            .with(
                Stage::SessionProductRoute,
                StageOutcome::fail(
                    Route::RustplusWssProduct,
                    StatusClass::Transport,
                    "wss_connect_failed",
                ),
            )
            .with(
                Stage::SessionWsCompare,
                StageOutcome::pass(Route::RustplusWsDirectNonproduct, "ws_ready"),
            );
        let mut rows = Vec::new();
        let verdict = run_sequence(&mut executor, &mut rows).await.expect("run");
        assert_eq!(verdict, Verdict::Go);
        assert_eq!(rows[4].status, Status::Fail);
        assert_eq!(rows[5].status, Status::Pass);
        assert!(executor.executed.contains(&Stage::SessionWsCompare));
    }

    #[tokio::test]
    async fn both_routes_failing_is_no_go_at_the_ws_comparison() {
        let mut executor = Scripted::passing()
            .with(
                Stage::SessionProductRoute,
                StageOutcome::fail(Route::RustplusWssProduct, StatusClass::Transport, "x"),
            )
            .with(
                Stage::SessionWsCompare,
                StageOutcome::fail(
                    Route::RustplusWsDirectNonproduct,
                    StatusClass::InvalidEndpoint,
                    "y",
                ),
            );
        let mut rows = Vec::new();
        let verdict = run_sequence(&mut executor, &mut rows).await.expect("run");
        assert_eq!(
            verdict,
            Verdict::NoGo {
                failed: Stage::SessionWsCompare
            }
        );
        assert_eq!(rows.last().map(|row| row.stage), Some(Stage::NoGo));
    }

    #[tokio::test]
    async fn non_fail_statuses_never_stop_the_run() {
        let mut executor = Scripted::passing()
            .with(
                Stage::NegativeAuth,
                StageOutcome::not_run(Route::None, "expiry_not_run"),
            )
            .with(
                Stage::ModernFisCompare,
                StageOutcome::unsupported(Route::None, "modern_fis_not_implemented"),
            );
        let mut rows = Vec::new();
        let verdict = run_sequence(&mut executor, &mut rows).await.expect("run");
        assert_eq!(verdict, Verdict::Go);
        assert_eq!(rows.len(), Stage::SEQUENCE.len());
        assert_eq!(rows[12].status, Status::Unsupported);
    }

    #[tokio::test]
    async fn only_receipt_rows_carry_the_delivery_contract() {
        let mut executor = Scripted::passing().with(
            Stage::AlarmTrue,
            StageOutcome::pass(Route::RustplusWssProduct, "entity_changed_observed")
                .as_receipt()
                .with_correlation(Correlation {
                    entity_id: 9,
                    value: "true",
                    event_seq: 1,
                }),
        );
        let mut rows = Vec::new();
        run_sequence(&mut executor, &mut rows).await.expect("run");
        for row in &rows {
            assert_eq!(
                row.delivery_contract.is_some(),
                row.stage == Stage::AlarmTrue,
                "{:?}",
                row.stage
            );
        }
        let alarm = rows
            .iter()
            .find(|row| row.stage == Stage::AlarmTrue)
            .expect("row");
        assert_eq!(alarm.delivery_contract, Some("at-least-once-unproven"));
        assert_eq!(
            alarm.correlation_hash,
            correlation_hash("alarm-true", 9, "true", 1)
        );
    }

    struct FailingSink;
    impl RowSink for FailingSink {
        fn append(&mut self, _row: &ManifestRow) -> Result<(), ManifestError> {
            Err(ManifestError::Write)
        }
    }

    #[tokio::test]
    async fn manifest_write_failure_still_runs_cleanup() {
        let mut executor = Scripted::passing();
        let result = run_sequence(&mut executor, &mut FailingSink).await;
        assert!(matches!(result, Err(ManifestError::Write)));
        assert_eq!(executor.executed, [Stage::EnrollFcm]);
        assert_eq!(executor.finished, 1);
    }
}
