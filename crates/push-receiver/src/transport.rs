use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use bytes::BytesMut;
use reqwest::Client;
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_rustls::{TlsConnector, client::TlsStream};

use crate::{
    Error, Notification, Result,
    checkin::{
        CheckinCredentials, build_checkin_request, parse_checkin_response,
        parse_registration_response,
    },
    codec::{FrameTag, McsCodec, McsFrame},
    mcs::{
        LoginParameters, decode_data_message, decode_login_response, encode_heartbeat_ack,
        encode_login,
    },
};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A connected, authenticated MCS session. Implementations own their socket and
/// must not spawn work outside the returned futures.
pub trait McsSession: Send {
    fn receive<'a>(&'a mut self) -> BoxFuture<'a, Result<SessionEvent>>;
    fn send_heartbeat<'a>(&'a mut self) -> BoxFuture<'a, Result<()>>;
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
    mcs_host: String,
    mcs_port: u16,
    credentials: Option<CheckinCredentials>,
    registration_token: String,
}

impl std::fmt::Debug for FcmConnector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FcmConnector")
            .field("checkin_url", &self.checkin_url)
            .field("registration_url", &self.registration_url)
            .field("mcs_host", &self.mcs_host)
            .field("mcs_port", &self.mcs_port)
            .field("credentials", &self.credentials)
            .field("registration_token", &"<redacted>")
            .finish()
    }
}

impl FcmConnector {
    pub fn new(registration_token: impl Into<String>) -> Result<Self> {
        let registration_token = registration_token.into();
        if registration_token.is_empty() {
            return Err(Error::MissingCredential("registration_token"));
        }
        let client = Client::builder()
            .https_only(true)
            .build()
            .map_err(|error| Error::Http(error.to_string()))?;
        Ok(Self {
            client,
            checkin_url: "https://android.clients.google.com/checkin".into(),
            registration_url: "https://android.clients.google.com/c2dm/register3".into(),
            mcs_host: "mtalk.google.com".into(),
            mcs_port: 5228,
            credentials: None,
            registration_token,
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
    pub fn with_mcs_endpoint(mut self, host: impl Into<String>, port: u16) -> Self {
        self.mcs_host = host.into();
        self.mcs_port = port;
        self
    }

    async fn ensure_registered(&mut self) -> Result<CheckinCredentials> {
        if let Some(credentials) = self.credentials {
            return Ok(credentials);
        }
        let response = self
            .client
            .post(&self.checkin_url)
            .header(reqwest::header::CONTENT_TYPE, "application/x-protobuf")
            .body(build_checkin_request(None))
            .send()
            .await
            .map_err(|error| Error::Http(error.to_string()))?
            .error_for_status()
            .map_err(|error| Error::Http(error.to_string()))?;
        let body = response
            .bytes()
            .await
            .map_err(|error| Error::Http(error.to_string()))?;
        let result = parse_checkin_response(&body)?;
        let form = [
            ("app", "org.chromium.linux".to_owned()),
            ("device", result.credentials.android_id().to_string()),
            ("sender", String::new()),
            ("cert", String::new()),
            ("scope", "*".to_owned()),
            ("X-subtype", self.registration_token.clone()),
            ("X-app_ver", "1".to_owned()),
            ("X-osv", "1".to_owned()),
            ("token", self.registration_token.clone()),
        ];
        let registration = self
            .client
            .post(&self.registration_url)
            .form(&form)
            .send()
            .await
            .map_err(|error| Error::Http(error.to_string()))?
            .error_for_status()
            .map_err(|error| Error::Http(error.to_string()))?;
        let registration_body = registration
            .text()
            .await
            .map_err(|error| Error::Http(error.to_string()))?;
        parse_registration_response(&registration_body)?;
        self.credentials = Some(result.credentials);
        Ok(result.credentials)
    }
}

impl ReceiverConnector for FcmConnector {
    fn connect<'a>(
        &'a self,
        persistent_ids: &'a [String],
    ) -> BoxFuture<'a, Result<Box<dyn McsSession>>> {
        Box::pin(async move {
            // Clone the small connector state so concurrent reconnect attempts cannot race credentials.
            let mut connector = self.clone();
            let credentials = connector.ensure_registered().await?;
            let socket =
                TcpStream::connect((connector.mcs_host.as_str(), connector.mcs_port)).await?;
            let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let server_name = ServerName::try_from(connector.mcs_host.clone())
                .map_err(|_| Error::InvalidState("invalid MCS server name"))?;
            let stream = TlsConnector::from(Arc::new(config))
                .connect(server_name, socket)
                .await
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
    Data(Notification),
    Heartbeat,
}

struct TlsMcsSession {
    stream: TlsStream<TcpStream>,
    decoder: McsCodec,
    encoder: McsCodec,
    buffer: BytesMut,
    heartbeat: Duration,
}

impl TlsMcsSession {
    fn new(stream: TlsStream<TcpStream>) -> Self {
        Self {
            stream,
            decoder: McsCodec::server(),
            encoder: McsCodec::client(),
            buffer: BytesMut::new(),
            heartbeat: Duration::from_secs(300),
        }
    }
    async fn send_frame(&mut self, frame: McsFrame) -> Result<()> {
        let mut bytes = BytesMut::new();
        self.encoder
            .encode_frame(&frame, &mut bytes)
            .map_err(|error| Error::Encode(error.to_string()))?;
        self.stream.write_all(&bytes).await?;
        self.stream.flush().await?;
        Ok(())
    }
    async fn receive_frame(&mut self) -> Result<FrameEvent> {
        loop {
            if let Some(frame) = self.decoder.decode_frame(&mut self.buffer)? {
                match frame.tag {
                    FrameTag::LoginResponse => {
                        let config = decode_login_response(&frame.payload)?;
                        self.heartbeat = Duration::from_millis(config.heartbeat_millis);
                        return Ok(FrameEvent::Login);
                    }
                    FrameTag::DataMessage => {
                        return Ok(FrameEvent::Data(decode_data_message(&frame.payload)?));
                    }
                    FrameTag::HeartbeatPing => return Ok(FrameEvent::Heartbeat),
                    FrameTag::Close => return Ok(FrameEvent::Heartbeat),
                    _ => continue,
                }
            }
            let read = self.stream.read_buf(&mut self.buffer).await?;
            if read == 0 {
                return Err(Error::TransportClosed);
            }
        }
    }
}

impl McsSession for TlsMcsSession {
    fn receive<'a>(&'a mut self) -> BoxFuture<'a, Result<SessionEvent>> {
        Box::pin(async move {
            loop {
                match self.receive_frame().await? {
                    FrameEvent::Data(notification) => {
                        return Ok(SessionEvent::Notification(notification));
                    }
                    FrameEvent::Heartbeat => self.send_heartbeat().await?,
                    FrameEvent::Login => {}
                }
            }
        })
    }
    fn send_heartbeat<'a>(&'a mut self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let payload = encode_heartbeat_ack(None)?;
            self.send_frame(McsFrame::new(FrameTag::HeartbeatAck, payload))
                .await
        })
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
