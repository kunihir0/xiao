//! Owned process composition and shutdown lifecycle.

use std::{collections::HashMap, error::Error, fmt, future::Future, pin::Pin};

use tokio::{sync::oneshot, task::JoinSet};
use tokio_util::sync::CancellationToken;

/// Error type accepted at component boundaries.
pub type BoxError = Box<dyn Error + Send + Sync + 'static>;
type ComponentFuture = Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send + 'static>>;

/// A signal that requests process shutdown.
pub trait ShutdownSignal: Send + 'static {
    /// Waits for one shutdown request or reports a listener failure.
    fn wait(self) -> Pin<Box<dyn Future<Output = Result<(), SignalError>> + Send>>;
}

/// A production signal listener backed by Tokio's Ctrl-C handler.
#[derive(Debug, Default)]
pub struct CtrlCSignal;

impl ShutdownSignal for CtrlCSignal {
    fn wait(self) -> Pin<Box<dyn Future<Output = Result<(), SignalError>> + Send>> {
        Box::pin(async {
            tokio::signal::ctrl_c()
                .await
                .map_err(|error| SignalError::Failed(Box::new(error)))
        })
    }
}

/// A deterministic in-process shutdown signal for tests and embedding.
pub struct ChannelSignal {
    receiver: oneshot::Receiver<()>,
}

/// The sender half of a [`ChannelSignal`].
pub struct ShutdownTrigger {
    sender: Option<oneshot::Sender<()>>,
}

/// Creates a deterministic signal and its trigger.
pub fn channel_signal() -> (ShutdownTrigger, ChannelSignal) {
    let (sender, receiver) = oneshot::channel();
    (
        ShutdownTrigger {
            sender: Some(sender),
        },
        ChannelSignal { receiver },
    )
}

impl ShutdownTrigger {
    /// Requests shutdown. Repeated calls are reported as a closed signal.
    pub fn trigger(&mut self) -> Result<(), SignalError> {
        self.sender
            .take()
            .ok_or(SignalError::Closed)?
            .send(())
            .map_err(|_| SignalError::Closed)
    }
}

impl ShutdownSignal for ChannelSignal {
    fn wait(self) -> Pin<Box<dyn Future<Output = Result<(), SignalError>> + Send>> {
        Box::pin(async move { self.receiver.await.map_err(|_| SignalError::Closed) })
    }
}

/// Shutdown listener failures.
#[derive(Debug)]
pub enum SignalError {
    /// The sender or listener was closed before a signal arrived.
    Closed,
    /// The operating-system signal listener failed.
    Failed(BoxError),
}

impl fmt::Display for SignalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("shutdown signal closed"),
            Self::Failed(error) => write!(formatter, "shutdown signal failed: {error}"),
        }
    }
}

impl Error for SignalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Failed(error) => Some(error.as_ref()),
            Self::Closed => None,
        }
    }
}

/// Internal lifecycle readiness state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Readiness {
    /// Components have not started.
    Starting,
    /// All registered components have been spawned.
    Ready,
    /// Cancellation has been requested and joins are in progress.
    Stopping,
    /// Every owned component task has been joined.
    Stopped,
}

/// The result of a complete lifecycle run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunReport {
    /// Final internal readiness state.
    pub readiness: Readiness,
    /// Whether a shutdown signal (rather than natural task completion) ended the run.
    pub shutdown_requested: bool,
    /// Number of component tasks joined by the owner.
    pub joined_tasks: usize,
}

/// Composition and lifecycle failures.
#[derive(Debug)]
pub enum CompositionError {
    /// A component was registered after startup began.
    AlreadyStarted,
    /// The shutdown listener failed.
    Signal(SignalError),
    /// A component returned an error.
    Component { name: String, source: BoxError },
    /// Tokio could not join an owned task.
    TaskJoin { name: String, cancelled: bool },
}

impl fmt::Display for CompositionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyStarted => formatter.write_str("composition has already started"),
            Self::Signal(error) => write!(formatter, "{error}"),
            Self::Component { name, source } => {
                write!(formatter, "component {name} failed: {source}")
            }
            Self::TaskJoin { name, cancelled } => {
                write!(
                    formatter,
                    "component {name} join failed (cancelled: {cancelled})"
                )
            }
        }
    }
}

impl Error for CompositionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Signal(error) => Some(error),
            Self::Component { source, .. } => Some(source.as_ref()),
            Self::AlreadyStarted | Self::TaskJoin { .. } => None,
        }
    }
}

struct NamedTask {
    name: String,
    result: Result<(), BoxError>,
}

/// Owns every registered component task and its cancellation lifecycle.
pub struct Composition<S> {
    signal: Option<S>,
    cancellation: CancellationToken,
    tasks: Vec<(String, ComponentFuture)>,
    readiness: Readiness,
}

impl<S> Composition<S>
where
    S: ShutdownSignal,
{
    /// Creates an empty composition with an owned shutdown signal.
    pub fn new(signal: S) -> Self {
        Self {
            signal: Some(signal),
            cancellation: CancellationToken::new(),
            tasks: Vec::new(),
            readiness: Readiness::Starting,
        }
    }

    /// Returns a child cancellation token for a component or test seam.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.child_token()
    }

    /// Returns internal readiness without exposing an HTTP health surface.
    pub fn readiness(&self) -> Readiness {
        self.readiness
    }

    /// Registers one owned component before startup.
    pub fn register<F>(&mut self, name: impl Into<String>, task: F) -> Result<(), CompositionError>
    where
        F: Future<Output = Result<(), BoxError>> + Send + 'static,
    {
        if self.readiness != Readiness::Starting {
            return Err(CompositionError::AlreadyStarted);
        }
        let name = name.into();
        let task: ComponentFuture = Box::pin(task);
        self.tasks.push((name, task));
        Ok(())
    }

    /// Runs until a signal, task failure, or natural completion, then joins all tasks.
    pub async fn run(mut self) -> Result<RunReport, CompositionError> {
        self.readiness = Readiness::Ready;
        let signal = match self.signal.take() {
            Some(signal) => signal,
            None => return Err(CompositionError::AlreadyStarted),
        };
        let mut signal_future = signal.wait();
        let mut tasks = JoinSet::new();
        let mut task_names = HashMap::new();
        for (name, task) in self.tasks {
            let task_name = name.clone();
            let task_id = tasks.spawn(async move {
                let result = task.await;
                NamedTask { name, result }
            });
            task_names.insert(task_id.id(), task_name);
        }
        let mut signal_done = false;
        let mut failure = None;
        let mut shutdown_requested = false;
        let mut joined_tasks = 0;
        let had_tasks = !tasks.is_empty();

        loop {
            if tasks.is_empty() {
                if had_tasks || signal_done || failure.is_some() {
                    break;
                }
            }

            tokio::select! {
                signal_result = &mut signal_future, if !signal_done && failure.is_none() => {
                    signal_done = true;
                    match signal_result {
                        Ok(()) => {
                            shutdown_requested = true;
                            self.readiness = Readiness::Stopping;
                            self.cancellation.cancel();
                        }
                        Err(error) => {
                            failure = Some(CompositionError::Signal(error));
                            self.readiness = Readiness::Stopping;
                            self.cancellation.cancel();
                        }
                    }
                }
                joined = tasks.join_next_with_id(), if !tasks.is_empty() => {
                    match joined {
                        Some(Ok((_id, task))) => {
                            joined_tasks += 1;
                            if let Err(source) = task.result {
                                self.readiness = Readiness::Stopping;
                                self.cancellation.cancel();
                                if failure.is_none() {
                                    failure = Some(CompositionError::Component { name: task.name, source });
                                }
                            }
                        }
                        Some(Err(error)) => {
                            joined_tasks += 1;
                            self.readiness = Readiness::Stopping;
                            self.cancellation.cancel();
                            if failure.is_none() {
                                let name = match task_names.remove(&error.id()) {
                                    Some(name) => name,
                                    None => "unknown".to_owned(),
                                };
                                failure = Some(CompositionError::TaskJoin {
                                    name,
                                    cancelled: error.is_cancelled(),
                                });
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        while let Some(joined) = tasks.join_next_with_id().await {
            joined_tasks += 1;
            match joined {
                Ok((_id, task)) => {
                    if failure.is_none() {
                        if let Err(source) = task.result {
                            failure = Some(CompositionError::Component {
                                name: task.name,
                                source,
                            });
                        }
                    }
                }
                Err(error) => {
                    if failure.is_none() {
                        let name = match task_names.remove(&error.id()) {
                            Some(name) => name,
                            None => "unknown".to_owned(),
                        };
                        failure = Some(CompositionError::TaskJoin {
                            name,
                            cancelled: error.is_cancelled(),
                        });
                    }
                }
            }
        }

        self.readiness = Readiness::Stopped;
        if let Some(error) = failure {
            Err(error)
        } else {
            Ok(RunReport {
                readiness: self.readiness,
                shutdown_requested,
                joined_tasks,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Composition, CompositionError, Readiness, channel_signal};
    use std::{error::Error, io};

    fn boxed_error(message: &'static str) -> super::BoxError {
        Box::new(io::Error::other(message))
    }

    #[tokio::test]
    async fn signal_cancels_and_joins_components() {
        let (mut trigger, signal) = channel_signal();
        let mut composition = Composition::new(signal);
        let token = composition.cancellation_token();
        let registered = composition.register("worker", async move {
            token.cancelled().await;
            Ok(())
        });
        assert!(registered.is_ok());
        let run = tokio::spawn(composition.run());
        tokio::task::yield_now().await;
        let triggered = trigger.trigger();
        assert!(triggered.is_ok());
        let result = match run.await {
            Ok(result) => result,
            Err(error) => Err(CompositionError::TaskJoin {
                name: "runner".to_owned(),
                cancelled: error.is_cancelled(),
            }),
        };
        assert!(result.is_ok());
        if let Ok(report) = result {
            assert_eq!(report.readiness, Readiness::Stopped);
            assert!(report.shutdown_requested);
            assert_eq!(report.joined_tasks, 1);
        }
    }

    #[tokio::test]
    async fn component_errors_reach_the_owner() {
        let (_trigger, signal) = channel_signal();
        let mut composition = Composition::new(signal);
        let registered =
            composition.register("failing", async { Err(boxed_error("component failed")) });
        assert!(registered.is_ok());
        let result = composition.run().await;
        assert!(matches!(
            result,
            Err(CompositionError::Component { ref name, .. }) if name == "failing"
        ));
    }

    #[tokio::test]
    async fn component_failure_is_not_swallowed_by_shutdown() {
        let (mut trigger, signal) = channel_signal();
        let mut composition = Composition::new(signal);
        let registered =
            composition.register("racing", async { Err(boxed_error("racing failure")) });
        assert!(registered.is_ok());
        let triggered = trigger.trigger();
        assert!(triggered.is_ok());
        let result = composition.run().await;
        assert!(matches!(
            result,
            Err(CompositionError::Component { ref name, .. }) if name == "racing"
        ));
    }

    #[tokio::test]
    async fn signal_listener_errors_reach_the_owner() {
        let (trigger, signal) = channel_signal();
        drop(trigger);
        let composition = Composition::new(signal);
        let result = composition.run().await;
        assert!(matches!(result, Err(CompositionError::Signal(_))));
    }

    #[test]
    fn component_registration_is_owned_by_startup() {
        let (_trigger, signal) = channel_signal();
        let mut composition = Composition::new(signal);
        let registered = composition.register("one", async { Ok(()) });
        assert!(registered.is_ok());
        assert_eq!(composition.readiness(), Readiness::Starting);
    }

    #[allow(dead_code)]
    fn _assert_error_object_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CompositionError>();
        let _ = boxed_error("not used");
        let _ = io::Error::other("not used").source();
    }
}
