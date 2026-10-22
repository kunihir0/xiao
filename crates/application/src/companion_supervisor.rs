//! The Rust+ companion supervisor (ADR 0003, flow step 7).
//!
//! One owned task decides which companion account's pairing service runs, and
//! when. It is a single `select!` loop that owns at most one running pairing
//! future (a field, never spawned), so the pairing task's lifetime is exactly
//! the lifetime of [`CompanionSupervisor::run`]. A child cancellation token
//! stops it, and the supervisor always awaits the future to completion before
//! it touches any store the pairing service writes to.
//!
//! # Startup precedence
//!
//! An active credential in the database always wins over the legacy
//! (`RUSTPLUS_STEAM_ID`) account, so leftover legacy config can never reverse
//! an owner's `/login` replacement. See [`CompanionSupervisor::run`].
//!
//! # Recoverable states
//!
//! When `/login` is enabled, "more than one active credential" and "legacy
//! token stale or unreadable" log an error and idle instead of stopping the
//! bot, so the owner can recover through `/login`. Without `/login` they stay
//! fatal. Restore failures at startup (decryption, an unreadable store, a
//! pairing service that cannot restore) are always fatal; after a runtime
//! switch they are not.
//!
//! # Shutdown
//!
//! Cancelling the supervisor cancels and awaits the running pairing. An error
//! from the pairing started at startup is returned, so the process exits
//! non-zero; an error from a pairing started by a runtime switch is logged and
//! the supervisor returns `Ok`.
//!
//! # Logging
//!
//! Log lines carry fixed messages and fixed labels only. SteamIDs, tokens,
//! links and store error text are never logged.

use std::{fmt, future::pending, sync::Arc};

use bot_domain::SteamId;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::{
    AccountSwitch, BoxFuture, CompanionRegistrar, EnrollmentError, PairingStore, PushCredential,
    PushCredentialStore, RegistrationError, SteamAuthTokenSource, StoreError, SwitchError,
    ensure_companion_registration, login_coordinator::kind as store_kind,
};

/// Why a pairing runtime could not start or stopped. Fixed variants: nothing
/// here carries a payload, so it can be logged and displayed freely. The
/// implementation logs any detail it has before it maps to one of these.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeError {
    /// The connector for the credential could not be built.
    Connector,
    /// The pairing service could not restore its persisted pairings.
    Restore,
    /// The pairing service failed while running.
    Service,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Connector => "pairing connector could not be built",
            Self::Restore => "pairing restore failed",
            Self::Service => "pairing service failed",
        })
    }
}
impl std::error::Error for RuntimeError {}

/// Turns an active credential into a running pairing service. `apps/bot`
/// implements it; the supervisor owns the returned future.
pub trait PairingRuntime: Send + Sync {
    /// Builds the connector for `credential` and returns the running pairing
    /// service as a future.
    ///
    /// The synchronous `Err` covers failures to build the connector. The
    /// returned future must finish promptly once `cancel` is cancelled; the
    /// supervisor awaits it to completion before it clears or retires
    /// anything. It does nothing until polled.
    fn start(
        &self,
        credential: PushCredential,
        cancel: CancellationToken,
    ) -> Result<BoxFuture<'static, Result<(), RuntimeError>>, RuntimeError>;
}

/// The legacy (operator-supplied, group B) account.
pub struct LegacyAccount {
    steam_id: SteamId,
    tokens: Arc<dyn SteamAuthTokenSource>,
}

impl LegacyAccount {
    #[must_use]
    pub fn new(steam_id: SteamId, tokens: Arc<dyn SteamAuthTokenSource>) -> Self {
        Self { steam_id, tokens }
    }
}

impl fmt::Debug for LegacyAccount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LegacyAccount([REDACTED])")
    }
}

/// Why [`CompanionSupervisor::run`] ended with an error. Every one of these
/// stops the bot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SupervisorError {
    /// The credential store could not list or load the active account.
    Store(StoreError),
    /// An account was listed active but its credential could not be loaded.
    CredentialMissing,
    /// More than one active account, and `/login` is not enabled to fix it.
    MultipleActiveAccounts,
    /// Registering the legacy account failed.
    Enrollment(EnrollmentError),
    /// The pairing runtime failed to start, or ended with an error that the
    /// supervisor does not recover from.
    Runtime(RuntimeError),
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "companion credential store failed: {error}"),
            Self::CredentialMissing => {
                formatter.write_str("an active companion credential could not be loaded")
            }
            Self::MultipleActiveAccounts => formatter
                .write_str("more than one active companion account; enable /login to resolve it"),
            Self::Enrollment(error) => write!(formatter, "companion enrollment failed: {error}"),
            Self::Runtime(error) => write!(formatter, "rust+ pairing runtime failed: {error}"),
        }
    }
}

impl std::error::Error for SupervisorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Enrollment(error) => Some(error),
            Self::Runtime(error) => Some(error),
            Self::CredentialMissing | Self::MultipleActiveAccounts => None,
        }
    }
}

/// The ports the supervisor drives.
pub struct SupervisorPorts {
    pub credentials: Arc<dyn PushCredentialStore>,
    pub pairings: Arc<dyn PairingStore>,
    pub registrar: Arc<dyn CompanionRegistrar>,
    pub runtime: Arc<dyn PairingRuntime>,
}

/// The one running pairing service.
struct Running {
    account: SteamId,
    /// Child of the supervisor's token; cancelling it stops only this pairing.
    cancel: CancellationToken,
    future: BoxFuture<'static, Result<(), RuntimeError>>,
    /// Started during startup rather than by a runtime switch: its failure is
    /// fatal.
    from_startup: bool,
}

impl Running {
    /// Cancels the pairing and awaits it to completion. Returns only after the
    /// future has finished and been dropped.
    async fn stop(slot: &mut Option<Self>) -> Result<(), RuntimeError> {
        let Some(running) = slot.as_mut() else {
            return Ok(());
        };
        running.cancel.cancel();
        let result = running.future.as_mut().await;
        // Dropped here, strictly after it completed. The slot is only emptied
        // once the future is done, so dropping this call mid-await leaves the
        // pairing owned (and stoppable) by the caller.
        *slot = None;
        result
    }

    /// Resolves when the pairing ends on its own; never resolves when idle.
    async fn finished(slot: &mut Option<Self>) -> Result<(), RuntimeError> {
        match slot {
            Some(running) => running.future.as_mut().await,
            None => pending().await,
        }
    }
}

/// Owns which companion account runs. Build with [`CompanionSupervisor::new`]
/// and drive with [`CompanionSupervisor::run`].
pub struct CompanionSupervisor {
    ports: SupervisorPorts,
    legacy: Option<LegacyAccount>,
    login_enabled: bool,
    switches: mpsc::Receiver<AccountSwitch>,
}

impl CompanionSupervisor {
    #[must_use]
    pub fn new(
        ports: SupervisorPorts,
        legacy: Option<LegacyAccount>,
        login_enabled: bool,
        switches: mpsc::Receiver<AccountSwitch>,
    ) -> Self {
        Self {
            ports,
            legacy,
            login_enabled,
            switches,
        }
    }

    /// Runs until `cancel` fires, then cancels and awaits the pairing service.
    ///
    /// Startup precedence, from `PushCredentialStore::list_active`:
    /// - Listing fails: fatal.
    /// - Exactly one active account: load and start it (a store or decrypt
    ///   error is fatal). A differing legacy account is ignored with a
    ///   warning; the registrar and token source are never called.
    /// - None, with a legacy account: restore or register it. A stale or
    ///   unreadable legacy token idles when `/login` is enabled and is fatal
    ///   otherwise; any other error is fatal.
    /// - None, without a legacy account: idle, awaiting `/login`.
    /// - More than one: idle when `/login` is enabled, fatal otherwise.
    ///
    /// Afterwards it handles [`AccountSwitch`] messages. A closed switch
    /// channel does not stop supervision of the current state. A pairing that
    /// ends with an error after a runtime switch idles the supervisor; one
    /// started at startup ends it with an error.
    ///
    /// On cancellation the pairing is cancelled and awaited. Returns `Ok`
    /// unless the pairing started at startup ended with an error while
    /// stopping; an error from a pairing a runtime switch started is logged.
    pub async fn run(mut self, cancel: CancellationToken) -> Result<(), SupervisorError> {
        let mut running = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(()),
            started = self.start_up(&cancel) => started?,
        };
        let mut switches_open = true;
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Self::shut_down(&mut running).await,
                result = Running::finished(&mut running) => {
                    let Some(ended) = running.take() else { continue };
                    Self::pairing_ended(&ended, result)?;
                }
                switch = self.switches.recv(), if switches_open => match switch {
                    Some(switch) => {
                        tokio::select! {
                            biased;
                            () = cancel.cancelled() => {
                                // Dropping the switch drops its ack, which the
                                // coordinator reads as "supervisor gone". Any
                                // pairing still in `running` is stopped here.
                                return Self::shut_down(&mut running).await;
                            }
                            () = self.handle_switch(&mut running, switch, &cancel) => {}
                        }
                    }
                    None => {
                        info!("account switch channel closed; supervising the current account");
                        switches_open = false;
                    }
                },
            }
        }
    }

    /// Stops the pairing. An error from the startup pairing is returned so the
    /// process exits non-zero; one from a pairing a runtime switch started is
    /// only logged, as it is after a switch.
    async fn shut_down(running: &mut Option<Running>) -> Result<(), SupervisorError> {
        let from_startup = running.as_ref().is_some_and(|current| current.from_startup);
        match Running::stop(running).await {
            Ok(()) => Ok(()),
            Err(error) if from_startup => {
                error!(%error, "the pairing service ended with an error during shutdown");
                Err(SupervisorError::Runtime(error))
            }
            Err(error) => {
                warn!(%error, "the pairing service ended with an error during shutdown");
                Ok(())
            }
        }
    }

    /// Handles a pairing future that ended without being cancelled.
    fn pairing_ended(
        ended: &Running,
        result: Result<(), RuntimeError>,
    ) -> Result<(), SupervisorError> {
        match result {
            Err(error) if ended.from_startup => {
                error!(%error, "the pairing service started at startup failed");
                Err(SupervisorError::Runtime(error))
            }
            Err(error) => {
                error!(%error, "the pairing service failed; idling until /login recovers it");
                Ok(())
            }
            Ok(()) => {
                warn!("the pairing service ended unexpectedly; idling until /login recovers it");
                Ok(())
            }
        }
    }

    // --- startup ------------------------------------------------------------

    async fn start_up(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Option<Running>, SupervisorError> {
        let active = self
            .ports
            .credentials
            .list_active()
            .await
            .map_err(SupervisorError::Store)?;
        match active.as_slice() {
            [] => self.start_up_without_active(cancel).await,
            [only] => {
                if self
                    .legacy
                    .as_ref()
                    .is_some_and(|legacy| legacy.steam_id != *only)
                {
                    warn!(
                        "the legacy RUSTPLUS_STEAM_ID differs from the active account; \
                         ignoring it (remove the legacy configuration)"
                    );
                }
                let credential = self
                    .ports
                    .credentials
                    .load_active(*only)
                    .await
                    .map_err(SupervisorError::Store)?
                    .ok_or(SupervisorError::CredentialMissing)?;
                self.launch(credential, cancel, true)
                    .map(Some)
                    .map_err(SupervisorError::Runtime)
            }
            _ if self.login_enabled => {
                error!("more than one active companion account; idling until /login resolves it");
                Ok(None)
            }
            _ => Err(SupervisorError::MultipleActiveAccounts),
        }
    }

    async fn start_up_without_active(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Option<Running>, SupervisorError> {
        let Some(legacy) = &self.legacy else {
            info!("no companion account configured; awaiting /login");
            return Ok(None);
        };
        let outcome = ensure_companion_registration(
            &*self.ports.credentials,
            &*self.ports.registrar,
            &*legacy.tokens,
            legacy.steam_id,
        )
        .await;
        match outcome {
            Ok(outcome) => {
                info!(kind = ?outcome.kind, "Rust+ companion credentials ready");
                self.launch(outcome.credential, cancel, true)
                    .map(Some)
                    .map_err(SupervisorError::Runtime)
            }
            Err(EnrollmentError::Registration(
                RegistrationError::ReenrollmentRequired
                | RegistrationError::AuthTokenUnavailable(_),
            )) if self.login_enabled => {
                error!("the legacy companion token is stale or unreadable; awaiting /login");
                Ok(None)
            }
            Err(error) => Err(SupervisorError::Enrollment(error)),
        }
    }

    fn launch(
        &self,
        credential: PushCredential,
        cancel: &CancellationToken,
        from_startup: bool,
    ) -> Result<Running, RuntimeError> {
        let account = credential.steam_id;
        let child = cancel.child_token();
        let future = self.ports.runtime.start(credential, child.clone())?;
        info!("pairing service started");
        Ok(Running {
            account,
            cancel: child,
            future,
            from_startup,
        })
    }

    // --- account switches ---------------------------------------------------

    async fn handle_switch(
        &self,
        running: &mut Option<Running>,
        switch: AccountSwitch,
        cancel: &CancellationToken,
    ) {
        let AccountSwitch {
            to,
            replace_from,
            ack,
        } = switch;
        // The coordinator drops its end when it gives up, so a closed ack
        // means the switch is stale: do nothing at all.
        if ack.is_closed() {
            info!("ignoring a stale account switch");
            return;
        }
        let result = match replace_from {
            None => Some(self.ensure_running(running, to, cancel).await),
            Some(_) => self.replace(running, to, &ack, cancel).await,
        };
        let Some(result) = result else {
            return;
        };
        if result.is_err() {
            warn!("account switch failed");
        }
        // The receiver may have gone while the switch ran.
        let _ = ack.send(result);
    }

    /// `replace_from == None`: make sure `to` is running.
    async fn ensure_running(
        &self,
        running: &mut Option<Running>,
        to: SteamId,
        cancel: &CancellationToken,
    ) -> Result<(), SwitchError> {
        match running.as_ref().map(|current| current.account) {
            Some(account) if account == to => Ok(()),
            Some(_) => {
                // Contract violation: `replace_from == None` says the
                // coordinator saw no other account, yet a different one is
                // running. Stopping a healthy pairing without the
                // clear/retire sequence would leave the old player's pairings
                // behind and two active credentials, so the running pairing is
                // kept and the switch is refused. `StoreFailed` is the closest
                // variant: the stores and the supervisor disagree.
                error!("account switch without a replaced account while another account runs");
                Err(SwitchError::StoreFailed)
            }
            None => self.restore(running, to, cancel).await,
        }
    }

    /// `replace_from == Some(_)`: check `to`, stop, purge, retire, then start
    /// `to`. Returns `None` when the switch went stale while the old pairing
    /// was stopping.
    async fn replace(
        &self,
        running: &mut Option<Running>,
        to: SteamId,
        ack: &oneshot::Sender<Result<(), SwitchError>>,
        cancel: &CancellationToken,
    ) -> Option<Result<(), SwitchError>> {
        // `to` already runs (a retried or duplicated switch): leave the pairing
        // and its pairings alone and only make sure no other account stays
        // active.
        if running
            .as_ref()
            .is_some_and(|current| current.account == to)
        {
            return Some(self.retire_other_accounts(to).await);
        }
        // 1. Nothing is destroyed for an account that cannot start: load `to`
        // first, so a failure leaves the old pairing and its data untouched.
        let credential = match self.load_credential(to).await {
            Ok(credential) => credential,
            Err(error) => return Some(Err(error)),
        };
        // 2. No pairing writer may race the purge: stop the old pairing and
        // wait for it. Its own failure does not matter, it is being replaced.
        let previous = running.as_ref().map(|current| current.account);
        if let Err(error) = Running::stop(running).await {
            warn!(%error, "the previous pairing service ended with an error while stopping");
        }
        // 3. Stopping can take a while; the coordinator may have given up. The
        // old pairing is already gone, so bring it back rather than leave
        // nothing running for a switch nobody is waiting for.
        if ack.is_closed() {
            warn!("account switch went stale while the previous pairing stopped; restarting it");
            if let Some(previous) = previous {
                self.relaunch(running, previous, cancel).await;
            }
            return None;
        }
        // 4. The old pairings belong to the old player.
        if let Err(error) = self.ports.pairings.clear().await {
            error!(
                error = store_kind(&error),
                "clearing the previous pairings failed"
            );
            return Some(Err(SwitchError::StoreFailed));
        }
        // 5. Only the new account's credential survives.
        if let Err(error) = self.retire_other_accounts(to).await {
            return Some(Err(error));
        }
        // 6. Start the new account with the credential loaded in step 1.
        Some(self.start(running, credential, cancel))
    }

    async fn retire_other_accounts(&self, keep: SteamId) -> Result<(), SwitchError> {
        self.ports
            .credentials
            .retire_other_accounts(keep)
            .await
            .map(drop)
            .map_err(|error| {
                error!(
                    error = store_kind(&error),
                    "retiring the previous companion account failed"
                );
                SwitchError::StoreFailed
            })
    }

    /// Restarts the account that ran before a switch that went stale. A
    /// failure leaves the supervisor idle.
    async fn relaunch(
        &self,
        running: &mut Option<Running>,
        previous: SteamId,
        cancel: &CancellationToken,
    ) {
        let restarted = match self.load_credential(previous).await {
            Ok(credential) => self.start(running, credential, cancel),
            Err(error) => Err(error),
        };
        if restarted.is_err() {
            error!("the previous pairing service could not be restarted; idling");
        }
    }

    /// Loads `to`'s active credential and starts it. Every failure is a
    /// `RestoreFailed` and leaves the supervisor idle.
    async fn restore(
        &self,
        running: &mut Option<Running>,
        to: SteamId,
        cancel: &CancellationToken,
    ) -> Result<(), SwitchError> {
        let credential = self.load_credential(to).await?;
        self.start(running, credential, cancel)
    }

    async fn load_credential(&self, account: SteamId) -> Result<PushCredential, SwitchError> {
        match self.ports.credentials.load_active(account).await {
            Ok(Some(credential)) => Ok(credential),
            Ok(None) => {
                error!("the companion credential is not active");
                Err(SwitchError::RestoreFailed)
            }
            Err(error) => {
                error!(
                    error = store_kind(&error),
                    "loading the companion credential failed"
                );
                Err(SwitchError::RestoreFailed)
            }
        }
    }

    fn start(
        &self,
        running: &mut Option<Running>,
        credential: PushCredential,
        cancel: &CancellationToken,
    ) -> Result<(), SwitchError> {
        match self.launch(credential, cancel, false) {
            Ok(started) => {
                *running = Some(started);
                Ok(())
            }
            Err(error) => {
                error!(%error, "starting the pairing service failed");
                Err(SwitchError::RestoreFailed)
            }
        }
    }
}
