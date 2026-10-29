//! UniFi OS session authentication and the Network events WebSocket.
//! Session secrets never appear in Debug output or controller error messages.

use futures_util::{SinkExt, StreamExt};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio_tungstenite::{WebSocketStream, tungstenite};
use tungstenite::protocol::{Message, Role, WebSocketConfig};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use super::{ApiError, ClientOptions, UnsupportedReason};

pub const LOGIN_HINT: &str = "Run `unifi auth login` for this host/profile. Live events require a UniFi OS session; an API key alone is insufficient on some controllers.";

/// Normalize to one origin. Reject paths, credentials and URL suffixes before
/// any secret is sent. HTTP is permitted only for loopback development/tests.
pub fn controller_origin(host: &str) -> Result<String, ApiError> {
    let candidate = if host.contains("://") {
        host.to_owned()
    } else {
        format!("https://{host}")
    };
    let url = reqwest::Url::parse(&candidate)
        .map_err(|_| ApiError::Other("Invalid controller host".into()))?;
    let loopback = url.host_str().is_some_and(|h| {
        h.trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || url.host().is_none()
    {
        return Err(ApiError::Other(
            "Session authentication requires an HTTPS controller origin without credentials, path, query or fragment (HTTP is allowed only on loopback).".into(),
        ));
    }
    Ok(url.origin().ascii_serialization())
}

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]
pub struct SessionSecret {
    origin: String,
    token: String,
    csrf: Option<String>,
}

pub struct NetworkSession {
    http: reqwest::Client,
    secret: SessionSecret,
}

fn secret_header(value: &str) -> Result<HeaderValue, ApiError> {
    let mut header = HeaderValue::from_str(value)
        .map_err(|_| ApiError::SessionAuth("Invalid session header; log in again.".into()))?;
    header.set_sensitive(true);
    Ok(header)
}

impl NetworkSession {
    pub fn from_secret(
        host: &str,
        secret: SessionSecret,
        options: ClientOptions,
    ) -> Result<Self, ApiError> {
        let origin = controller_origin(host)?;
        if secret.origin != origin || secret.token.is_empty() {
            return Err(ApiError::SessionAuth(
                "Stored session does not match this controller; log in again.".into(),
            ));
        }
        let mut headers = HeaderMap::new();
        headers.insert("cookie", secret_header(&format!("TOKEN={}", secret.token))?);
        if let Some(csrf) = &secret.csrf {
            headers.insert("x-csrf-token", secret_header(csrf)?);
        }
        Ok(Self {
            http: session_http(options).default_headers(headers).build()?,
            secret,
        })
    }

    /// Password-only convenience API. Interactive callers should use
    /// begin_login and complete an MFA challenge with a one-time code.
    pub async fn login(
        host: &str,
        username: &str,
        password: &str,
        options: ClientOptions,
    ) -> Result<Self, ApiError> {
        match Self::begin_login(host, username, password, options).await? {
            LoginOutcome::Authenticated(session) => Ok(session),
            LoginOutcome::MfaRequired(_) => Err(ApiError::SessionAuth(
                "MFA code required. Run `unifi auth login` in an interactive terminal, or complete the challenge returned by begin_login.".into(),
            )),
        }
    }

    pub async fn begin_login(
        host: &str,
        username: &str,
        password: &str,
        options: ClientOptions,
    ) -> Result<LoginOutcome, ApiError> {
        let origin = controller_origin(host)?;
        if username.trim().is_empty() || password.is_empty() {
            return Err(ApiError::SessionAuth(
                "Username and password are required for session login.".into(),
            ));
        }
        let http = session_http(options).build()?;
        let response = http
            .post(format!("{origin}/api/auth/login"))
            .json(&serde_json::json!({"username": username, "password": password, "rememberMe": false}))
            .send().await?;
        if response.status().as_u16() == 404 {
            return Err(ApiError::Unsupported {
                endpoint: "/api/auth/login (UniFi OS session login)".into(),
                reason: UnsupportedReason::Removed,
            });
        }
        let status = response.status().as_u16();
        if status != 499 {
            check_status(status, "Session login")?;
            if let Some(secret) = login_secret(&origin, response.headers()) {
                return Self::from_secret(&origin, secret, options)
                    .map(LoginOutcome::Authenticated);
            }
        }
        // Only read the challenge when needed. Deserialize an allowlist of
        // fields; never expose challenge cookies or controller response text.
        let challenge = read_challenge(response).await?;
        let (kind, cookie) = if status == 499 {
            let cookie = challenge.data.and_then(|data| data.mfa_cookie)
                .and_then(challenge_cookie_pair)
                .ok_or_else(|| ApiError::SessionAuth(
                    "MFA challenge was missing a valid challenge cookie. Start a fresh login; this controller's MFA response may be unsupported.".into(),
                ))?;
            (MfaKind::Sso, Some(Zeroizing::new(cookie)))
        } else if challenge
            .meta
            .is_some_and(|meta| meta.msg == "api.err.Ubic2faTokenRequired")
        {
            (MfaKind::Local, None)
        } else {
            return Err(ApiError::SessionAuth(
                "Login returned no UniFi OS session. Check the credentials or MFA configuration."
                    .into(),
            ));
        };
        Ok(LoginOutcome::MfaRequired(MfaChallenge {
            http,
            origin,
            kind,
            cookie,
            options,
        }))
    }

    pub fn secret(&self) -> &SessionSecret {
        &self.secret
    }

    pub async fn check(&self) -> Result<(), ApiError> {
        self.check_network_access(false).await
    }

    /// A new login that cannot access Network needs a permission diagnosis,
    /// not another password/MFA attempt.
    pub async fn check_after_login(&self) -> Result<(), ApiError> {
        self.check_network_access(true).await
    }

    async fn check_network_access(&self, after_login: bool) -> Result<(), ApiError> {
        let response = self
            .http
            .get(format!(
                "{}/proxy/network/api/self/sites",
                self.secret.origin
            ))
            .send()
            .await?;
        let status = response.status().as_u16();
        if status == 404 {
            return Err(ApiError::Unsupported {
                endpoint: "/proxy/network/api/self/sites (Network application session check)"
                    .into(),
                reason: UnsupportedReason::Removed,
            });
        }
        if after_login && matches!(status, 401 | 403) {
            return Err(ApiError::SessionAuth(format!(
                "Login succeeded, but Network rejected this account (HTTP {status}). Check its Network application role and access permissions before logging in again."
            )));
        }
        check_status(status, "Session check")?;
        let failure_hint = if after_login {
            "Login succeeded, but Network did not confirm access. Check the account's Network application role and the controller's Network app."
        } else {
            "Controller did not confirm a Network session. Run `unifi auth login` for this host/profile."
        };
        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|_| ApiError::SessionAuth(failure_hint.into()))?;
        if value["meta"]["rc"] != "ok" {
            return Err(ApiError::SessionAuth(failure_hint.into()));
        }
        Ok(())
    }

    pub async fn events(&self, site: &str) -> Result<EventStream, ApiError> {
        validate_site(site)?;
        let key = tungstenite::handshake::client::generate_key();
        let response = self
            .http
            .get(format!(
                "{}/proxy/network/wss/s/{site}/events",
                self.secret.origin
            ))
            .header("origin", &self.secret.origin)
            .header("connection", "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", &key)
            .send()
            .await?;
        let status = response.status().as_u16();
        if status != 101 {
            if status == 404 {
                return Err(ApiError::Unsupported {
                    endpoint: "/proxy/network/wss/s/{site}/events".into(),
                    reason: UnsupportedReason::Removed,
                });
            }
            check_status(status, "Events WebSocket")?;
            return Err(ApiError::Other(
                "Controller did not upgrade the events WebSocket.".into(),
            ));
        }
        let headers = response.headers();
        let expected = tungstenite::handshake::derive_accept_key(key.as_bytes());
        let upgrade = headers
            .get("upgrade")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let connection = headers
            .get("connection")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if headers
            .get("sec-websocket-accept")
            .and_then(|v| v.to_str().ok())
            != Some(expected.as_str())
            || !upgrade.eq_ignore_ascii_case("websocket")
            || !connection
                .split(',')
                .any(|v| v.trim().eq_ignore_ascii_case("upgrade"))
            || headers.contains_key("sec-websocket-extensions")
            || headers.contains_key("sec-websocket-protocol")
        {
            return Err(ApiError::Other(
                "Invalid events WebSocket handshake.".into(),
            ));
        }
        let stream = response.upgrade().await?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(4 * 1024 * 1024))
            .max_frame_size(Some(4 * 1024 * 1024));
        Ok(EventStream {
            socket: WebSocketStream::from_raw_socket(stream, Role::Client, Some(config)).await,
            awaiting_pong: false,
        })
    }
}

/// Login state intentionally has no Debug implementation: challenges contain
/// authentication material and must never be included in diagnostic output.
pub enum LoginOutcome {
    Authenticated(NetworkSession),
    MfaRequired(MfaChallenge),
}

enum MfaKind {
    Sso,
    Local,
}

pub struct MfaChallenge {
    http: reqwest::Client,
    origin: String,
    kind: MfaKind,
    cookie: Option<Zeroizing<String>>,
    options: ClientOptions,
}

impl MfaChallenge {
    /// Consume the challenge, allowing exactly one code submission. Passwords
    /// and one-time codes are borrowed, never stored in the resulting session.
    pub async fn complete(
        self,
        username: &str,
        password: &str,
        code: &str,
    ) -> Result<NetworkSession, ApiError> {
        if !matches!(code.len(), 6 | 8) || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ApiError::SessionAuth(
                "Enter a 6- or 8-digit authenticator code. The code was not submitted.".into(),
            ));
        }
        let mut body =
            serde_json::json!({"username":username,"password":password,"rememberMe":false});
        body[match self.kind {
            MfaKind::Sso => "token",
            MfaKind::Local => "ubic_2fa_token",
        }] = code.into();
        let mut request = self
            .http
            .post(format!("{}/api/auth/login", self.origin))
            .json(&body);
        if let Some(cookie) = &self.cookie {
            request = request.header("cookie", secret_header(cookie)?);
        }
        let response = request.send().await?;
        if matches!(response.status().as_u16(), 400 | 401 | 403 | 499) {
            return Err(ApiError::SessionAuth(
                "MFA login rejected. Check the current authenticator code and start a fresh login. No automatic retry was attempted.".into(),
            ));
        }
        check_status(response.status().as_u16(), "MFA login")?;
        let secret = login_secret(&self.origin, response.headers()).ok_or_else(|| ApiError::SessionAuth(
            "MFA login returned no session. Start a fresh login with a current authenticator code.".into(),
        ))?;
        NetworkSession::from_secret(&self.origin, secret, self.options)
    }
}

fn login_secret(origin: &str, headers: &HeaderMap) -> Option<SessionSecret> {
    // Partitioned cookies are not supported by reqwest's cookie jar.
    let token = headers.get_all("set-cookie").iter().find_map(|h| {
        h.to_str()
            .ok()?
            .split(';')
            .next()?
            .strip_prefix("TOKEN=")
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    })?;
    let csrf = headers
        .get("x-csrf-token")
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned);
    Some(SessionSecret {
        origin: origin.to_owned(),
        token,
        csrf,
    })
}

#[derive(Deserialize)]
struct ChallengeResponse {
    // Local 2FA may include unrelated array data. Parse only an SSO object;
    // ignore other shapes so legacy meta.msg can still be recognized.
    #[serde(default, deserialize_with = "challenge_data")]
    data: Option<ChallengeData>,
    meta: Option<ChallengeMeta>,
}

#[derive(Deserialize)]
struct ChallengeData {
    #[serde(rename = "mfaCookie")]
    mfa_cookie: Option<String>,
}

#[derive(Deserialize)]
struct ChallengeMeta {
    #[serde(default)]
    msg: String,
}

fn challenge_data<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<ChallengeData>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

async fn read_challenge(mut response: reqwest::Response) -> Result<ChallengeResponse, ApiError> {
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > 64 * 1024 {
            return Err(ApiError::SessionAuth(
                "Login response exceeded the size limit.".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        ApiError::SessionAuth(
            "Login returned an invalid authentication challenge. Check the controller address and start a fresh login.".into(),
        )
    })
}

fn challenge_cookie_pair(cookie: String) -> Option<String> {
    // Controllers may include Set-Cookie attributes. Only the first pair is
    // valid in the outgoing Cookie header; attributes are never forwarded.
    let cookie = Zeroizing::new(cookie);
    let pair = cookie.split(';').next()?.trim();
    let (name, value) = pair.split_once('=')?;
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        && !value.is_empty()
        && value
            .bytes()
            .all(|b| matches!(b, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e));
    valid.then(|| pair.to_owned())
}

fn session_http(options: ClientOptions) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(options.accept_invalid_certs)
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .no_proxy()
        .user_agent("unifi-cli")
        .timeout(Duration::from_secs(30))
}

fn check_status(status: u16, action: &str) -> Result<(), ApiError> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    // Do not echo response bodies, cookie values, close reasons or login errors.
    if matches!(status, 401 | 403) {
        return Err(ApiError::SessionAuth(format!(
            "{action} rejected (HTTP {status}). {LOGIN_HINT}"
        )));
    }
    Err(ApiError::Api {
        status,
        message: if (300..400).contains(&status) {
            format!("{action} refused a redirect (HTTP {status}).")
        } else {
            format!("{action} failed (HTTP {status}).")
        },
    })
}

pub fn validate_site(site: &str) -> Result<(), ApiError> {
    if site.is_empty()
        || !site
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ApiError::Other(
            "Site must contain only letters, digits, underscores or hyphens.".into(),
        ));
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Debug)]
pub struct LiveMessage {
    pub message: String,
    pub data: serde_json::Value,
}

pub struct EventStream {
    socket: WebSocketStream<reqwest::Upgraded>,
    awaiting_pong: bool,
}

impl EventStream {
    pub async fn next(&mut self) -> Result<LiveMessage, ApiError> {
        loop {
            match self.socket.next().await {
                Some(Ok(Message::Text(text))) => return decode_message(text.as_bytes()),
                Some(Ok(Message::Binary(bytes))) => return decode_message(&bytes),
                Some(Ok(Message::Pong(_))) => self.awaiting_pong = false,
                Some(Ok(Message::Ping(_))) => self.socket.flush().await.map_err(stream_error)?,
                Some(Ok(Message::Close(frame))) => {
                    let code = frame.map(|f| u16::from(f.code));
                    return Err(ApiError::Other(format!(
                        "Events stream closed by controller (code {}). No automatic reconnect was attempted; updates may have been missed. {LOGIN_HINT}",
                        code.map(|c| c.to_string()).unwrap_or_else(|| "none".into())
                    )));
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(stream_error(e)),
                None => {
                    return Err(ApiError::Other(format!(
                        "Events stream disconnected. {LOGIN_HINT}"
                    )));
                }
            }
        }
    }

    pub async fn heartbeat(&mut self) -> Result<(), ApiError> {
        if self.awaiting_pong {
            return Err(ApiError::Other(
                "Events heartbeat timed out; the connection is no longer healthy.".into(),
            ));
        }
        self.socket
            .send(Message::Ping(Vec::new().into()))
            .await
            .map_err(stream_error)?;
        self.awaiting_pong = true;
        Ok(())
    }

    pub async fn close(&mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), self.socket.close(None)).await;
    }
}

fn decode_message(bytes: &[u8]) -> Result<LiveMessage, ApiError> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| ApiError::Other("Controller sent invalid event JSON.".into()))?;
    let message = value["meta"]["message"]
        .as_str()
        .filter(|m| !m.is_empty())
        .ok_or_else(|| ApiError::Other("Controller sent an invalid event envelope.".into()))?
        .to_owned();
    if value.get("data").is_none() {
        return Err(ApiError::Other(
            "Controller sent an event without data.".into(),
        ));
    }
    Ok(LiveMessage {
        message,
        data: value["data"].take(),
    })
}

fn stream_error(error: tungstenite::Error) -> ApiError {
    let cause = match error {
        tungstenite::Error::Capacity(_) => "message exceeded the configured size limit",
        tungstenite::Error::Protocol(_) => "invalid WebSocket protocol",
        tungstenite::Error::Utf8(_) => "invalid UTF-8 message",
        _ => "connection failed",
    };
    ApiError::Other(format!(
        "Events WebSocket {cause}; no automatic reconnect was attempted."
    ))
}
