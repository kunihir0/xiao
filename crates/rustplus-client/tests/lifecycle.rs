#![allow(clippy::expect_used)]

use futures_util::{SinkExt, StreamExt};
use rustplus_client::{ClientConfig, Error, Readiness, Request, ResponsePayload, RustPlusClient};
use std::time::Duration;
use tokio::net::TcpListener;
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

async fn delayed_handshake_server(
    delay: Duration,
) -> (String, tokio::task::JoinHandle<()>) {
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
            let _ = socket
                .send(Message::Binary(vec![0x0a, 0x80].into()))
                .await;
        }
    });
    (endpoint, task)
}

#[tokio::test]
async fn readiness_request_and_bounded_disconnect_use_owned_transport() {
    let (endpoint, task) = server(true).await;
    let client = RustPlusClient::new(ClientConfig::new(endpoint, 42, 7).allow_insecure_for_tests());
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
async fn readiness_stays_connecting_until_live_confirmation() {
    let (endpoint, task) = delayed_handshake_server(Duration::from_millis(100)).await;
    let client =
        RustPlusClient::new(ClientConfig::new(endpoint, 42, 7).allow_insecure_for_tests());
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
async fn malformed_child_error_is_surfaced_by_reconnect() {
    let (endpoint, task) = child_error_server().await;
    let mut config = ClientConfig::new(endpoint, 42, 7).allow_insecure_for_tests();
    config.reconnect.max_attempts = 0;
    let client = RustPlusClient::new(config);
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        client.connect_with_reconnect(),
    )
    .await
    .expect("reconnect must terminate")
    .expect_err("malformed child must surface");
    assert!(matches!(error, Error::Decode));
    task.abort();
}

#[tokio::test]
async fn timeout_removes_pending_and_max_pending_rejects_admission() {
    let (endpoint, task) = server(false).await;
    let mut config = ClientConfig::new(endpoint, 42, 7).allow_insecure_for_tests();
    config.request_timeout = Duration::from_millis(20);
    config.max_pending = 1;
    let client = RustPlusClient::new(config);
    client.connect().await.expect("connect");
    let first_client = client.clone();
    let first = tokio::spawn(async move { first_client.send_request(Request::Info).await });
    tokio::task::yield_now().await;
    let second = client.send_request(Request::Info).await;
    assert!(matches!(second, Err(Error::PendingFull)));
    let first = first.await.expect("first request task");
    assert!(matches!(first, Err(Error::Timeout(_))));
    client.disconnect().await.expect("disconnect");
    task.abort();
}
