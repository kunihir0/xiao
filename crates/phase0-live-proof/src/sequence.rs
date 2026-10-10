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

/// Whether a failed stage ends the run immediately. A failed product route
/// does not: the ws comparison then still gathers evidence and decides
/// whether any route exists. It is nevertheless a required stage, so the
/// run can no longer be GO ([`run_sequence`] records it as NO-GO at the end).
#[must_use]
pub fn is_blocking_failure(stage: Stage, status: Status) -> bool {
    status == Status::Fail && stage != Stage::SessionProductRoute
}

/// Runs every stage in order, writing exactly one row per stage and stopping
/// at the first blocking failure with a `no-go` row. The verdict is GO only
/// if every required stage passed: a failed `session-product-route` lets the
/// run continue for evidence but ends it with a `no-go` row and
/// `Verdict::NoGo`. After an early stop the `no-go` row is written first,
/// then a best-effort `teardown` row, so the NO-GO evidence never waits on
/// cleanup or the operator.
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
    let mut deferred: Option<(Stage, usize)> = None;
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
            if !product_route_passed {
                deferred = Some((stage, index));
            }
        }
        if is_blocking_failure(stage, outcome.status) {
            write_no_go(sink, stage, index, run_start)?;
            if stage != Stage::Teardown {
                // The NO-GO row is already durable; teardown is best effort
                // and its status never changes the verdict.
                record_teardown(executor, sink, run_start).await?;
            }
            return Ok(Verdict::NoGo { failed: stage });
        }
    }
    if let Some((stage, index)) = deferred {
        write_no_go(sink, stage, index, run_start)?;
        return Ok(Verdict::NoGo { failed: stage });
    }
    Ok(Verdict::Go)
}

/// Runs the teardown stage after an early stop so every outcome, GO or
/// NO-GO, ends with exactly one `teardown` row.
async fn record_teardown<E: StageExecutor, S: RowSink>(
    executor: &mut E,
    sink: &mut S,
    run_start: Instant,
) -> Result<(), ManifestError> {
    let started_at = SystemTime::now();
    tracing::debug!(stage = Stage::Teardown.as_str(), "stage start");
    let outcome = executor.execute(Stage::Teardown).await;
    let ended_at = SystemTime::now();
    tracing::debug!(stage = Stage::Teardown.as_str(), status = ?outcome.status, "stage end");
    sink.append(&row_for(
        Stage::Teardown,
        &outcome,
        started_at,
        ended_at,
        run_start,
    ))
}

fn write_no_go<S: RowSink>(
    sink: &mut S,
    failed: Stage,
    index: usize,
    run_start: Instant,
) -> Result<(), ManifestError> {
    let now = SystemTime::now();
    let marker = StageOutcome::fail(
        Route::None,
        StatusClass::RequiredStageFailed,
        "required_stage_failed",
    )
    .with_correlation(Correlation {
        entity_id: 0,
        value: failed.as_str(),
        event_seq: index as u64,
    });
    sink.append(&row_for(Stage::NoGo, &marker, now, now, run_start))
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

    #[test]
    fn stage_ids_are_pinned_to_the_specified_order() {
        let ids: Vec<&str> = Stage::SEQUENCE.iter().map(|stage| stage.as_str()).collect();
        assert_eq!(
            ids,
            [
                "enroll-fcm",
                "enroll-companion",
                "mcs-ready",
                "pairing",
                "session-product-route",
                "session-ws-compare",
                "entity-info",
                "alarm-true",
                "alarm-false",
                "independent-receipt",
                "restart-reconnect",
                "negative-auth",
                "modern-fis-compare",
                "teardown",
            ]
        );
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
                Stage::NoGo,
                Stage::Teardown
            ]
        );
        assert_eq!(
            executor.executed,
            [
                Stage::EnrollFcm,
                Stage::EnrollCompanion,
                Stage::McsReady,
                Stage::Pairing,
                Stage::Teardown
            ]
        );
        let no_go = &rows[4];
        assert_eq!(no_go.stage, Stage::NoGo);
        assert_eq!(no_go.status, Status::Fail);
        assert_eq!(no_go.status_class, StatusClass::RequiredStageFailed);
        assert_eq!(
            no_go.correlation_hash,
            correlation_hash("no-go", 0, "pairing", 3)
        );
        assert_eq!(executor.finished, 1, "cleanup runs on NO-GO");
    }

    fn teardown_rows(rows: &[ManifestRow]) -> Vec<&ManifestRow> {
        rows.iter()
            .filter(|row| row.stage == Stage::Teardown)
            .collect()
    }

    #[tokio::test]
    async fn teardown_row_is_recorded_after_go() {
        let mut executor = Scripted::passing();
        let mut rows = Vec::new();
        let verdict = run_sequence(&mut executor, &mut rows).await.expect("run");
        assert_eq!(verdict, Verdict::Go);
        let teardown = teardown_rows(&rows);
        assert_eq!(teardown.len(), 1);
        assert_eq!(teardown[0].status, Status::Pass);
        assert_eq!(rows.last().map(|row| row.stage), Some(Stage::Teardown));
    }

    #[tokio::test]
    async fn teardown_row_is_recorded_after_no_go_at_stage_3() {
        let mut executor = Scripted::passing().with(
            Stage::McsReady,
            StageOutcome::fail(Route::McsPush, StatusClass::Timeout, "mcs_ready_timeout"),
        );
        let mut rows = Vec::new();
        let verdict = run_sequence(&mut executor, &mut rows).await.expect("run");
        assert_eq!(
            verdict,
            Verdict::NoGo {
                failed: Stage::McsReady
            }
        );
        assert_eq!(
            stages(&rows),
            [
                Stage::EnrollFcm,
                Stage::EnrollCompanion,
                Stage::McsReady,
                Stage::NoGo,
                Stage::Teardown
            ]
        );
        assert_eq!(executor.executed.last(), Some(&Stage::Teardown));
        assert_eq!(executor.finished, 1);
    }

    #[tokio::test]
    async fn teardown_row_is_recorded_after_no_go_at_stage_6() {
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
        assert_eq!(teardown_rows(&rows).len(), 1);
        assert_eq!(
            stages(&rows),
            [
                Stage::EnrollFcm,
                Stage::EnrollCompanion,
                Stage::McsReady,
                Stage::Pairing,
                Stage::SessionProductRoute,
                Stage::SessionWsCompare,
                Stage::NoGo,
                Stage::Teardown
            ]
        );
    }

    #[tokio::test]
    async fn failed_teardown_after_no_go_keeps_verdict_and_records_failure() {
        let mut executor = Scripted::passing()
            .with(
                Stage::Pairing,
                StageOutcome::fail(Route::McsPush, StatusClass::Timeout, "pairing_timeout"),
            )
            .with(
                Stage::Teardown,
                StageOutcome::fail(
                    Route::Local,
                    StatusClass::HumanResetUnconfirmed,
                    "human_reset_unconfirmed",
                ),
            );
        let mut rows = Vec::new();
        let verdict = run_sequence(&mut executor, &mut rows).await.expect("run");
        assert_eq!(
            verdict,
            Verdict::NoGo {
                failed: Stage::Pairing
            }
        );
        let teardown = teardown_rows(&rows);
        assert_eq!(teardown.len(), 1);
        assert_eq!(teardown[0].status, Status::Fail);
        assert_eq!(teardown[0].status_class, StatusClass::HumanResetUnconfirmed);
        assert_eq!(rows.iter().filter(|r| r.stage == Stage::NoGo).count(), 1);
    }

    #[tokio::test]
    async fn failed_teardown_after_all_stages_passed_is_no_go_without_second_teardown() {
        let mut executor = Scripted::passing().with(
            Stage::Teardown,
            StageOutcome::fail(
                Route::Local,
                StatusClass::HumanResetUnconfirmed,
                "human_reset_unconfirmed",
            ),
        );
        let mut rows = Vec::new();
        let verdict = run_sequence(&mut executor, &mut rows).await.expect("run");
        assert_eq!(
            verdict,
            Verdict::NoGo {
                failed: Stage::Teardown
            }
        );
        assert_eq!(teardown_rows(&rows).len(), 1);
        assert_eq!(rows.last().map(|row| row.stage), Some(Stage::NoGo));
        assert_eq!(
            executor
                .executed
                .iter()
                .filter(|stage| **stage == Stage::Teardown)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn failed_product_route_continues_for_evidence_but_is_never_go() {
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
        // Required product route failed: evidence is still gathered, but the
        // verdict is never GO.
        assert_eq!(
            verdict,
            Verdict::NoGo {
                failed: Stage::SessionProductRoute
            }
        );
        assert_eq!(rows[4].status, Status::Fail);
        assert_eq!(rows[5].status, Status::Pass);
        assert!(executor.executed.contains(&Stage::SessionWsCompare));
        assert_eq!(rows.len(), Stage::SEQUENCE.len() + 1);
        let no_go = rows.last().expect("no-go row");
        assert_eq!(no_go.stage, Stage::NoGo);
        assert_eq!(
            no_go.correlation_hash,
            correlation_hash("no-go", 0, "session-product-route", 4)
        );
        assert_eq!(executor.finished, 1);
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
        assert_eq!(rows.last().map(|row| row.stage), Some(Stage::Teardown));
        assert_eq!(rows[rows.len() - 2].stage, Stage::NoGo);
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

    #[tokio::test]
    async fn correlation_hash_is_deterministic_within_a_run() {
        let a = correlation_hash("alarm-true", 9, "true", 1);
        assert_eq!(a, correlation_hash("alarm-true", 9, "true", 1));
        assert_ne!(a, correlation_hash("alarm-true", 9, "true", 2));
        assert_ne!(a, correlation_hash("alarm-false", 9, "true", 1));
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
