use std::time::Duration;

use bot_application::{
    BoxFuture, SessionConnector, SessionError, SessionEvent, SessionHandle, SessionReadiness,
};
use bot_domain::Pairing;
use tokio::time::timeout;
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use url::Url;

use crate::{AdapterError, parse::parse_public_ip};

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
            #[cfg(test)]
            let config = config.allow_insecure_for_tests();
            let client = rustplus_client::RustPlusClient::new(config).map_err(map_client_error)?;
            let mut readiness = client.subscribe_readiness();
            let events = client.subscribe_events();
            let driver_client = client.clone();
            let mut driver = AbortOnDropHandle::new(tokio::spawn(async move {
                driver_client.connect_with_reconnect().await
            }));
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
    driver: Option<AbortOnDropHandle<rustplus_client::Result<()>>>,
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
        drop(self.driver.take());
    }
}

fn endpoint(pairing: &Pairing) -> Result<String, AdapterError> {
    let address = if cfg!(test) {
        pairing
            .address()
            .host()
            .parse()
            .map_err(|_| AdapterError::InvalidEndpoint)?
    } else {
        parse_public_ip(pairing.address().host()).map_err(|_| AdapterError::InvalidEndpoint)?
    };
    let mut endpoint = Url::parse(if cfg!(test) {
        "ws://127.0.0.1"
    } else {
        "wss://127.0.0.1"
    })
    .map_err(|_| AdapterError::InvalidEndpoint)?;
    endpoint
        .set_ip_host(address)
        .map_err(|_| AdapterError::InvalidEndpoint)?;
    endpoint
        .set_port(Some(pairing.address().port()))
        .map_err(|_| AdapterError::InvalidEndpoint)?;
    Ok(endpoint.to_string())
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

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use bot_application::{SessionConnector, SessionEvent, SessionReadiness};
    use bot_domain::{Pairing, PlayerCredentials, PlayerId, PlayerToken, ServerAddress};
    use futures_util::{SinkExt, StreamExt};
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    fn fixture(name: &str) -> Vec<u8> {
        include_str!("../../rustplus-client/tests/fixtures/legacy-wire.txt")
            .lines()
            .find_map(|line| {
                let (key, bytes) = line.split_once(" = ")?;
                (key == name).then(|| {
                    bytes
                        .split_whitespace()
                        .map(|byte| u8::from_str_radix(byte, 16).expect("fixture byte"))
                        .collect()
                })
            })
            .expect("fixture entry")
    }

    fn pairing(port: u16) -> Pairing {
        Pairing::new(
            ServerAddress::new("127.0.0.1", port).expect("address"),
            PlayerCredentials::new(PlayerId::new(42).expect("player"), PlayerToken::new(7)),
            None,
            None,
        )
    }

    async fn server(
        connections: usize,
        close_after_info: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("listener");
        let endpoint = format!("ws://{}/", listener.local_addr().expect("address"));
        let task = tokio::spawn(async move {
            for _ in 0..connections {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let Ok(mut socket) = accept_async(stream).await else {
                    return;
                };
                let Some(Ok(Message::Binary(data))) = socket.next().await else {
                    return;
                };
                let mut response = fixture("response_success");
                if let Some(sequence) = data.get(1) {
                    response[3] = *sequence;
                }
                let _ = socket.send(Message::Binary(response.into())).await;
                if close_after_info {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    let _ = socket.close(None).await;
                } else {
                    while socket.next().await.is_some() {}
                }
            }
        });
        (endpoint, task)
    }

    fn policy(max_attempts: u32) -> rustplus_client::ReconnectPolicy {
        rustplus_client::ReconnectPolicy {
            max_attempts,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            jitter_percent: 0,
        }
    }

    #[tokio::test]
    async fn reaches_ready_after_info_and_shutdown_joins() {
        let (endpoint, server_task) = server(1, false).await;
        let port = endpoint
            .rsplit_once(':')
            .and_then(|(_, port)| port.trim_end_matches('/').parse().ok())
            .expect("port");
        let connector = RustPlusSessionConnector::new(policy(0)).expect("connector");
        let mut handle = connector.connect(&pairing(port)).await.expect("connected");
        assert_eq!(handle.readiness(), SessionReadiness::Ready);
        tokio::time::timeout(Duration::from_secs(1), handle.shutdown())
            .await
            .expect("shutdown bound")
            .expect("shutdown");
        tokio::time::timeout(Duration::from_secs(1), server_task)
            .await
            .expect("server join bound")
            .expect("server task");
    }

    #[tokio::test]
    async fn connection_drop_reports_reconnecting() {
        let (endpoint, server_task) = server(1, true).await;
        let port = endpoint
            .rsplit_once(':')
            .and_then(|(_, port)| port.trim_end_matches('/').parse().ok())
            .expect("port");
        let connector = RustPlusSessionConnector::new(policy(1)).expect("connector");
        let mut handle = connector.connect(&pairing(port)).await.expect("connected");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), handle.next_event())
                .await
                .expect("event bound")
                .expect("event"),
            Some(SessionEvent::Reconnecting)
        );
        tokio::time::timeout(Duration::from_secs(1), handle.shutdown())
            .await
            .expect("shutdown bound")
            .expect("shutdown");
        tokio::time::timeout(Duration::from_secs(1), server_task)
            .await
            .expect("server join bound")
            .expect("server task");
    }

    #[tokio::test]
    async fn reconnect_exhaustion_reports_terminal_event() {
        let (endpoint, server_task) = server(1, true).await;
        let port = endpoint
            .rsplit_once(':')
            .and_then(|(_, port)| port.trim_end_matches('/').parse().ok())
            .expect("port");
        let connector = RustPlusSessionConnector::new(policy(0)).expect("connector");
        let mut handle = connector.connect(&pairing(port)).await.expect("connected");
        let first = tokio::time::timeout(Duration::from_secs(1), handle.next_event())
            .await
            .expect("event bound")
            .expect("event");
        let event = if matches!(first, Some(SessionEvent::Terminated { .. })) {
            first
        } else {
            tokio::time::timeout(Duration::from_secs(1), handle.next_event())
                .await
                .expect("terminal bound")
                .expect("terminal event")
        };
        assert!(matches!(event, Some(SessionEvent::Terminated { .. })));
        tokio::time::timeout(Duration::from_secs(1), server_task)
            .await
            .expect("server join bound")
            .expect("server task");
    }
}
