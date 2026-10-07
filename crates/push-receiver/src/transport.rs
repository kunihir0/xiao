use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::BytesMut;
use p256::{
    SecretKey,
    elliptic_curve::{Generate, sec1::ToSec1Point},
};
use prost::Message;
use rand::{RngExt, rng};
use reqwest::{Client, Response};
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::{TlsConnector, client::TlsStream};
use tokio_util::codec::Decoder;
use uuid::Uuid;

use crate::{
    Error, Notification, Result,
    checkin::{
        CheckinCredentials, build_checkin_request, parse_checkin_response,
        parse_registration_response,
    },
    codec::{FrameTag, McsCodec, McsFrame},
    crypto::WebPushKeyMaterial,
    mcs::{
        LoginParameters, decode_data_message_with_metadata, decode_login_response_details,
        encode_heartbeat_ack, encode_heartbeat_ping, encode_login, encode_stream_ack,
    },
    proto,
};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const HEARTBEAT_GRACE: Duration = Duration::from_secs(5);
const STREAM_ACK_THRESHOLD: usize = 10;

fn http_error(error: reqwest::Error) -> Error {
    Error::Http(error.without_url().to_string())
}
const SERVER_KEY: &[u8] = &[
    0x04, 0x33, 0x94, 0xf7, 0xdf, 0xa1, 0xeb, 0xb1, 0xdc, 0x03, 0xa2, 0x5e, 0x15, 0x71, 0xdb, 0x48,
    0xd3, 0x2e, 0xed, 0xed, 0xb2, 0x34, 0xdb, 0xb7, 0x47, 0x3a, 0x0c, 0x8f, 0xc4, 0xcc, 0xe1, 0x6f,
    0x3c, 0x8c, 0x84, 0xdf, 0xab, 0xb6, 0x66, 0x3e, 0xf2, 0x0c, 0xd4, 0x8b, 0xfe, 0xe3, 0xf9, 0x76,
    0x2f, 0x14, 0x1c, 0x63, 0x08, 0x6a, 0x6f, 0x2d, 0xb1, 0x1a, 0x95, 0xb0, 0xce, 0x37, 0xc0, 0x9c,
    0x6e,
];

async fn bounded_body(mut response: Response, maximum: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(Error::FrameTooLarge {
            actual: response
                .content_length()
                .unwrap_or(u64::MAX)
                .min(usize::MAX as u64) as usize,
            max: maximum,
        });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(http_error)? {
        let actual = body.len().saturating_add(chunk.len());
        if actual > maximum {
            return Err(Error::FrameTooLarge {
                actual,
                max: maximum,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A connected, authenticated MCS session. Implementations own their socket
/// and must not spawn work outside the returned futures.
pub trait McsSession: Send {
    fn receive<'a>(&'a mut self) -> BoxFuture<'a, Result<SessionEvent>>;
    fn send_heartbeat<'a>(&'a mut self) -> BoxFuture<'a, Result<()>>;
    fn heartbeat_interval(&self) -> Duration {
        Duration::from_secs(300)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    Notification(Notification),
    Closed,
}

/// Creates authenticated sessions, retaining no raw wire types at the API boundary.
pub trait ReceiverConnector: Send + Sync {
    fn connect<'a>(
        &'a self,
        persistent_ids: &'a [String],
    ) -> BoxFuture<'a, Result<Box<dyn McsSession>>>;
}

#[derive(Clone)]
pub struct FcmConnector {
    client: Client,
    checkin_url: String,
    registration_url: String,
    subscribe_url: String,
    mcs_host: String,
    mcs_port: u16,
    credentials: Arc<tokio::sync::Mutex<Option<CheckinCredentials>>>,
    web_push_keys: Arc<tokio::sync::Mutex<Option<WebPushKeyMaterial>>>,
    sender_id: String,
}

impl std::fmt::Debug for FcmConnector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FcmConnector")
            .field("checkin_url", &"<redacted>")
            .field("registration_url", &"<redacted>")
            .field("subscribe_url", &"<redacted>")
            .field("mcs_host", &self.mcs_host)
            .field("mcs_port", &self.mcs_port)
            .field("credentials", &"<redacted>")
            .field("sender_id", &"<redacted>")
            .finish()
    }
}

impl FcmConnector {
    /// Creates a connector for the GCM/FCM sender ID. Registration tokens are
    /// obtained by the authenticated legacy register3 exchange, not supplied
    /// by callers.
    pub fn new(sender_id: impl Into<String>) -> Result<Self> {
        let sender_id = sender_id.into();
        if sender_id.is_empty() || sender_id.len() > 256 {
            return Err(Error::MissingCredential("sender_id"));
        }
        // reqwest's rustls-no-provider feature requires a process provider.
        // Install the sole enabled backend before Client construction; the MCS
        // client below also passes the same provider explicitly.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Client::builder()
            .https_only(true)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(IO_TIMEOUT)
            .build()
            .map_err(http_error)?;
        Ok(Self {
            client,
            checkin_url: "https://android.clients.google.com/checkin".into(),
            registration_url: "https://android.clients.google.com/c2dm/register3".into(),
            subscribe_url: "https://fcm.googleapis.com/fcm/connect/subscribe".into(),
            mcs_host: "mtalk.google.com".into(),
            mcs_port: 5228,
            credentials: Arc::new(tokio::sync::Mutex::new(None)),
            web_push_keys: Arc::new(tokio::sync::Mutex::new(None)),
            sender_id,
        })
    }

    #[must_use]
    pub fn with_endpoints(
        mut self,
        checkin_url: impl Into<String>,
        registration_url: impl Into<String>,
    ) -> Self {
        self.checkin_url = checkin_url.into();
        self.registration_url = registration_url.into();
        self
    }

    #[must_use]
    pub fn with_subscribe_endpoint(mut self, subscribe_url: impl Into<String>) -> Self {
        self.subscribe_url = subscribe_url.into();
        self
    }

    #[must_use]
    pub fn with_mcs_endpoint(mut self, host: impl Into<String>, port: u16) -> Self {
        self.mcs_host = host.into();
        self.mcs_port = port;
        self
    }

    pub async fn web_push_key_material(&self) -> Option<WebPushKeyMaterial> {
        self.web_push_keys.lock().await.clone()
    }
    async fn ensure_registered(&self) -> Result<CheckinCredentials> {
        let mut cached = self.credentials.lock().await;
        if let Some(credentials) = *cached {
            return Ok(credentials);
        }
        let response = self
            .client
            .post(&self.checkin_url)
            .header(reqwest::header::CONTENT_TYPE, "application/x-protobuf")
            .body(build_checkin_request(None))
            .send()
            .await
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?;
        let body = bounded_body(response, crate::MAX_FRAME_SIZE).await?;
        let result = parse_checkin_response(&body)?;
        let app_id = format!("wp:receiver.push.com#{}", Uuid::new_v4());
        let server_key = URL_SAFE_NO_PAD.encode(SERVER_KEY);
        let form = [
            ("app", "org.chromium.linux".to_owned()),
            ("X-subtype", app_id),
            ("device", result.credentials.android_id().to_string()),
            ("sender", server_key),
        ];
        let registration = self
            .client
            .post(&self.registration_url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!(
                    "AidLogin {}:{}",
                    result.credentials.android_id(),
                    result.credentials.security_token()
                ),
            )
            .form(&form)
            .send()
            .await
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?;
        let registration_body = String::from_utf8(bounded_body(registration, 4096).await?)
            .map_err(|_| Error::InvalidRegistration("registration response is not UTF-8".into()))?;
        let token = parse_registration_response(&registration_body)?;
        let key_material = generate_web_push_keys();
        let subscribe_form = [
            ("authorized_entity", self.sender_id.clone()),
            (
                "endpoint",
                format!("https://fcm.googleapis.com/fcm/send/{}", token.as_str()),
            ),
            ("encryption_key", key_material.public_key),
            ("encryption_auth", key_material.auth_secret),
        ];
        let subscription = self
            .client
            .post(&self.subscribe_url)
            .form(&subscribe_form)
            .send()
            .await
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?;
        let body = bounded_body(subscription, 16 * 1024).await?;
        serde_json::from_slice::<serde_json::Value>(&body).map_err(|error| {
            Error::InvalidRegistration(format!("invalid FCM response: {error}"))
        })?;
        *self.web_push_keys.lock().await = Some(key_material.material);
        *cached = Some(result.credentials);
        Ok(result.credentials)
    }
}

struct WebPushKeys {
    material: WebPushKeyMaterial,
    public_key: String,
    auth_secret: String,
}

fn generate_web_push_keys() -> WebPushKeys {
    let secret = SecretKey::generate();
    let private_key = secret.to_bytes();
    let public_key = URL_SAFE_NO_PAD.encode(secret.public_key().to_sec1_point(false).as_bytes());
    let mut auth_secret = [0u8; 16];
    rng().fill(&mut auth_secret);
    WebPushKeys {
        material: WebPushKeyMaterial::from_bytes(private_key.into(), auth_secret),
        public_key,
        auth_secret: URL_SAFE_NO_PAD.encode(auth_secret),
    }
}

impl ReceiverConnector for FcmConnector {
    fn connect<'a>(
        &'a self,
        persistent_ids: &'a [String],
    ) -> BoxFuture<'a, Result<Box<dyn McsSession>>> {
        Box::pin(async move {
            let credentials = self.ensure_registered().await?;
            let socket = timeout(
                CONNECT_TIMEOUT,
                TcpStream::connect((self.mcs_host.as_str(), self.mcs_port)),
            )
            .await
            .map_err(|_| Error::Timeout("MCS TCP connect"))??;
            let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|_| Error::InvalidState("ring provider lacks safe TLS versions"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
            let server_name = ServerName::try_from(self.mcs_host.clone())
                .map_err(|_| Error::InvalidState("invalid MCS server name"))?;
            let stream = timeout(
                CONNECT_TIMEOUT,
                TlsConnector::from(Arc::new(config)).connect(server_name, socket),
            )
            .await
            .map_err(|_| Error::Timeout("MCS TLS handshake"))?
            .map_err(|error| Error::Tls(error.to_string()))?;
            let mut session = TlsMcsSession::new(stream);
            let login = encode_login(&LoginParameters {
                credentials,
                persistent_ids: persistent_ids.to_vec(),
            })?;
            session
                .send_frame(McsFrame::new(FrameTag::LoginRequest, login))
                .await?;
            let event = session.receive_frame().await?;
            if !matches!(event, FrameEvent::Login) {
                return Err(Error::InvalidState("MCS server did not send LoginResponse"));
            }
            Ok(Box::new(session) as Box<dyn McsSession>)
        })
    }
}
enum FrameEvent {
    Login,
    Data(crate::mcs::DecodedDataMessage),
    HeartbeatPing(Option<i32>),
    HeartbeatAck(Option<i32>),
    Iq(Option<i32>),
    Close,
}

fn read_deadline(heartbeat: Duration) -> Duration {
    heartbeat.saturating_add(HEARTBEAT_GRACE)
}

fn eof_result(decoder: &mut McsCodec, buffer: &mut BytesMut) -> Result<FrameEvent> {
    match decoder.decode_eof(buffer) {
        Ok(None) => Err(Error::TransportClosed),
        Ok(Some(_)) => Err(Error::Protocol(
            "MCS decoder returned a frame at EOF".into(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(Error::TruncatedFrame)
        }
        Err(error) => Err(Error::Decode(error.to_string())),
    }
}

fn next_stream_id(current: i32, observed: Option<i32>) -> i32 {
    observed.unwrap_or_else(|| {
        if current > 0 {
            current.saturating_add(1)
        } else {
            current
        }
    })
}

struct TlsMcsSession {
    stream: TlsStream<TcpStream>,
    decoder: McsCodec,
    encoder: McsCodec,
    buffer: BytesMut,
    heartbeat: Duration,
    last_stream_id_received: i32,
    unacknowledged_messages: usize,
}

impl TlsMcsSession {
    fn new(stream: TlsStream<TcpStream>) -> Self {
        Self {
            stream,
            decoder: McsCodec::server(),
            encoder: McsCodec::client(),
            buffer: BytesMut::new(),
            heartbeat: Duration::from_secs(300),
            last_stream_id_received: 0,
            unacknowledged_messages: 0,
        }
    }
    fn read_timeout(&self) -> Duration {
        read_deadline(self.heartbeat)
    }
    async fn send_frame(&mut self, frame: McsFrame) -> Result<()> {
        let mut bytes = BytesMut::new();
        self.encoder
            .encode_frame(&frame, &mut bytes)
            .map_err(|error| Error::Encode(error.to_string()))?;
        timeout(IO_TIMEOUT, self.stream.write_all(&bytes))
            .await
            .map_err(|_| Error::Timeout("MCS write"))??;
        timeout(IO_TIMEOUT, self.stream.flush())
            .await
            .map_err(|_| Error::Timeout("MCS flush"))??;
        Ok(())
    }
    async fn receive_frame(&mut self) -> Result<FrameEvent> {
        loop {
            if let Some(frame) = self.decoder.decode_frame(&mut self.buffer)? {
                match frame.tag {
                    FrameTag::LoginResponse => {
                        let response = proto_login_response(&frame.payload)?;
                        self.heartbeat = response.heartbeat;
                        self.last_stream_id_received = response.last_stream_id_received;
                        return Ok(FrameEvent::Login);
                    }
                    FrameTag::DataMessage => {
                        return Ok(FrameEvent::Data(decode_data_message_with_metadata(
                            &frame.payload,
                        )?));
                    }
                    FrameTag::HeartbeatPing => {
                        let ping = proto::HeartbeatPing::decode(frame.payload.as_slice())
                            .map_err(|error| Error::Decode(error.to_string()))?;
                        return Ok(FrameEvent::HeartbeatPing(ping.stream_id));
                    }
                    FrameTag::HeartbeatAck => {
                        let ack = proto::HeartbeatAck::decode(frame.payload.as_slice())
                            .map_err(|error| Error::Decode(error.to_string()))?;
                        return Ok(FrameEvent::HeartbeatAck(ack.stream_id));
                    }
                    FrameTag::Iq => {
                        let iq = proto::IqStanza::decode(frame.payload.as_slice())
                            .map_err(|error| Error::Decode(error.to_string()))?;
                        return Ok(FrameEvent::Iq(iq.stream_id));
                    }
                    FrameTag::Close => return Ok(FrameEvent::Close),
                    FrameTag::StreamError => {
                        return Err(Error::Protocol("MCS stream error".into()));
                    }
                    _ => continue,
                }
            }
            let read = timeout(self.read_timeout(), self.stream.read_buf(&mut self.buffer))
                .await
                .map_err(|_| Error::Timeout("MCS read"))??;
            if read == 0 {
                return eof_result(&mut self.decoder, &mut self.buffer);
            }
        }
    }
}

struct LoginResponseConfig {
    heartbeat: Duration,
    last_stream_id_received: i32,
}

fn proto_login_response(payload: &[u8]) -> Result<LoginResponseConfig> {
    let details = decode_login_response_details(payload)?;
    Ok(LoginResponseConfig {
        heartbeat: Duration::from_millis(details.config.heartbeat_millis),
        last_stream_id_received: details
            .last_stream_id_received
            .or(details.stream_id)
            .unwrap_or(1),
    })
}

impl McsSession for TlsMcsSession {
    fn receive<'a>(&'a mut self) -> BoxFuture<'a, Result<SessionEvent>> {
        Box::pin(async move {
            loop {
                let event = self.receive_frame().await?;
                let stream_id = match &event {
                    FrameEvent::Data(data) => data.stream_id,
                    FrameEvent::HeartbeatPing(stream_id)
                    | FrameEvent::HeartbeatAck(stream_id)
                    | FrameEvent::Iq(stream_id) => *stream_id,
                    FrameEvent::Close => None,
                    FrameEvent::Login => None,
                };
                if !matches!(&event, FrameEvent::Login) {
                    self.last_stream_id_received =
                        next_stream_id(self.last_stream_id_received, stream_id);
                }
                match event {
                    FrameEvent::Data(data) => {
                        if data.notification.persistent_id.is_some() {
                            self.unacknowledged_messages =
                                self.unacknowledged_messages.saturating_add(1);
                        }
                        if data.immediate_ack
                            || self.unacknowledged_messages >= STREAM_ACK_THRESHOLD
                        {
                            let payload = encode_stream_ack(self.last_stream_id_received)?;
                            self.send_frame(McsFrame::new(FrameTag::Iq, payload))
                                .await?;
                            self.unacknowledged_messages = 0;
                        }
                        return Ok(SessionEvent::Notification(data.notification));
                    }
                    FrameEvent::HeartbeatPing(_) => {
                        let payload = encode_heartbeat_ack(
                            (self.last_stream_id_received > 0)
                                .then_some(self.last_stream_id_received),
                        )?;
                        self.send_frame(McsFrame::new(FrameTag::HeartbeatAck, payload))
                            .await?;
                    }
                    FrameEvent::HeartbeatAck(_) | FrameEvent::Iq(_) => {}
                    FrameEvent::Close => return Ok(SessionEvent::Closed),
                    FrameEvent::Login => {}
                }
            }
        })
    }
    fn send_heartbeat<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let payload = encode_heartbeat_ping(
                (self.last_stream_id_received > 0).then_some(self.last_stream_id_received),
            )?;
            self.send_frame(McsFrame::new(FrameTag::HeartbeatPing, payload))
                .await
        })
    }
    fn heartbeat_interval(&self) -> Duration {
        self.heartbeat
    }
}

#[derive(Clone)]
pub struct ProductionConfig {
    pub queue_capacity: usize,
    pub dedupe_capacity: usize,
    pub retry: crate::RetryPolicy,
}
impl Default for ProductionConfig {
    fn default() -> Self {
        Self {
            queue_capacity: crate::DEFAULT_QUEUE_CAPACITY,
            dedupe_capacity: crate::DEFAULT_DEDUPE_CAPACITY,
            retry: crate::RetryPolicy::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eof_with_partial_frame_is_a_typed_truncation() {
        let mut decoder = McsCodec::server();
        let mut buffer = BytesMut::from(&[FrameTag::DataMessage as u8, 5, 1][..]);
        assert!(decoder.decode_frame(&mut buffer).is_ok());
        assert!(matches!(
            eof_result(&mut decoder, &mut buffer),
            Err(Error::TruncatedFrame)
        ));
    }

    #[test]
    fn read_deadline_follows_negotiated_heartbeat() {
        assert_eq!(
            read_deadline(Duration::from_secs(45)),
            Duration::from_secs(50)
        );
    }

    #[test]
    fn stream_counter_advances_for_stanzas_without_explicit_ids() {
        assert_eq!(next_stream_id(7, None), 8);
        assert_eq!(next_stream_id(7, Some(12)), 12);
        assert_eq!(next_stream_id(0, None), 0);
    }

    #[test]
    fn ring_provider_can_construct_connector_client() {
        assert!(FcmConnector::new("sender").is_ok());
    }

    #[test]
    fn connector_debug_redacts_custom_endpoint_urls() {
        let connector = FcmConnector::new("sender").ok().map(|connector| {
            connector.with_endpoints(
                "https://secret.example/checkin?token=secret",
                "https://secret.example/register",
            )
        });
        assert!(connector.is_some());
        if let Some(connector) = connector {
            let debug = format!("{connector:?}");
            assert!(!debug.contains("secret.example"));
            assert!(!debug.contains("token=secret"));
        }
    }
}
