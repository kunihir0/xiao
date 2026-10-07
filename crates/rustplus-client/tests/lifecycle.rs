#![allow(clippy::expect_used)]

use futures_util::{SinkExt, StreamExt};
use rustplus_client::{
    ClientConfig, Error, EventReceiveError, MAX_REQUEST_SIZE, Readiness, Request, ResponsePayload,
    RustPlusClient,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, sync::Barrier};
use tokio_tungstenite::{accept_async, tungstenite::Message};

fn fixture(name: &str) -> Vec<u8> {
    include_str!("fixtures/legacy-wire.txt")
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

async fn server(respond: bool) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("listener");
    let endpoint = format!("ws://{}/", listener.local_addr().expect("address"));
    let task = tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut socket) = accept_async(stream).await else {
            return;
        };
        let mut confirmed = false;
        while let Some(Ok(message)) = socket.next().await {
            if let Message::Binary(data) = &message
                && (respond || !confirmed)
            {
                let mut response = fixture("response_success");
                if let Some(sequence) = data.get(1) {
                    response[3] = *sequence;
                }
                let _ = socket.send(Message::Binary(response.into())).await;
                confirmed = true;
            }
            if matches!(&message, Message::Close(_)) {
                break;
            }
        }
    });
    (endpoint, task)
}

async fn event_burst_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("listener");
    let endpoint = format!("ws://{}/", listener.local_addr().expect("address"));
    let task = tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut socket) = accept_async(stream).await else {
            return;
        };
        let mut requests = 0;
        while let Some(Ok(Message::Binary(data))) = socket.next().await {
            let mut response = fixture("response_success");
            if let Some(sequence) = data.get(1) {
                response[3] = *sequence;
            }
            let _ = socket.send(Message::Binary(response.into())).await;
            requests += 1;
            let count = if requests == 1 { 4 } else { 1 };
            for _ in 0..count {
                let _ = socket
                    .send(Message::Binary(fixture("event_camera").into()))
                    .await;
            }
        }
    });
    (endpoint, task)
}

async fn generation_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("listener");
    let endpoint = format!("ws://{}/", listener.local_addr().expect("address"));
    let task = tokio::spawn(async move {
        for connection in 0..2 {
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
            let Some(Ok(Message::Binary(data))) = socket.next().await else {
                return;
            };
            if connection == 0 {
                let _ = socket.close(None).await;
                continue;
            }
            let Some(sequence) = data.get(1).copied() else {
                return;
            };
            let mut stale = fixture("response_success");
            stale[3] = 2;
            let _ = socket.send(Message::Binary(stale.into())).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut current = fixture("response_success");
            current[3] = sequence;
            let _ = socket.send(Message::Binary(current.into())).await;
        }
    });
    (endpoint, task)
}

async fn closing_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("listener");
    let endpoint = format!("ws://{}/", listener.local_addr().expect("address"));
    let task = tokio::spawn(async move {
        for connection in 0..2 {
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
            if connection == 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let _ = socket.close(None).await;
            } else if let Some(Ok(Message::Binary(data))) = socket.next().await {
                let mut response = fixture("response_success");
                if let Some(sequence) = data.get(1) {
                    response[3] = *sequence;
                }
                let _ = socket.send(Message::Binary(response.into())).await;
            }
        }
    });
    (endpoint, task)
}
async fn delayed_handshake_server(delay: Duration) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("listener");
    let endpoint = format!("ws://{}/", listener.local_addr().expect("address"));
    let task = tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut socket) = accept_async(stream).await else {
            return;
        };
        while let Some(Ok(message)) = socket.next().await {
            if let Message::Binary(data) = &message {
                tokio::time::sleep(delay).await;
                let mut response = fixture("response_success");
                if let Some(sequence) = data.get(1) {
                    response[3] = *sequence;
                }
                let _ = socket.send(Message::Binary(response.into())).await;
                continue;
            }
        }
    });
    (endpoint, task)
}

async fn stalled_handshake_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("listener");
    let endpoint = format!("ws://{}/", listener.local_addr().expect("address"));
    let task = tokio::spawn(async move {
        let Ok((_stream, _)) = listener.accept().await else {
            return;
        };
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    (endpoint, task)
}

async fn child_error_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("listener");
    let endpoint = format!("ws://{}/", listener.local_addr().expect("address"));
    let task = tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut socket) = accept_async(stream).await else {
            return;
        };
        if let Some(Ok(Message::Binary(data))) = socket.next().await {
            let mut response = fixture("response_success");
            if let Some(sequence) = data.get(1) {
                response[3] = *sequence;
            }
            let _ = socket.send(Message::Binary(response.into())).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = socket.send(Message::Binary(vec![0x0a, 0x80].into())).await;
        }
    });
    (endpoint, task)
}

async fn flapping_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("listener");
    let endpoint = format!("ws://{}/", listener.local_addr().expect("address"));
    let task = tokio::spawn(async move {
        for _ in 0..2 {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut socket) = accept_async(stream).await else {
                return;
            };
            if let Some(Ok(Message::Binary(data))) = socket.next().await {
                let mut response = fixture("response_success");
                if let Some(sequence) = data.get(1) {
                    response[3] = *sequence;
                }
                let _ = socket.send(Message::Binary(response.into())).await;
            }
        }
    });
    (endpoint, task)
}

async fn repeated_server(connections: usize) -> (String, tokio::task::JoinHandle<()>) {
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
            while let Some(Ok(message)) = socket.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        }
    });
    (endpoint, task)
}

#[tokio::test]

async fn concurrent_connect_disconnect_always_ends_disconnected() {
    let (endpoint, task) = repeated_server(50).await;
    let client = RustPlusClient::new(ClientConfig::new(endpoint, 42, 7).allow_insecure_for_tests())
        .expect("client");
    for _ in 0..50 {
        let barrier = Arc::new(Barrier::new(2));
        let connecting = {
            let client = client.clone();
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                client.connect().await
            })
        };
        barrier.wait().await;
        let disconnecting = client.disconnect();
        let (connect_result, disconnect_result) = tokio::join!(connecting, disconnecting);
        let connect_result = connect_result.expect("connect task");
        assert!(matches!(
            connect_result,
            Ok(())
                | Err(Error::AlreadyConnecting
                    | Error::ShuttingDown
                    | Error::Transport
                    | Error::Disconnected)
        ));
        assert!(disconnect_result.is_ok() || matches!(disconnect_result, Err(Error::Transport)));
        assert_eq!(client.readiness(), Readiness::Disconnected);
    }
    task.abort();
}
#[tokio::test]
async fn concurrent_connects_keep_one_owned_transport() {
    let (endpoint, task) = repeated_server(50).await;
    let client = RustPlusClient::new(ClientConfig::new(endpoint, 42, 7).allow_insecure_for_tests())
        .expect("client");
    for _ in 0..50 {
        let barrier = Arc::new(Barrier::new(2));
        let first = {
            let client = client.clone();
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                client.connect().await
            })
        };
        barrier.wait().await;
        let second = client.connect().await;
        let first = first.await.expect("first connect task");
        assert!(
            (first.is_ok() && matches!(second, Err(Error::AlreadyConnecting)))
                || (matches!(first, Err(Error::AlreadyConnecting)) && second.is_ok())
        );
        assert_eq!(client.readiness(), Readiness::Ready);
        client.disconnect().await.expect("disconnect");
        assert_eq!(client.readiness(), Readiness::Disconnected);
    }
    task.abort();
}

#[tokio::test]
async fn readiness_request_and_bounded_disconnect_use_owned_transport() {
    let (endpoint, task) = server(true).await;
    let client = RustPlusClient::new(ClientConfig::new(endpoint, 42, 7).allow_insecure_for_tests())
        .expect("client");
    assert_eq!(client.readiness(), Readiness::Disconnected);
    client.connect().await.expect("connect");
    assert_eq!(client.readiness(), Readiness::Ready);
    let response = client.send_request(Request::Info).await.expect("response");
    assert!(matches!(response.payload, ResponsePayload::Success));
    client.disconnect().await.expect("disconnect");
    assert_eq!(client.readiness(), Readiness::Disconnected);
    task.abort();
}

#[tokio::test]
async fn dropping_last_owner_cancels_tasks_and_publishes_terminal_readiness() {
    let (endpoint, task) = server(true).await;
    let client = RustPlusClient::new(ClientConfig::new(endpoint, 42, 7).allow_insecure_for_tests())
        .expect("client");
    client.connect().await.expect("connect");
    let mut readiness = client.subscribe_readiness();
    let owner = client.clone();
    drop(client);
    drop(owner);
    tokio::time::timeout(Duration::from_millis(500), readiness.changed())
        .await
        .expect("drop readiness must be bounded")
        .expect("readiness channel");
    assert_eq!(*readiness.borrow(), Readiness::Disconnected);
    task.abort();
}

#[tokio::test]
async fn broadcast_lag_is_explicit_and_resubscribe_continues() {
    let (endpoint, task) = event_burst_server().await;
    let mut config = ClientConfig::new(endpoint, 42, 7).allow_insecure_for_tests();
    config.event_capacity = 2;
    let client = RustPlusClient::new(config).expect("client");
    let mut events = client.subscribe_events();
    client.connect().await.expect("connect");
    let first = events.recv().await;
    assert!(matches!(first, Err(EventReceiveError::Lagged { skipped }) if skipped > 0));
    let mut replacement = client.subscribe_events();
    client
        .send_request(Request::Info)
        .await
        .expect("second request");
    assert!(replacement.recv().await.is_ok());
    client.disconnect().await.expect("disconnect");
    task.abort();
}

#[tokio::test]
async fn readiness_stays_connecting_until_live_confirmation() {
    let (endpoint, task) = delayed_handshake_server(Duration::from_millis(100)).await;
    let client = RustPlusClient::new(ClientConfig::new(endpoint, 42, 7).allow_insecure_for_tests())
        .expect("client");
    let connecting = {
        let client = client.clone();
        tokio::spawn(async move { client.connect().await })
    };
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(client.readiness(), Readiness::Connecting);
    connecting.await.expect("connect task").expect("connect");
    assert_eq!(client.readiness(), Readiness::Ready);
    client.disconnect().await.expect("disconnect");
    task.abort();
}

#[tokio::test]
async fn disconnect_cancels_stalled_handshake_with_terminal_readiness() {
    let (endpoint, task) = stalled_handshake_server().await;
    let client = RustPlusClient::new(ClientConfig::new(endpoint, 42, 7).allow_insecure_for_tests())
        .expect("client");
    let connecting = {
        let client = client.clone();
        tokio::spawn(async move { client.connect().await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(client.readiness(), Readiness::Connecting);
    let started = Instant::now();
    let disconnected = tokio::time::timeout(Duration::from_millis(500), client.disconnect())
        .await
        .expect("disconnect must be bounded");
    assert!(disconnected.is_ok());
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(client.readiness(), Readiness::Disconnected);
    let connect_result = tokio::time::timeout(Duration::from_millis(500), connecting)
        .await
        .expect("connect task must stop")
        .expect("connect task join");
    assert!(matches!(connect_result, Err(Error::ShuttingDown)));
    task.abort();
}

#[test]
fn new_rejects_invalid_configuration_before_allocating_channels() {
    let mut config = ClientConfig::new("wss://example.invalid", 42, 7);
    config.rate_policy.capacity = 0;
    assert!(matches!(
        RustPlusClient::new(config),
        Err(Error::InvalidConfig(_))
    ));
}

#[tokio::test]
async fn malformed_child_error_is_surfaced_by_reconnect() {
    let (endpoint, task) = child_error_server().await;
    let mut config = ClientConfig::new(endpoint, 42, 7).allow_insecure_for_tests();
    config.reconnect.max_attempts = 0;
    let client = RustPlusClient::new(config).expect("client");
    let error = tokio::time::timeout(Duration::from_secs(1), client.connect_with_reconnect())
        .await
        .expect("reconnect must terminate")
        .expect_err("malformed child must surface");
    assert!(matches!(error, Error::Decode));
    task.abort();
}

#[tokio::test]
async fn disconnect_surfaces_child_error_and_keeps_terminal_readiness() {
    let (endpoint, task) = child_error_server().await;
    let client = RustPlusClient::new(ClientConfig::new(endpoint, 42, 7).allow_insecure_for_tests())
        .expect("client");
    client.connect().await.expect("connect");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let result = client.disconnect().await;
    assert!(matches!(
        result,
        Err(Error::Decode | Error::Transport | Error::Disconnected | Error::ShuttingDown)
    ));
    assert_eq!(client.readiness(), Readiness::Disconnected);
    task.abort();
}

#[tokio::test]
async fn stale_response_from_previous_generation_cannot_resolve_new_waiter() {
    let (endpoint, task) = generation_server().await;
    let client = RustPlusClient::new(ClientConfig::new(endpoint, 42, 7).allow_insecure_for_tests())
        .expect("client");
    client.connect().await.expect("first connect");
    let first = {
        let client = client.clone();
        tokio::spawn(async move { client.send_request(Request::Info).await })
    };
    let first_result = tokio::time::timeout(Duration::from_secs(1), first)
        .await
        .expect("old waiter must finish")
        .expect("old waiter task");
    assert!(matches!(first_result, Err(Error::Disconnected)));
    assert_eq!(client.readiness(), Readiness::Disconnected);
    client.connect().await.expect("second connect");
    let mut current = Box::pin(client.send_request(Request::Info));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut current)
            .await
            .is_err(),
        "stale response must not resolve the current waiter"
    );
    let response = current.await.expect("current response");
    assert!(matches!(response.payload, ResponsePayload::Success));
    assert!(matches!(
        client.disconnect().await,
        Ok(()) | Err(Error::Transport)
    ));
    task.abort();
}

#[tokio::test]
async fn transport_send_failure_cleans_pending_before_next_generation() {
    let (endpoint, task) = closing_server().await;
    let mut config = ClientConfig::new(endpoint, 42, 7).allow_insecure_for_tests();
    config.max_pending = 1;
    let client = RustPlusClient::new(config).expect("client");
    client.connect().await.expect("first connect");
    let failed = client.send_request(Request::Info).await;
    assert!(matches!(
        failed,
        Err(Error::Disconnected | Error::Transport | Error::ShuttingDown)
    ));
    client.connect().await.expect("second connect");
    let response = client
        .send_request(Request::Info)
        .await
        .expect("next request must be admitted");
    assert!(matches!(response.payload, ResponsePayload::Success));
    assert!(matches!(
        client.disconnect().await,
        Ok(()) | Err(Error::Transport)
    ));
    task.abort();
}

#[tokio::test]
async fn oversized_outbound_payloads_are_rejected_before_enqueue() {
    let (endpoint, task) = server(true).await;
    let client = RustPlusClient::new(ClientConfig::new(endpoint, 42, 7).allow_insecure_for_tests())
        .expect("client");
    client.connect().await.expect("connect");
    let oversized = "x".repeat(MAX_REQUEST_SIZE);
    for request in [
        Request::TeamMessage(oversized.clone()),
        Request::SetClanMotd(oversized.clone()),
        Request::NexusAuth(oversized.clone()),
        Request::CameraSubscribe(oversized),
    ] {
        assert!(matches!(
            client.send_request(request).await,
            Err(Error::RequestTooLarge)
        ));
    }
    client.disconnect().await.expect("disconnect");
    task.abort();
}

#[tokio::test]
async fn reconnects_after_established_loss_with_hard_attempt_cap() {
    let (endpoint, task) = flapping_server().await;
    let mut config = ClientConfig::new(endpoint, 42, 7).allow_insecure_for_tests();
    config.reconnect.max_attempts = 1;
    config.reconnect.initial_delay = Duration::from_millis(1);
    config.reconnect.max_delay = Duration::from_millis(2);
    let client = RustPlusClient::new(config).expect("client");
    let result = tokio::time::timeout(Duration::from_secs(8), client.connect_with_reconnect())
        .await
        .expect("reconnect attempts must be bounded");
    assert!(result.is_err());
    task.abort();
}

#[tokio::test]
async fn timeout_removes_pending_and_max_pending_rejects_admission() {
    let (endpoint, task) = server(false).await;
    let mut config = ClientConfig::new(endpoint, 42, 7).allow_insecure_for_tests();
    config.request_timeout = Duration::from_millis(20);
    config.max_pending = 1;
    let client = RustPlusClient::new(config).expect("client");
    client.connect().await.expect("connect");
    let first_client = client.clone();
    let first = tokio::spawn(async move { first_client.send_request(Request::Info).await });
    tokio::task::yield_now().await;
    let second = client.send_request(Request::Info).await;
    assert!(matches!(second, Err(Error::PendingFull)));
    let first = first.await.expect("first request task");
    assert!(matches!(first, Err(Error::Timeout(_))));
    let after_timeout = client.send_request(Request::Info).await;
    assert!(matches!(after_timeout, Err(Error::Timeout(_))));
    client.disconnect().await.expect("disconnect");
    task.abort();
}
