//! Facepunch companion registration and credential conversion.
//!
//! The request shapes and ordering follow the upstream companion clients:
//! - <https://github.com/liamcottle/rustplus.js/blob/master/cli/index.js>
//!   (`getExpoPushToken` and `registerWithRustPlus`)
//! - <https://github.com/liamcottle/rustplus.js/blob/master/docs/PairingFlow.md>
//! - <https://github.com/HandyS11/RustPlusApi> (`RustCompanionClient.cs`)
//!
//! The upstream JavaScript client now obtains an FCM token through Android FCM
//! (Firebase Installations, sender `976529667804`), while this crate uses the
//! legacy `register3` flow in `FcmConnector`. Acceptance of that legacy token
//! by Expo has not been verified with live credentials.
//!
//! Registration deliberately has no internal retry loop. Facepunch treats a
//! registration for the same device ID as route replacement, so the operation
//! is idempotent; startup orchestration retries by restarting the operator.

use std::{fmt, path::PathBuf, time::Duration};

use bot_application::{
    BoxFuture, CompanionRegistrar, DeviceId, FcmSecrets, RegistrationError, SteamAuthToken,
    SteamAuthTokenSource,
};
use push_receiver::{
    CheckinCredentials, FcmConnector, FcmRegistrationSnapshot, RegistrationToken,
    WebPushKeyMaterial,
};
use reqwest::{Client, Response, StatusCode};
use serde_json::Value;
use tokio::{fs, io::AsyncReadExt};
use url::Url;
use zeroize::Zeroizing;

const EXPO_PRODUCTION_URL: &str = "https://exp.host/--/api/v2/push/getExpoPushToken";
const FACEPUNCH_PRODUCTION_URL: &str = "https://companion-rust.facepunch.com/api/push/register";
const EXPO_APP_ID: &str = "com.facepunch.rust.companion";
const EXPO_PROJECT_ID: &str = "49451aca-a822-41e6-ad59-955718d0ff9c";
const MAX_RESPONSE_BODY: usize = 16 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// HTTPS endpoints for Expo and Facepunch companion registration.
#[derive(Clone, Eq, PartialEq)]
pub struct CompanionEndpoints {
    expo_url: String,
    facepunch_url: String,
    plaintext_loopback: bool,
}

impl fmt::Debug for CompanionEndpoints {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompanionEndpoints")
            .field("expo_url", &"<redacted>")
            .field("facepunch_url", &"<redacted>")
            .finish()
    }
}

impl CompanionEndpoints {
    /// The production Expo and Facepunch registration services.
    #[must_use]
    pub fn production() -> Self {
        Self {
            expo_url: EXPO_PRODUCTION_URL.to_owned(),
            facepunch_url: FACEPUNCH_PRODUCTION_URL.to_owned(),
            plaintext_loopback: false,
        }
    }

    /// Creates HTTPS endpoints. Plaintext endpoints are never accepted here.
    pub fn new(
        expo_url: impl Into<String>,
        facepunch_url: impl Into<String>,
    ) -> Result<Self, RegistrationError> {
        let expo_url = expo_url.into();
        let facepunch_url = facepunch_url.into();
        validate_endpoint(&expo_url, false)?;
        validate_endpoint(&facepunch_url, false)?;
        Ok(Self {
            expo_url,
            facepunch_url,
            plaintext_loopback: false,
        })
    }

    /// Builds loopback HTTP endpoints for deterministic unit/integration tests.
    ///
    /// This method is compiled only for tests or with the explicit
    /// `insecure-test-transport` feature. The base must be `http://localhost`,
    /// `http://127.0.0.1`, or `http://[::1]`; the two service paths are appended
    /// as `/expo` and `/facepunch`.
    #[cfg(any(test, feature = "insecure-test-transport"))]
    pub fn loopback_for_tests(base: &str) -> Result<Self, RegistrationError> {
        validate_endpoint(base, true)?;
        let base = base.trim_end_matches('/');
        Self::new_for_loopback(format!("{base}/expo"), format!("{base}/facepunch"))
    }

    #[cfg(any(test, feature = "insecure-test-transport"))]
    fn new_for_loopback(
        expo_url: String,
        facepunch_url: String,
    ) -> Result<Self, RegistrationError> {
        validate_endpoint(&expo_url, true)?;
        validate_endpoint(&facepunch_url, true)?;
        Ok(Self {
            expo_url,
            facepunch_url,
            plaintext_loopback: true,
        })
    }

    fn expo_url(&self) -> &str {
        &self.expo_url
    }

    fn facepunch_url(&self) -> &str {
        &self.facepunch_url
    }
}

fn validate_endpoint(value: &str, allow_loopback: bool) -> Result<(), RegistrationError> {
    let parsed =
        Url::parse(value).map_err(|_| RegistrationError::InvalidResponse("invalid endpoint"))?;
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.query().is_some() {
        return Err(RegistrationError::InvalidResponse("invalid endpoint"));
    }
    if parsed.scheme() == "https" && parsed.host_str().is_some() {
        return Ok(());
    }
    if allow_loopback
        && parsed.scheme() == "http"
        && parsed.host_str().is_some_and(|host| {
            host == "localhost" || host == "127.0.0.1" || host == "[::1]" || host == "::1"
        })
    {
        return Ok(());
    }
    Err(RegistrationError::InvalidResponse("insecure endpoint"))
}

/// Converts a complete push-receiver snapshot into application-owned secrets.
pub fn fcm_secrets_from_snapshot(
    snapshot: &FcmRegistrationSnapshot,
) -> Result<FcmSecrets, RegistrationError> {
    let private_key =
        snapshot
            .keys
            .private_key_bytes()
            .ok_or(RegistrationError::InvalidResponse(
                "incomplete Web Push key material",
            ))?;
    let auth_secret =
        snapshot
            .keys
            .auth_secret_bytes()
            .ok_or(RegistrationError::InvalidResponse(
                "incomplete Web Push key material",
            ))?;
    FcmSecrets::new(
        snapshot.checkin.android_id(),
        snapshot.checkin.security_token(),
        snapshot.registration_token.as_str().to_owned(),
        private_key,
        auth_secret,
    )
    .map_err(|_| RegistrationError::InvalidResponse("invalid FCM registration snapshot"))
}

/// Converts persisted application-owned secrets into a push-receiver snapshot.
pub fn snapshot_from_fcm_secrets(
    secrets: &FcmSecrets,
) -> Result<FcmRegistrationSnapshot, RegistrationError> {
    let checkin = CheckinCredentials::new(secrets.android_id(), secrets.security_token())
        .map_err(|_| RegistrationError::InvalidResponse("invalid FCM check-in credentials"))?;
    let registration_token = RegistrationToken::new(secrets.registration_token().to_owned())
        .map_err(|_| RegistrationError::InvalidResponse("invalid FCM registration token"))?;
    let keys = WebPushKeyMaterial::from_bytes(
        *secrets.web_push_private_key(),
        *secrets.web_push_auth_secret(),
    );
    Ok(FcmRegistrationSnapshot {
        checkin,
        registration_token,
        keys,
    })
}

/// Registers FCM credentials with Expo and Facepunch.
pub struct FacepunchRegistrar {
    fcm_sender_id: String,
    endpoints: CompanionEndpoints,
    client: Client,
    timeout: Duration,
}

impl fmt::Debug for FacepunchRegistrar {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FacepunchRegistrar")
            .field("fcm_sender_id", &"<redacted>")
            .field("endpoints", &self.endpoints)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl FacepunchRegistrar {
    pub fn new(
        fcm_sender_id: String,
        endpoints: CompanionEndpoints,
    ) -> Result<Self, RegistrationError> {
        Self::build(fcm_sender_id, endpoints, DEFAULT_TIMEOUT)
    }

    #[cfg(any(test, feature = "insecure-test-transport"))]
    pub fn new_with_timeout(
        fcm_sender_id: String,
        endpoints: CompanionEndpoints,
        timeout: Duration,
    ) -> Result<Self, RegistrationError> {
        if timeout.is_zero() {
            return Err(RegistrationError::InvalidResponse(
                "timeout must be positive",
            ));
        }
        Self::build(fcm_sender_id, endpoints, timeout)
    }

    fn build(
        fcm_sender_id: String,
        endpoints: CompanionEndpoints,
        timeout: Duration,
    ) -> Result<Self, RegistrationError> {
        if fcm_sender_id.is_empty() || fcm_sender_id.len() > 256 {
            return Err(RegistrationError::InvalidResponse("invalid FCM sender ID"));
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let builder = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT.min(timeout))
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy();
        // Only endpoints built by `loopback_for_tests` may use plaintext; the
        // production client is HTTPS-only even when the test feature is
        // compiled in (for example `--all-features` CI builds).
        let builder = builder.https_only(!endpoints.plaintext_loopback);
        let client = builder
            .build()
            .map_err(|_| RegistrationError::Transport("client"))?;
        Ok(Self {
            fcm_sender_id,
            endpoints,
            client,
            timeout,
        })
    }

    async fn send_json(
        &self,
        service: &'static str,
        url: &str,
        body: Vec<u8>,
    ) -> Result<Response, RegistrationError> {
        self.client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|error| map_http_error(service, error))
    }

    async fn bounded_body(
        &self,
        service: &'static str,
        mut response: Response,
    ) -> Result<Vec<u8>, RegistrationError> {
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BODY as u64)
        {
            return Err(RegistrationError::InvalidResponse(
                "response body too large",
            ));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| map_http_error(service, error))?
        {
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BODY {
                return Err(RegistrationError::InvalidResponse(
                    "response body too large",
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    async fn register_companion(
        &self,
        auth_token: &SteamAuthToken,
        device_id: &DeviceId,
        fcm: &FcmSecrets,
    ) -> Result<(), RegistrationError> {
        let device_token = Zeroizing::new(fcm.registration_token().to_owned());
        let expo_request = serde_json::json!({
            "type": "fcm",
            "deviceId": device_id.as_str(),
            "development": false,
            "appId": EXPO_APP_ID,
            "deviceToken": device_token.as_str(),
            "projectId": EXPO_PROJECT_ID,
        });
        let expo_body = serde_json::to_vec(&expo_request)
            .map_err(|_| RegistrationError::InvalidResponse("could not encode Expo request"))?;
        let expo_response = self
            .send_json("expo", self.endpoints.expo_url(), expo_body)
            .await?;
        let expo_status = expo_response.status();
        let expo_body = self.bounded_body("expo", expo_response).await?;
        if !expo_status.is_success() {
            return Err(status_error("expo", expo_status));
        }
        let expo_token = parse_expo_token(&expo_body)?;
        let expo_token = Zeroizing::new(expo_token);
        let steam_token = Zeroizing::new(auth_token.expose_secret().to_owned());
        let facepunch_request = serde_json::json!({
            "AuthToken": steam_token.as_str(),
            "DeviceId": device_id.as_str(),
            "PushKind": 3,
            "PushToken": expo_token.as_str(),
        });
        let facepunch_body = serde_json::to_vec(&facepunch_request).map_err(|_| {
            RegistrationError::InvalidResponse("could not encode Facepunch request")
        })?;
        let facepunch_response = self
            .send_json("facepunch", self.endpoints.facepunch_url(), facepunch_body)
            .await?;
        let facepunch_status = facepunch_response.status();
        let _ = self.bounded_body("facepunch", facepunch_response).await?;
        if !facepunch_status.is_success() {
            return Err(status_error("facepunch", facepunch_status));
        }
        Ok(())
    }
}

impl CompanionRegistrar for FacepunchRegistrar {
    fn create_fcm_credentials<'a>(
        &'a self,
    ) -> BoxFuture<'a, Result<FcmSecrets, RegistrationError>> {
        let sender_id = self.fcm_sender_id.clone();
        Box::pin(async move {
            let connector = FcmConnector::new(sender_id)
                .map_err(|error| RegistrationError::Fcm(redact_fcm_error(error)))?;
            let snapshot = connector
                .register()
                .await
                .map_err(|error| RegistrationError::Fcm(redact_fcm_error(error)))?;
            fcm_secrets_from_snapshot(&snapshot)
        })
    }

    fn register<'a>(
        &'a self,
        auth_token: &'a SteamAuthToken,
        device_id: &'a DeviceId,
        fcm: &'a FcmSecrets,
    ) -> BoxFuture<'a, Result<(), RegistrationError>> {
        Box::pin(self.register_companion(auth_token, device_id, fcm))
    }
}

fn redact_fcm_error(error: push_receiver::Error) -> String {
    // push-receiver's HTTP errors are already URL-free; retain only its typed,
    // non-secret message for useful operator diagnostics.
    error.to_string()
}

fn map_http_error(service: &'static str, error: reqwest::Error) -> RegistrationError {
    if error.is_timeout() {
        RegistrationError::Timeout(service)
    } else {
        RegistrationError::Transport(service)
    }
}

fn status_error(service: &'static str, status: StatusCode) -> RegistrationError {
    if status == StatusCode::TOO_MANY_REQUESTS {
        RegistrationError::RateLimited
    } else if service == "facepunch"
        && (status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN)
    {
        RegistrationError::ReenrollmentRequired
    } else {
        RegistrationError::Rejected {
            service,
            status: status.as_u16(),
        }
    }
}

fn parse_expo_token(body: &[u8]) -> Result<String, RegistrationError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| RegistrationError::InvalidResponse("malformed Expo response"))?;
    let token = value
        .get("data")
        .and_then(|data| data.get("expoPushToken"))
        .and_then(Value::as_str)
        .ok_or(RegistrationError::InvalidResponse(
            "missing Expo push token",
        ))?;
    if (!token.starts_with("ExponentPushToken[") && !token.starts_with("ExpoPushToken["))
        || token.len() > 256
        || token.chars().any(char::is_control)
        || !token.ends_with(']')
    {
        return Err(RegistrationError::InvalidResponse(
            "invalid Expo push token",
        ));
    }
    Ok(token.to_owned())
}

/// Reads an operator-provided Steam auth token only when `load` is called.
pub struct FileSteamAuthToken {
    path: PathBuf,
}

impl fmt::Debug for FileSteamAuthToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileSteamAuthToken")
            .field("path", &self.path)
            .finish()
    }
}

impl FileSteamAuthToken {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl SteamAuthTokenSource for FileSteamAuthToken {
    fn load<'a>(&'a self) -> BoxFuture<'a, Result<SteamAuthToken, RegistrationError>> {
        Box::pin(async move {
            let metadata = fs::metadata(&self.path).await.map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    RegistrationError::AuthTokenUnavailable("missing")
                } else {
                    RegistrationError::AuthTokenUnavailable("unreadable")
                }
            })?;
            if !metadata.is_file() {
                return Err(RegistrationError::AuthTokenUnavailable(
                    "not a regular file",
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o077 != 0 {
                    return Err(RegistrationError::AuthTokenUnavailable(
                        "insecure permissions",
                    ));
                }
            }
            let file = fs::File::open(&self.path).await.map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    RegistrationError::AuthTokenUnavailable("missing")
                } else {
                    RegistrationError::AuthTokenUnavailable("unreadable")
                }
            })?;
            let mut bytes = Zeroizing::new(Vec::with_capacity(8 * 1024 + 1));
            file.take((8 * 1024 + 1) as u64)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| RegistrationError::AuthTokenUnavailable("unreadable"))?;
            if bytes.len() > 8 * 1024 {
                return Err(RegistrationError::AuthTokenUnavailable("oversized"));
            }
            let value = std::str::from_utf8(&bytes)
                .map_err(|_| RegistrationError::AuthTokenUnavailable("invalid"))?
                .trim_end();
            if value.is_empty() {
                return Err(RegistrationError::AuthTokenUnavailable("empty"));
            }
            SteamAuthToken::new(value.to_owned())
                .map_err(|_| RegistrationError::AuthTokenUnavailable("invalid"))
        })
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::{
        fs as std_fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

    fn test_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "xiao-registration-token-{}-{}",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn conversion_round_trip() {
        let secrets = FcmSecrets::new(7, 9, "registration".to_owned(), [1; 32], [2; 16]).unwrap();
        let snapshot = snapshot_from_fcm_secrets(&secrets).unwrap();
        let restored = fcm_secrets_from_snapshot(&snapshot).unwrap();
        assert_eq!(restored.android_id(), 7);
        assert_eq!(restored.security_token(), 9);
        assert_eq!(restored.registration_token(), "registration");
        assert_eq!(restored.web_push_private_key(), &[1; 32]);
        assert_eq!(restored.web_push_auth_secret(), &[2; 16]);
    }

    #[derive(Clone)]
    struct ScriptedResponse {
        status: u16,
        body: Vec<u8>,
        chunked: bool,
        hang: bool,
    }

    async fn scripted_server(
        scripts: Vec<ScriptedResponse>,
    ) -> (String, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut requests = Vec::new();
            for script in scripts {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut byte = [0_u8; 1];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    socket.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                }
                let header_end = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .unwrap()
                    + 4;
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                let mut body = vec![0; content_length];
                socket.read_exact(&mut body).await.unwrap();
                request.extend_from_slice(&body);
                requests.push(request);
                if script.hang {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    continue;
                }
                let reason = if script.status == 200 { "OK" } else { "TEST" };
                let location = if script.status == 302 {
                    "Location: http://example.com/other\r\n"
                } else {
                    ""
                };
                let response = if script.chunked {
                    format!(
                        "HTTP/1.1 {} {reason}\r\n{location}Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n",
                        script.status,
                        script.body.len()
                    )
                } else {
                    format!(
                        "HTTP/1.1 {} {reason}\r\n{location}Content-Length: {}\r\nConnection: close\r\n\r\n",
                        script.status,
                        script.body.len()
                    )
                };
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(&script.body).await.unwrap();
                if script.chunked {
                    socket.write_all(b"\r\n0\r\n\r\n").await.unwrap();
                }
            }
            requests
        });
        (format!("http://{address}"), handle)
    }

    #[tokio::test]
    async fn registration_posts_exact_expo_and_facepunch_shapes() {
        let (base, server) = scripted_server(vec![
            ScriptedResponse {
                status: 200,
                body: br#"{"data":{"expoPushToken":"ExponentPushToken[test]"}}"#.to_vec(),
                chunked: false,
                hang: false,
            },
            ScriptedResponse {
                status: 204,
                body: Vec::new(),
                chunked: false,
                hang: false,
            },
        ])
        .await;
        let endpoints = CompanionEndpoints::loopback_for_tests(&base).unwrap();
        let registrar = FacepunchRegistrar::new_with_timeout(
            "sender".to_owned(),
            endpoints,
            Duration::from_secs(2),
        )
        .unwrap();
        let auth = SteamAuthToken::new("steam-secret".to_owned()).unwrap();
        let device = DeviceId::new("device-1").unwrap();
        let fcm = FcmSecrets::new(1, 2, "fcm-secret".to_owned(), [1; 32], [2; 16]).unwrap();
        registrar.register(&auth, &device, &fcm).await.unwrap();
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        let first_header_end = requests[0]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        let second_header_end = requests[1]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        let first_headers =
            String::from_utf8_lossy(&requests[0][..first_header_end]).to_ascii_lowercase();
        let second_headers =
            String::from_utf8_lossy(&requests[1][..second_header_end]).to_ascii_lowercase();
        assert!(first_headers.contains("content-type: application/json"));
        assert!(second_headers.contains("content-type: application/json"));
        let expo: Value = serde_json::from_slice(&requests[0][first_header_end..]).unwrap();
        assert_eq!(expo["type"], "fcm");
        assert_eq!(expo["deviceId"], "device-1");
        assert_eq!(expo["development"], false);
        assert_eq!(expo["appId"], EXPO_APP_ID);
        assert_eq!(expo["deviceToken"], "fcm-secret");
        assert_eq!(expo["projectId"], EXPO_PROJECT_ID);
        let facepunch: Value = serde_json::from_slice(&requests[1][second_header_end..]).unwrap();
        assert_eq!(facepunch["AuthToken"], "steam-secret");
        assert_eq!(facepunch["DeviceId"], "device-1");
        assert_eq!(facepunch["PushKind"], 3);
        assert_eq!(facepunch["PushToken"], "ExponentPushToken[test]");
    }

    fn test_credentials() -> (SteamAuthToken, DeviceId, FcmSecrets) {
        (
            SteamAuthToken::new("steam-secret".to_owned()).unwrap(),
            DeviceId::new("device-1").unwrap(),
            FcmSecrets::new(1, 2, "fcm-secret".to_owned(), [1; 32], [2; 16]).unwrap(),
        )
    }

    #[tokio::test]
    async fn facepunch_statuses_map_without_leaking_credentials() {
        for (status, expected) in [
            (401, RegistrationError::ReenrollmentRequired),
            (403, RegistrationError::ReenrollmentRequired),
            (429, RegistrationError::RateLimited),
            (
                500,
                RegistrationError::Rejected {
                    service: "facepunch",
                    status: 500,
                },
            ),
        ] {
            let (base, server) = scripted_server(vec![
                ScriptedResponse {
                    status: 200,
                    body: br#"{"data":{"expoPushToken":"ExponentPushToken[test]"}}"#.to_vec(),
                    chunked: false,
                    hang: false,
                },
                ScriptedResponse {
                    status,
                    body: b"steam-secret fcm-secret ExponentPushToken[test]".to_vec(),
                    chunked: false,
                    hang: false,
                },
            ])
            .await;
            let registrar = FacepunchRegistrar::new_with_timeout(
                "sender".to_owned(),
                CompanionEndpoints::loopback_for_tests(&base).unwrap(),
                Duration::from_secs(2),
            )
            .unwrap();
            let (auth, device, fcm) = test_credentials();
            let result = registrar.register(&auth, &device, &fcm).await;
            assert_eq!(result, Err(expected));
            let _ = server.await.unwrap();
            let debug = format!("{result:?}");
            let display = result
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default();
            let registrar_debug = format!("{registrar:?}");
            for output in [debug, display, registrar_debug] {
                assert!(!output.contains("steam-secret"));
                assert!(!output.contains("fcm-secret"));
                assert!(!output.contains("ExponentPushToken[test]"));
                assert!(!output.contains("?"));
            }
        }
    }

    #[tokio::test]
    async fn expo_statuses_and_invalid_responses_are_rejected() {
        let cases = [
            (
                200,
                br#"not-json"#.to_vec(),
                RegistrationError::InvalidResponse("malformed Expo response"),
            ),
            (
                200,
                br#"{"data":{}}"#.to_vec(),
                RegistrationError::InvalidResponse("missing Expo push token"),
            ),
            (
                200,
                br#"{"data":{"expoPushToken":"wrong"}}"#.to_vec(),
                RegistrationError::InvalidResponse("invalid Expo push token"),
            ),
            (
                429,
                b"secret fcm-token".to_vec(),
                RegistrationError::RateLimited,
            ),
            (
                500,
                b"secret fcm-token".to_vec(),
                RegistrationError::Rejected {
                    service: "expo",
                    status: 500,
                },
            ),
        ];
        for (status, body, expected) in cases {
            let (base, server) = scripted_server(vec![ScriptedResponse {
                status,
                body,
                chunked: false,
                hang: false,
            }])
            .await;
            let registrar = FacepunchRegistrar::new_with_timeout(
                "sender".to_owned(),
                CompanionEndpoints::loopback_for_tests(&base).unwrap(),
                Duration::from_secs(2),
            )
            .unwrap();
            let (auth, device, fcm) = test_credentials();
            let result = registrar.register(&auth, &device, &fcm).await;
            assert_eq!(result, Err(expected));
            let _ = server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn response_bodies_are_bounded_for_length_and_chunked_streams() {
        for chunked in [false, true] {
            let (base, server) = scripted_server(vec![ScriptedResponse {
                status: 200,
                body: vec![b'x'; MAX_RESPONSE_BODY + 1],
                chunked,
                hang: false,
            }])
            .await;
            let registrar = FacepunchRegistrar::new_with_timeout(
                "sender".to_owned(),
                CompanionEndpoints::loopback_for_tests(&base).unwrap(),
                Duration::from_secs(2),
            )
            .unwrap();
            let (auth, device, fcm) = test_credentials();
            assert_eq!(
                registrar.register(&auth, &device, &fcm).await,
                Err(RegistrationError::InvalidResponse(
                    "response body too large"
                ))
            );
            let _ = server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn redirects_are_not_followed_and_hanging_requests_timeout() {
        let (base, server) = scripted_server(vec![ScriptedResponse {
            status: 302,
            body: Vec::new(),
            chunked: false,
            hang: false,
        }])
        .await;
        let registrar = FacepunchRegistrar::new_with_timeout(
            "sender".to_owned(),
            CompanionEndpoints::loopback_for_tests(&base).unwrap(),
            Duration::from_secs(2),
        )
        .unwrap();
        let (auth, device, fcm) = test_credentials();
        assert_eq!(
            registrar.register(&auth, &device, &fcm).await,
            Err(RegistrationError::Rejected {
                service: "expo",
                status: 302,
            })
        );
        let _ = server.await.unwrap();

        let (base, server) = scripted_server(vec![ScriptedResponse {
            status: 200,
            body: Vec::new(),
            chunked: false,
            hang: true,
        }])
        .await;
        let registrar = FacepunchRegistrar::new_with_timeout(
            "sender".to_owned(),
            CompanionEndpoints::loopback_for_tests(&base).unwrap(),
            Duration::from_millis(50),
        )
        .unwrap();
        let (auth, device, fcm) = test_credentials();
        assert_eq!(
            registrar.register(&auth, &device, &fcm).await,
            Err(RegistrationError::Timeout("expo"))
        );
        server.abort();
    }

    #[test]
    fn expo_token_validation_rejects_malformed_values() {
        assert!(parse_expo_token(br#"{"data":{}}"#).is_err());
        assert!(parse_expo_token(br#"{"data":{"expoPushToken":"wrong"}}"#).is_err());
        assert!(
            parse_expo_token(
                &serde_json::json!({"data":{"expoPushToken":"ExponentPushToken[x\ny]"}})
                    .to_string()
                    .into_bytes()
            )
            .is_err()
        );
        assert_eq!(
            parse_expo_token(br#"{"data":{"expoPushToken":"ExpoPushToken[ok]"}}"#).unwrap(),
            "ExpoPushToken[ok]"
        );
    }

    #[test]
    fn endpoint_validation_is_https_only_except_loopback_test_constructor() {
        assert!(CompanionEndpoints::new("http://example.com", "https://example.com").is_err());
        assert!(CompanionEndpoints::loopback_for_tests("http://example.com").is_err());
        assert!(CompanionEndpoints::loopback_for_tests("http://127.0.0.1:1234").is_ok());
        assert!(CompanionEndpoints::loopback_for_tests("http://localhost:1234").is_ok());
        assert!(CompanionEndpoints::loopback_for_tests("http://[::1]:1234").is_ok());
    }

    #[test]
    fn endpoint_debug_is_redacted() {
        let endpoint = CompanionEndpoints::new(
            "https://expo.example/?token=secret",
            "https://facepunch.example/",
        );
        assert!(endpoint.is_err());
        let endpoints = CompanionEndpoints::production();
        assert!(!format!("{endpoints:?}").contains(EXPO_PRODUCTION_URL));
    }

    #[tokio::test]
    async fn file_token_source_validates_and_trims() {
        let path = test_path();
        std_fs::write(&path, "steam-secret\n\t").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std_fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o600);
            std_fs::set_permissions(&path, permissions).unwrap();
        }
        let source = FileSteamAuthToken::new(path.clone());
        assert_eq!(source.load().await.unwrap().expose_secret(), "steam-secret");
        assert!(!format!("{source:?}").contains("steam-secret"));
        std_fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn file_token_source_reports_missing_and_empty() {
        let path = test_path();
        let source = FileSteamAuthToken::new(path.clone());
        assert!(matches!(
            source.load().await,
            Err(RegistrationError::AuthTokenUnavailable("missing"))
        ));
        std_fs::write(&path, "\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std_fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o600);
            std_fs::set_permissions(&path, permissions).unwrap();
        }
        assert!(matches!(
            source.load().await,
            Err(RegistrationError::AuthTokenUnavailable("empty"))
        ));
        std_fs::remove_file(path).unwrap();
    }
    #[tokio::test]
    async fn file_token_source_rejects_oversized_and_insecure_files() {
        let path = test_path();
        std_fs::write(&path, vec![b'x'; 8 * 1024 + 1]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std_fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o600);
            std_fs::set_permissions(&path, permissions).unwrap();
        }
        let source = FileSteamAuthToken::new(path.clone());
        assert!(matches!(
            source.load().await,
            Err(RegistrationError::AuthTokenUnavailable("oversized"))
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std_fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o644);
            std_fs::set_permissions(&path, permissions).unwrap();
            assert!(matches!(
                source.load().await,
                Err(RegistrationError::AuthTokenUnavailable(
                    "insecure permissions"
                ))
            ));
        }
        std_fs::remove_file(path).unwrap();
    }
}
