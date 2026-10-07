use std::time::Duration;

use bot_application::{
    BoxFuture, SessionConnector, SessionError, SessionEvent, SessionHandle, SessionReadiness,
};
use bot_domain::Pairing;
use tokio::{task::JoinHandle, time::timeout};
use tokio_util::sync::CancellationToken;

use crate::AdapterError;

pub struct RustPlusSessionConnector {
    reconnect: rustplus_client::ReconnectPolicy,
}

impl RustPlusSessionConnector {
    pub fn new(reconnect: rustplus_client::ReconnectPolicy) -> Result<Self, AdapterError> {
        reconnect
            .validate()
            .map_err(AdapterError::InvalidReconnectPolicy)?;
        Ok(Self { reconnect })
    }
}

impl SessionConnector for RustPlusSessionConnector {
    fn connect<'a>(
        &'a self,
        pairing: &'a Pairing,
    ) -> BoxFuture<'a, Result<Box<dyn SessionHandle>, SessionError>> {
        let reconnect = self.reconnect;
        Box::pin(async move {
            let endpoint = endpoint(pairing).map_err(|_| SessionError::Protocol)?;
            let mut config = rustplus_client::ClientConfig::new(
                endpoint,
                pairing.credentials().player_id().value(),
                pairing.credentials().player_token().value(),
            );
            config.reconnect = reconnect;
            let client = rustplus_client::RustPlusClient::new(config).map_err(map_client_error)?;
            let mut readiness = client.subscribe_readiness();
            let events = client.subscribe_events();
            let driver_client = client.clone();
            let mut driver =
                tokio::spawn(async move { driver_client.connect_with_reconnect().await });
            loop {
                if *readiness.borrow() == rustplus_client::Readiness::Ready {
                    return Ok(Box::new(RustPlusSessionHandle {
                        client,
                        readiness,
                        events,
                        driver: Some(driver),
                        cancellation: CancellationToken::new(),
                        shutdown_timeout: Duration::from_secs(5),
                    }) as Box<dyn SessionHandle>);
                }
                tokio::select! {
                    result = &mut driver => {
                        return match result {
                            Ok(Ok(())) => Err(SessionError::Disconnected),
                            Ok(Err(error)) => Err(map_client_error(error)),
                            Err(_) => Err(SessionError::Failed),
                        };
                    }
                    changed = readiness.changed() => {
                        if changed.is_err() { return Err(SessionError::Disconnected); }
                    }
                    _ = tokio::time::sleep(Duration::from_secs(60 * 60)) => {
                        return Err(SessionError::ConnectTimeout(Duration::from_secs(60 * 60)));
                    }
                }
            }
        })
    }
}

pub struct RustPlusSessionHandle {
    client: rustplus_client::RustPlusClient,
    readiness: tokio::sync::watch::Receiver<rustplus_client::Readiness>,
    events: rustplus_client::EventReceiver,
    driver: Option<JoinHandle<rustplus_client::Result<()>>>,
    cancellation: CancellationToken,
    shutdown_timeout: Duration,
}

impl SessionHandle for RustPlusSessionHandle {
    fn readiness(&self) -> SessionReadiness {
        map_readiness(self.client.readiness(), self.driver.is_some())
    }

    fn next_event<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<SessionEvent>, SessionError>> {
        Box::pin(async move {
            loop {
                if let Some(driver) = self.driver.as_mut() {
                    tokio::select! {
                        result = &mut *driver => {
                            self.driver = None;
                            return match result {
                                Ok(Ok(())) => Ok(Some(SessionEvent::Terminated { error: None })),
                                Ok(Err(error)) => Ok(Some(SessionEvent::Terminated { error: Some(map_client_error(error)) })),
                                Err(_) => Ok(Some(SessionEvent::Terminated { error: Some(SessionError::Failed) })),
                            };
                        }
                        changed = self.readiness.changed() => {
                            if changed.is_err() { return Ok(None); }
                            match map_readiness(*self.readiness.borrow(), self.driver.is_some()) {
                                SessionReadiness::Ready => return Ok(Some(SessionEvent::Ready)),
                                SessionReadiness::Reconnecting => return Ok(Some(SessionEvent::Reconnecting)),
                                SessionReadiness::Closing | SessionReadiness::Closed => return Ok(Some(SessionEvent::Terminated { error: None })),
                                SessionReadiness::Connecting => {}
                            }
                        }
                        event = self.events.recv() => {
                            match event {
                                Ok(_) => {}
                                Err(rustplus_client::EventReceiveError::Lagged { skipped }) => return Ok(Some(SessionEvent::Lagged { skipped })),
                                Err(rustplus_client::EventReceiveError::Closed) => return Ok(None),
                            }
                        }
                    }
                } else {
                    return Ok(Some(SessionEvent::Terminated { error: None }));
                }
            }
        })
    }

    fn shutdown<'a>(&'a mut self) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            self.cancellation.cancel();
            let disconnect = self.client.disconnect().await.map_err(map_client_error);
            let driver_result = if let Some(driver) = self.driver.take() {
                match timeout(self.shutdown_timeout, driver).await {
                    Ok(Ok(Ok(()))) => Ok(()),
                    Ok(Ok(Err(error))) => Err(map_client_error(error)),
                    Ok(Err(_)) => Err(SessionError::Failed),
                    Err(_) => Err(SessionError::ShutdownTimeout(self.shutdown_timeout)),
                }
            } else {
                Ok(())
            };
            disconnect.and(driver_result)
        })
    }
}

impl Drop for RustPlusSessionHandle {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(driver) = self.driver.take() {
            driver.abort();
        }
    }
}

fn endpoint(pairing: &Pairing) -> Result<String, AdapterError> {
    let host = pairing.address().host();
    if host.is_empty()
        || host
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        || host.contains('/')
        || host.contains('?')
        || host.contains('#')
        || host.contains("//")
    {
        return Err(AdapterError::InvalidEndpoint);
    }
    let host = if host.contains(':') {
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(AdapterError::InvalidEndpoint);
        }
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    Ok(format!("wss://{host}:{}", pairing.address().port()))
}

fn map_readiness(readiness: rustplus_client::Readiness, driver_alive: bool) -> SessionReadiness {
    match readiness {
        rustplus_client::Readiness::Connecting => SessionReadiness::Connecting,
        rustplus_client::Readiness::Ready => SessionReadiness::Ready,
        rustplus_client::Readiness::Closing => SessionReadiness::Closing,
        rustplus_client::Readiness::Disconnected if driver_alive => SessionReadiness::Reconnecting,
        rustplus_client::Readiness::Disconnected => SessionReadiness::Closed,
    }
}

fn map_client_error(error: rustplus_client::Error) -> SessionError {
    match error {
        rustplus_client::Error::Timeout(duration) => SessionError::ConnectTimeout(duration),
        rustplus_client::Error::Disconnected => SessionError::Disconnected,
        rustplus_client::Error::Transport | rustplus_client::Error::Io => SessionError::Transport,
        rustplus_client::Error::ShuttingDown => SessionError::ShuttingDown,
        rustplus_client::Error::ShutdownTimeout(duration) => {
            SessionError::ShutdownTimeout(duration)
        }
        rustplus_client::Error::Api(_) => SessionError::InvalidCredentials,
        _ => SessionError::Protocol,
    }
}
