use std::{
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use bot_application::{BoxFuture, PairingRuntime, PushCredential, RuntimeError};
use bot_domain::SteamId;
use tokio::{sync::oneshot, time::sleep};
use tokio_util::sync::CancellationToken;

use crate::{CallEvent, EventLog};

#[derive(Default)]
struct Script {
    fail_start: bool,
    /// Result the next started futures end with immediately.
    end_immediately: Option<Result<(), RuntimeError>>,
    stop_delay: Duration,
    /// What a cancelled future ends with.
    stop_result: Option<Result<(), RuntimeError>>,
    /// Ends the most recently started future.
    end_latest: Option<oneshot::Sender<Result<(), RuntimeError>>>,
}

struct Shared {
    log: EventLog,
    script: Mutex<Script>,
    starts: AtomicUsize,
    alive: AtomicUsize,
}

/// Records `PairingRuntime::start` calls in an [`EventLog`] and returns
/// scripted futures. A future runs until its token is cancelled, then waits
/// the configured stop delay and finishes `Ok`; the test can instead end it
/// with a result at any time. Clones share one script.
#[derive(Clone)]
pub struct FakePairingRuntime {
    shared: Arc<Shared>,
}

impl FakePairingRuntime {
    pub fn new(log: &EventLog) -> Self {
        Self {
            shared: Arc::new(Shared {
                log: log.clone(),
                script: Mutex::new(Script::default()),
                starts: AtomicUsize::new(0),
                alive: AtomicUsize::new(0),
            }),
        }
    }

    fn script(&self) -> std::sync::MutexGuard<'_, Script> {
        self.shared
            .script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes `start` fail synchronously with `RuntimeError::Connector`.
    pub fn fail_start(&self, fail: bool) {
        self.script().fail_start = fail;
    }

    /// Futures started from now on end with `result` the first time they are
    /// polled, without waiting for cancellation.
    pub fn end_immediately(&self, result: Option<Result<(), RuntimeError>>) {
        self.script().end_immediately = result;
    }

    /// How long a cancelled future takes to finish. Use with paused time to
    /// make "the supervisor awaited the stop" observable.
    pub fn set_stop_delay(&self, delay: Duration) {
        self.script().stop_delay = delay;
    }

    /// Makes a cancelled future end with `result` instead of `Ok(())`.
    pub fn set_stop_result(&self, result: Result<(), RuntimeError>) {
        self.script().stop_result = Some(result);
    }

    /// Ends the most recently started future with `result`. Returns whether
    /// a future was still waiting to be ended.
    pub fn end_running(&self, result: Result<(), RuntimeError>) -> bool {
        match self.script().end_latest.take() {
            Some(sender) => sender.send(result).is_ok(),
            None => false,
        }
    }

    /// Number of successful `start` calls.
    pub fn starts(&self) -> usize {
        self.shared.starts.load(Ordering::SeqCst)
    }

    /// Number of started futures that have not been dropped yet.
    pub fn alive(&self) -> usize {
        self.shared.alive.load(Ordering::SeqCst)
    }
}

/// Logs the drop of a pairing future and releases its `alive` slot.
struct DropGuard {
    shared: Arc<Shared>,
    steam_id: SteamId,
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        self.shared.alive.fetch_sub(1, Ordering::SeqCst);
        self.shared
            .log
            .push(CallEvent::RuntimeDropped(self.steam_id));
    }
}

impl PairingRuntime for FakePairingRuntime {
    fn start(
        &self,
        credential: PushCredential,
        cancel: CancellationToken,
    ) -> Result<BoxFuture<'static, Result<(), RuntimeError>>, RuntimeError> {
        let (immediate, receiver) = {
            let mut script = self.script();
            if script.fail_start {
                return Err(RuntimeError::Connector);
            }
            let (sender, receiver) = oneshot::channel();
            script.end_latest = Some(sender);
            (script.end_immediately, receiver)
        };
        let steam_id = credential.steam_id;
        let shared = Arc::clone(&self.shared);
        shared.starts.fetch_add(1, Ordering::SeqCst);
        shared.alive.fetch_add(1, Ordering::SeqCst);
        shared.log.push(CallEvent::RuntimeStart(steam_id));
        Ok(Box::pin(async move {
            let _guard = DropGuard {
                shared: Arc::clone(&shared),
                steam_id,
            };
            if let Some(result) = immediate {
                return result;
            }
            let ended = tokio::select! {
                () = cancel.cancelled() => None,
                result = receiver => result.ok(),
            };
            match ended {
                Some(result) => result,
                // Cancelled (or the script was replaced and dropped, which
                // leaves the future running until cancelled).
                None => {
                    cancel.cancelled().await;
                    // Read when cancelled, so a test can script the stop after
                    // the future started.
                    let (stop_delay, stop_result) = {
                        let script = shared.script.lock().unwrap_or_else(PoisonError::into_inner);
                        (script.stop_delay, script.stop_result.unwrap_or(Ok(())))
                    };
                    shared.log.push(CallEvent::RuntimeCancelled(steam_id));
                    sleep(stop_delay).await;
                    shared.log.push(CallEvent::RuntimeStopped(steam_id));
                    stop_result
                }
            }
        }))
    }
}
