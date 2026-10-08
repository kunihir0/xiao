//! Narrow Poise/Serenity adapter for the bot's public Discord command surface.
//!
//! Poise and Serenity types stay in this crate. The application only deals with
//! [`DiscordConfig`], [`DiscordHandle`], readiness, and [`DiscordError`].

use std::{fmt, future::Future, pin::Pin, time::Duration};

use poise::serenity_prelude as serenity;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;
#[cfg(feature = "live-acceptance")]
pub mod acceptance;

const REDACTED_TOKEN: &str = "[REDACTED]";

/// The deterministic response returned by the public `/ping` command.
pub const PING_RESPONSE: &str = "pong";

/// The public slash command's registered name.
pub const PING_COMMAND_NAME: &str = "ping";

/// Readiness of the Discord gateway and command registration lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiscordReadiness {
    /// The client has not completed gateway setup and command registration.
    Starting,
    /// Gateway setup and global command registration completed successfully.
    Ready,
    /// Cancellation was requested and shard shutdown is in progress.
    Stopping,
    /// The owned Discord future has terminated.
    Stopped,
}

/// Configuration owned by the Discord adapter.
#[derive(Clone)]
pub struct DiscordConfig {
    token: String,
}

impl DiscordConfig {
    /// Validates and stores a Discord bot token.
    pub fn new(token: impl Into<String>) -> Result<Self, DiscordError> {
        let token = token.into();
        if token.trim().is_empty() {
            return Err(DiscordError::InvalidToken);
        }
        Ok(Self { token })
    }

    /// The value used in diagnostics in place of the credential.
    pub const fn redacted_token() -> &'static str {
        REDACTED_TOKEN
    }
}

impl fmt::Debug for DiscordConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiscordConfig")
            .field("token", &REDACTED_TOKEN)
            .finish()
    }
}

/// Errors surfaced by the owned Discord lifecycle.
#[derive(Debug, thiserror::Error)]
pub enum DiscordError {
    /// The adapter was given an empty credential.
    #[error("Discord token must not be empty")]
    InvalidToken,
    /// Serenity could not construct an authenticated client.
    #[error("failed to build Discord client: {0}")]
    ClientBuild(String),
    /// Global slash-command registration failed during Poise setup.
    #[error("Discord command registration failed: {0}")]
    Registration(String),
    /// The gateway terminated with an error.
    #[error("Discord gateway failed: {0}")]
    Gateway(String),
    #[error("Discord client task failed: {0}")]
    ClientTask(String),
    /// Graceful shutdown exceeded its bounded wait.
    #[error("Discord client shutdown timed out after {0:?}")]
    ShutdownTimeout(Duration),
    #[error("Discord command response failed: {0}")]
    Command(String),
    #[error("Discord API error: {0}")]
    Api(String),
}

/// The future owned by [`DiscordHandle`] and registered by application composition.
pub type DiscordRunFuture =
    Pin<Box<dyn Future<Output = Result<(), DiscordError>> + Send + 'static>>;
type ClientStartFuture = Pin<Box<dyn Future<Output = Result<(), DiscordError>> + Send>>;
type ShutdownFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type ShutdownFn = Box<dyn Fn() -> ShutdownFuture + Send + Sync>;
type ClientTask = tokio_util::task::AbortOnDropHandle<Result<(), DiscordError>>;
const CLIENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Owns Discord startup, readiness, and graceful shutdown.
///
/// Constructing a handle does not spawn a task or perform network I/O. Call
/// [`DiscordHandle::run`] and register the returned future with the process
/// composition before that future is polled.
pub struct DiscordHandle {
    config: DiscordConfig,
    readiness_sender: watch::Sender<DiscordReadiness>,
    readiness_receiver: watch::Receiver<DiscordReadiness>,
}

impl DiscordHandle {
    /// Creates an unstarted Discord component.
    pub fn new(config: DiscordConfig) -> Self {
        let (readiness_sender, readiness_receiver) = watch::channel(DiscordReadiness::Starting);
        Self {
            config,
            readiness_sender,
            readiness_receiver,
        }
    }

    /// Returns the latest readiness state without exposing Serenity internals.
    pub fn readiness(&self) -> DiscordReadiness {
        *self.readiness_receiver.borrow()
    }

    /// Returns a receiver for readiness transitions.
    pub fn readiness_receiver(&self) -> watch::Receiver<DiscordReadiness> {
        self.readiness_receiver.clone()
    }

    /// Returns the owned, unspawned lifecycle future.
    pub fn run(self, cancellation: CancellationToken) -> DiscordRunFuture {
        Box::pin(async move { self.run_inner(cancellation).await })
    }

    async fn run_inner(self, cancellation: CancellationToken) -> Result<(), DiscordError> {
        let DiscordHandle {
            config,
            readiness_sender,
            readiness_receiver: _,
        } = self;

        if cancellation.is_cancelled() {
            readiness_sender.send_replace(DiscordReadiness::Stopping);
            readiness_sender.send_replace(DiscordReadiness::Stopped);
            return Ok(());
        }

        let (setup_sender, setup_receiver) = oneshot::channel();
        let framework = poise::Framework::builder()
            .options(poise::FrameworkOptions {
                commands: vec![ping()],
                on_error: on_framework_error,
                ..Default::default()
            })
            .setup(move |ctx, ready_event, _framework| {
                Box::pin(async move {
                    tracing::info!(
                        bot_user = %ready_event.user.name,
                        bot_id = %ready_event.user.id.get(),
                        guild_count = ready_event.guilds.len(),
                        "Discord gateway READY received"
                    );
                    match register_global_ping(ctx).await {
                        Ok(()) => {
                            tracing::info!("Discord global slash command registration succeeded");
                            let _ = setup_sender.send(SetupStatus::Ready);
                            Ok(())
                        }
                        Err(error) => {
                            let detail = error.to_string();
                            tracing::error!(
                                error = %detail,
                                "Discord command registration failed during setup"
                            );
                            let _ = setup_sender.send(SetupStatus::RegistrationFailed(detail));
                            Ok(())
                        }
                    }
                })
            })
            .build();

        let intents = serenity::GatewayIntents::GUILDS;
        let client_builder =
            serenity::ClientBuilder::new(config.token, intents).framework(framework);
        let mut client = tokio::select! {
            result = std::future::IntoFuture::into_future(client_builder) => match result {
                Ok(client) => client,
                Err(error) => {
                    return Err(client_build_error(&readiness_sender, error.to_string()));
                }
            },
            _ = cancellation.cancelled() => {
                readiness_sender.send_replace(DiscordReadiness::Stopping);
                readiness_sender.send_replace(DiscordReadiness::Stopped);
                return Ok(());
            }
        };
        let shard_manager = client.shard_manager.clone();
        let client_start: ClientStartFuture = Box::pin(async move {
            client
                .start()
                .await
                .map_err(|error| DiscordError::Gateway(error.to_string()))
        });
        let shutdown: ShutdownFn = Box::new(move || {
            let shard_manager = shard_manager.clone();
            Box::pin(async move {
                shard_manager.shutdown_all().await;
            })
        });

        run_lifecycle(
            client_start,
            setup_receiver,
            cancellation,
            readiness_sender,
            shutdown,
        )
        .await
    }
}

async fn register_global_ping(ctx: &serenity::Context) -> Result<(), Box<serenity::Error>> {
    let commands = ctx.http.get_global_commands().await?;
    if commands
        .iter()
        .any(|command| command.name == PING_COMMAND_NAME)
    {
        return Ok(());
    }
    let command =
        serenity::builder::CreateCommand::new(PING_COMMAND_NAME).description("Responds with pong.");
    ctx.http.create_global_command(&command).await?;
    Ok(())
}

async fn run_lifecycle(
    client_start: ClientStartFuture,
    setup_receiver: oneshot::Receiver<SetupStatus>,
    cancellation: CancellationToken,
    readiness_sender: watch::Sender<DiscordReadiness>,
    shutdown: ShutdownFn,
) -> Result<(), DiscordError> {
    run_lifecycle_with_timeout(
        client_start,
        setup_receiver,
        cancellation,
        readiness_sender,
        shutdown,
        CLIENT_SHUTDOWN_TIMEOUT,
    )
    .await
}

async fn run_lifecycle_with_timeout(
    client_start: ClientStartFuture,
    mut setup_receiver: oneshot::Receiver<SetupStatus>,
    cancellation: CancellationToken,
    readiness_sender: watch::Sender<DiscordReadiness>,
    shutdown: ShutdownFn,
    shutdown_timeout: Duration,
) -> Result<(), DiscordError> {
    if cancellation.is_cancelled() {
        readiness_sender.send_replace(DiscordReadiness::Stopping);
        publish_stopped(&readiness_sender);
        return Ok(());
    }

    let mut client_task = ClientTask::new(tokio::spawn(client_start));
    let mut setup_complete = false;
    let mut ready = false;

    let result = loop {
        tokio::select! {
            biased;
            setup_status = &mut setup_receiver, if !setup_complete => {
                setup_complete = true;
                match setup_status {
                    Ok(SetupStatus::Ready) => {
                        ready = true;
                        readiness_sender.send_replace(DiscordReadiness::Ready);
                    }
                    Ok(SetupStatus::RegistrationFailed(detail)) => {
                        readiness_sender.send_replace(DiscordReadiness::Stopping);
                        shutdown().await;
                        let client_result =
                            await_client_task(&mut client_task, shutdown_timeout).await;
                        break match client_result {
                            Ok(()) => Err(DiscordError::Registration(detail)),
                            Err(error) => Err(error),
                        };
                    }
                    Err(_) => {
                        readiness_sender.send_replace(DiscordReadiness::Stopping);
                        shutdown().await;
                        let client_result =
                            await_client_task(&mut client_task, shutdown_timeout).await;
                        break client_result.and_then(|_| Err(DiscordError::Gateway(
                            "setup callback ended before readiness".to_owned(),
                        )));
                    }
                }
            }
            _ = cancellation.cancelled() => {
                readiness_sender.send_replace(DiscordReadiness::Stopping);
                shutdown().await;
                break await_client_task(&mut client_task, shutdown_timeout).await;
            }
            client_result = &mut client_task => {
                if cancellation.is_cancelled() {
                    readiness_sender.send_replace(DiscordReadiness::Stopping);
                    shutdown().await;
                    break match client_result {
                        Ok(result) => result,
                        Err(error) => Err(DiscordError::ClientTask(error.to_string())),
                    };
                }
                break match client_result {
                    Ok(Ok(())) if ready => Ok(()),
                    Ok(Ok(())) => Err(DiscordError::Gateway(
                        "client terminated before Discord setup completed".to_owned(),
                    )),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(DiscordError::ClientTask(error.to_string())),
                };
            }
        }
    };

    publish_stopped(&readiness_sender);
    result.map(|_| ())
}

async fn await_client_task(
    client_task: &mut ClientTask,
    timeout: Duration,
) -> Result<(), DiscordError> {
    match tokio::time::timeout(timeout, &mut *client_task).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(DiscordError::ClientTask(error.to_string())),
        Err(_) => {
            client_task.abort();
            let _ = client_task.await;
            Err(DiscordError::ShutdownTimeout(timeout))
        }
    }
}

fn client_build_error(sender: &watch::Sender<DiscordReadiness>, error: String) -> DiscordError {
    publish_stopped(sender);
    DiscordError::ClientBuild(error)
}

fn publish_stopped(sender: &watch::Sender<DiscordReadiness>) {
    sender.send_replace(DiscordReadiness::Stopped);
}

#[derive(Debug)]
enum SetupStatus {
    Ready,
    RegistrationFailed(String),
}

type CommandContext<'a> = poise::Context<'a, (), DiscordError>;

/// Responds with pong.
#[poise::command(slash_command)]
async fn ping(ctx: CommandContext<'_>) -> Result<(), DiscordError> {
    tracing::info!(
        user = %ctx.author().name,
        user_id = %ctx.author().id.get(),
        guild_id = ?ctx.guild_id().map(|id| id.get()),
        "Received /ping command, responding with pong"
    );
    ctx.say(PING_RESPONSE)
        .await
        .map(|_| ())
        .map_err(|error| DiscordError::Command(error.to_string()))
}

fn on_framework_error(
    error: poise::FrameworkError<'_, (), DiscordError>,
) -> poise::BoxFuture<'_, ()> {
    Box::pin(async move {
        tracing::error!(error = ?error, "Discord framework error");
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ClientStartFuture, DiscordConfig, DiscordError, DiscordHandle, DiscordReadiness,
        PING_COMMAND_NAME, PING_RESPONSE, SetupStatus, ShutdownFn, run_lifecycle,
        run_lifecycle_with_timeout,
    };
    use tokio::sync::{oneshot, watch};
    use tokio_util::sync::CancellationToken;

    #[test]
    fn ping_metadata_is_slash_only_and_deterministic() {
        let command = super::ping();
        assert_eq!(command.name, PING_COMMAND_NAME);
        assert_eq!(command.description.as_deref(), Some("Responds with pong."));
        assert!(command.slash_action.is_some());
        assert!(command.prefix_action.is_none());
        assert_eq!(PING_RESPONSE, "pong");
    }

    #[test]
    fn config_rejects_empty_tokens_and_redacts_debug() {
        assert!(matches!(
            DiscordConfig::new("  "),
            Err(DiscordError::InvalidToken)
        ));
        assert!(DiscordConfig::new("secret-token").is_ok());
        if let Ok(config) = DiscordConfig::new("secret-token") {
            let debug = format!("{config:?}");
            assert!(!debug.contains("secret-token"));
            assert!(debug.contains(DiscordConfig::redacted_token()));
        }
    }

    #[tokio::test]
    async fn cancellation_before_start_stops_without_network_access() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(DiscordConfig::new("test-token").is_ok());
        if let Ok(config) = DiscordConfig::new("test-token") {
            let handle = DiscordHandle::new(config);
            assert_eq!(handle.readiness(), DiscordReadiness::Starting);
            let mut readiness = handle.readiness_receiver();
            let result = handle.run(cancellation).await;
            assert!(result.is_ok());
            assert_eq!(*readiness.borrow_and_update(), DiscordReadiness::Stopped);
        }
    }
    #[test]
    fn client_build_failure_publishes_stopped() {
        let (sender, receiver) = watch::channel(DiscordReadiness::Starting);
        let error = super::client_build_error(&sender, "build failed".to_owned());
        assert!(matches!(error, DiscordError::ClientBuild(message) if message == "build failed"));
        assert_eq!(*receiver.borrow(), DiscordReadiness::Stopped);
    }

    fn shutdown_driven_client() -> (ClientStartFuture, ShutdownFn) {
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        let client: ClientStartFuture = Box::pin(async move {
            let mut shutdown_receiver = shutdown_receiver;
            shutdown_receiver
                .wait_for(|shutdown| *shutdown)
                .await
                .map(|_| ())
                .map_err(|_| DiscordError::Gateway("shutdown signal dropped".to_owned()))
        });
        let shutdown: ShutdownFn = Box::new(move || {
            let _ = shutdown_sender.send(true);
            Box::pin(async {})
        });
        (client, shutdown)
    }

    #[tokio::test]
    async fn registration_failure_is_typed_and_completes_client_lifecycle() {
        let (setup_sender, setup_receiver) = oneshot::channel();
        let _ = setup_sender.send(SetupStatus::RegistrationFailed(
            "registration failed".to_owned(),
        ));
        let (client, shutdown) = shutdown_driven_client();
        let cancellation = CancellationToken::new();
        let (readiness_sender, readiness_receiver) = watch::channel(DiscordReadiness::Starting);

        let result = run_lifecycle(
            client,
            setup_receiver,
            cancellation,
            readiness_sender,
            shutdown,
        )
        .await;

        assert!(matches!(
            result,
            Err(DiscordError::Registration(message)) if message == "registration failed"
        ));
        assert_eq!(*readiness_receiver.borrow(), DiscordReadiness::Stopped);
    }
    #[tokio::test]
    async fn registration_failure_wins_when_client_already_terminated() {
        let (setup_sender, setup_receiver) = oneshot::channel();
        let _ = setup_sender.send(SetupStatus::RegistrationFailed(
            "registration failed".to_owned(),
        ));
        let client: ClientStartFuture = Box::pin(async { Ok(()) });
        let shutdown: ShutdownFn = Box::new(|| Box::pin(async {}));
        let (readiness_sender, readiness_receiver) = watch::channel(DiscordReadiness::Starting);

        let result = run_lifecycle(
            client,
            setup_receiver,
            CancellationToken::new(),
            readiness_sender,
            shutdown,
        )
        .await;

        assert!(matches!(
            result,
            Err(DiscordError::Registration(message)) if message == "registration failed"
        ));
        assert_eq!(*readiness_receiver.borrow(), DiscordReadiness::Stopped);
    }

    #[tokio::test]
    async fn registration_failure_propagates_client_gateway_failure() {
        let (setup_sender, setup_receiver) = oneshot::channel();
        let _ = setup_sender.send(SetupStatus::RegistrationFailed(
            "registration failed".to_owned(),
        ));
        let client: ClientStartFuture =
            Box::pin(async { Err(DiscordError::Gateway("gateway failed".to_owned())) });
        let shutdown: ShutdownFn = Box::new(|| Box::pin(async {}));
        let (readiness_sender, readiness_receiver) = watch::channel(DiscordReadiness::Starting);

        let result = run_lifecycle(
            client,
            setup_receiver,
            CancellationToken::new(),
            readiness_sender,
            shutdown,
        )
        .await;

        assert!(matches!(
            result,
            Err(DiscordError::Gateway(message)) if message == "gateway failed"
        ));
        assert_eq!(*readiness_receiver.borrow(), DiscordReadiness::Stopped);
    }

    #[tokio::test]
    async fn client_start_failure_is_surfaced_and_stopped() {
        let (_setup_sender, setup_receiver) = oneshot::channel();
        let client: ClientStartFuture =
            Box::pin(async { Err(DiscordError::Gateway("gateway failed".to_owned())) });
        let shutdown: ShutdownFn = Box::new(|| Box::pin(async {}));
        let (readiness_sender, readiness_receiver) = watch::channel(DiscordReadiness::Starting);

        let result = run_lifecycle(
            client,
            setup_receiver,
            CancellationToken::new(),
            readiness_sender,
            shutdown,
        )
        .await;

        assert!(matches!(
            result,
            Err(DiscordError::Gateway(message)) if message == "gateway failed"
        ));
        assert_eq!(*readiness_receiver.borrow(), DiscordReadiness::Stopped);
    }

    #[tokio::test]
    async fn cancellation_before_and_after_ready_shuts_down_owned_client() {
        let (setup_sender, setup_receiver) = oneshot::channel();
        let _ = setup_sender.send(SetupStatus::Ready);
        let (client, shutdown) = shutdown_driven_client();
        let cancellation = CancellationToken::new();
        let cancellation_for_task = cancellation.clone();
        let cancellation_task = tokio::spawn(async move {
            tokio::task::yield_now().await;
            cancellation_for_task.cancel();
        });
        let (readiness_sender, readiness_receiver) = watch::channel(DiscordReadiness::Starting);

        let result = run_lifecycle(
            client,
            setup_receiver,
            cancellation,
            readiness_sender,
            shutdown,
        )
        .await;
        let _ = cancellation_task.await;

        assert!(result.is_ok());
        assert_eq!(*readiness_receiver.borrow(), DiscordReadiness::Stopped);
    }

    #[tokio::test]
    async fn cancellation_before_lifecycle_start_stops_without_client_work() {
        let (_setup_sender, setup_receiver) = oneshot::channel();
        let (client, shutdown) = shutdown_driven_client();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let (readiness_sender, readiness_receiver) = watch::channel(DiscordReadiness::Starting);

        let result = run_lifecycle(
            client,
            setup_receiver,
            cancellation,
            readiness_sender,
            shutdown,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(*readiness_receiver.borrow(), DiscordReadiness::Stopped);
    }
    #[tokio::test]
    async fn cancellation_aborts_empty_manager_start_after_timeout() {
        let (_setup_sender, setup_receiver) = oneshot::channel();
        let client: ClientStartFuture = Box::pin(std::future::pending());
        let shutdown: ShutdownFn = Box::new(|| Box::pin(async {}));
        let cancellation = CancellationToken::new();
        let cancellation_for_task = cancellation.clone();
        let cancellation_task = tokio::spawn(async move {
            tokio::task::yield_now().await;
            cancellation_for_task.cancel();
        });
        let (readiness_sender, readiness_receiver) = watch::channel(DiscordReadiness::Starting);

        let result = run_lifecycle_with_timeout(
            client,
            setup_receiver,
            cancellation,
            readiness_sender,
            shutdown,
            std::time::Duration::from_millis(10),
        )
        .await;
        let _ = cancellation_task.await;

        assert!(matches!(
            result,
            Err(DiscordError::ShutdownTimeout(duration))
                if duration == std::time::Duration::from_millis(10)
        ));
        assert_eq!(*readiness_receiver.borrow(), DiscordReadiness::Stopped);
    }
}
