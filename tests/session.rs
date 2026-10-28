//! Fake credentials and loopback fixtures only. Native store tests are opt-in.
use futures_util::{SinkExt, StreamExt};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{self, Message, protocol::Role},
};
use unifi_cli::api::{
    ApiError, ClientOptions,
    session::{NetworkSession, SessionSecret, controller_origin},
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, header, method, path},
};

const TOKEN: &str = "fake-session-token";
const CSRF: &str = "fake-csrf-token";

fn isolated_cli() -> (tempfile::TempDir, std::process::Command) {
    let root = tempfile::tempdir().unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_unifi"));
    for key in [
        "UNIFI_HOST",
        "UNIFI_API_KEY",
        "UNIFI_USERNAME",
        "UNIFI_PASSWORD",
        "UNIFI_PROFILE",
        "UNIFI_ACCEPT_INVALID_CERTS",
        "UNIFI_MFA_CODE",
    ] {
        command.env_remove(key);
    }
    command
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path())
        .env("APPDATA", root.path());
    (root, command)
}

fn secret(origin: &str) -> SessionSecret {
    serde_json::from_value(serde_json::json!({"origin":origin,"token":TOKEN,"csrf":CSRF})).unwrap()
}

#[test]
fn rejects_unsafe_origins_and_canonicalizes_safe_ones() {
    assert_eq!(
        controller_origin("controller.example").unwrap(),
        "https://controller.example"
    );
    assert_eq!(
        controller_origin("https://CONTROLLER.example:443/").unwrap(),
        "https://controller.example"
    );
    assert_eq!(
        controller_origin("http://[::1]:8080").unwrap(),
        "http://[::1]:8080"
    );
    for host in [
        "http://controller.example",
        "https://user:secret@controller.example",
        "https://controller.example/api",
        "https://controller.example/?x=secret",
        "https://controller.example/#secret",
        "ftp://controller.example",
        "",
    ] {
        assert!(controller_origin(host).is_err(), "accepted unsafe origin");
    }
}

#[tokio::test]
async fn login_uses_partitioned_cookie_and_csrf_without_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/api/auth/login"))
        .and(body_json(serde_json::json!({"username":"test-user", "password":"fake-password", "rememberMe":false})))
        .respond_with(ResponseTemplate::new(200).append_header("set-cookie", format!("TOKEN={TOKEN}; Secure; HttpOnly; Partitioned"))
            .insert_header("x-csrf-token", CSRF).set_body_string("not needed"))
        .expect(1).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/proxy/network/api/self/sites"))
        .and(header("cookie", format!("TOKEN={TOKEN}")))
        .and(header("x-csrf-token", CSRF))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"meta":{"rc":"ok"},"data":[]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let session = NetworkSession::login(
        &server.uri(),
        "test-user",
        "fake-password",
        ClientOptions::default(),
    )
    .await
    .unwrap();
    session.check().await.unwrap();
    for request in server.received_requests().await.unwrap() {
        assert!(!request.headers.contains_key("x-api-key"));
        assert_eq!(request.headers.get("user-agent").unwrap(), "unifi-cli");
    }
}

#[tokio::test]
async fn login_rejects_redirects_without_leaking_credentials_or_error_bodies() {
    let source = MockServer::start().await;
    let destination = MockServer::start().await;
    for status in [307, 401, 403, 429, 500] {
        source.reset().await;
        Mock::given(path("/api/auth/login"))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("location", format!("{}/stolen", destination.uri()))
                    .set_body_string("SECRET-CONTROLLER-BODY"),
            )
            .expect(1)
            .mount(&source)
            .await;
        let err = match NetworkSession::login(
            &source.uri(),
            "test-user",
            "fake-password",
            ClientOptions::default(),
        )
        .await
        {
            Ok(_) => panic!("unexpected success"),
            Err(e) => e,
        };
        let display = format!("{err:?} {err}");
        for secret in ["SECRET-CONTROLLER-BODY", "fake-password", "test-user"] {
            assert!(!display.contains(secret));
        }
        assert_eq!(
            matches!(err, ApiError::SessionAuth(_)),
            matches!(status, 401 | 403)
        );
    }
    assert!(destination.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn missing_cookie_mfa_and_expired_session_are_actionable() {
    for status in [200, 499] {
        let server = MockServer::start().await;
        Mock::given(path("/api/auth/login"))
            .respond_with(ResponseTemplate::new(status).set_body_json(
                serde_json::json!({"code":"MFA_AUTH_REQUIRED", "private":"PRIVATE"}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let err = NetworkSession::login(
            &server.uri(),
            "test-user",
            "fake-password",
            ClientOptions::default(),
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(err, ApiError::SessionAuth(_)));
        assert!(err.to_string().contains("MFA"));
        assert!(!err.to_string().contains("PRIVATE"));
    }
    let server = MockServer::start().await;
    Mock::given(path("/proxy/network/api/self/sites"))
        .respond_with(ResponseTemplate::new(401).set_body_string("PRIVATE"))
        .mount(&server)
        .await;
    let session = NetworkSession::from_secret(
        &server.uri(),
        secret(&server.uri()),
        ClientOptions::default(),
    )
    .unwrap();
    let err = session.check().await.unwrap_err();
    assert!(matches!(err, ApiError::SessionAuth(_)));
    assert!(err.to_string().contains("auth login"));
    assert!(!err.to_string().contains("PRIVATE"));
}

#[test]
fn session_cannot_be_replayed_to_another_origin() {
    assert!(
        NetworkSession::from_secret(
            "https://other.example",
            secret("https://controller.example"),
            ClientOptions::default()
        )
        .is_err()
    );
}

struct WsFixture {
    origin: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for WsFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn websocket_fixture(mode: &'static str) -> WsFixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let captured = captured.clone();
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    if bytes.len() > 16384 {
                        return;
                    }
                    let Ok(byte) = socket.read_u8().await else {
                        return;
                    };
                    bytes.push(byte);
                }
                let request = String::from_utf8(bytes).unwrap();
                captured.lock().unwrap().push(request.clone());
                if request.starts_with("POST /api/auth/login ") {
                    let length: usize = request
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .unwrap()
                        .parse()
                        .unwrap();
                    let mut body = vec![0; length];
                    socket.read_exact(&mut body).await.unwrap();
                    assert_eq!(
                        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["password"],
                        "fake-password"
                    );
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nSet-Cookie: TOKEN={TOKEN}; Partitioned\r\nX-CSRF-Token: {CSRF}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                    return;
                }
                assert!(request.contains(&format!("cookie: TOKEN={TOKEN}\r\n")));
                assert!(request.contains(&format!("x-csrf-token: {CSRF}\r\n")));
                if request.starts_with("GET /proxy/network/api/self/sites ") {
                    let body = r#"{"meta":{"rc":"ok"},"data":[{"name":"office"}]}"#;
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                    return;
                }
                assert!(request.starts_with("GET /proxy/network/wss/s/office/events HTTP/1.1\r\n"));
                assert!(request.contains("origin: http://127.0.0.1:"));
                assert!(!request.to_lowercase().contains("x-api-key"));
                if matches!(mode, "401" | "404" | "redirect") {
                    let status = match mode {
                        "401" => "401 Unauthorized",
                        "404" => "404 Not Found",
                        _ => "307 Temporary Redirect",
                    };
                    socket.write_all(format!("HTTP/1.1 {status}\r\nLocation: https://other.example/\r\nContent-Length: 7\r\nConnection: close\r\n\r\nPRIVATE").as_bytes()).await.unwrap();
                    return;
                }
                let key = request
                    .lines()
                    .find_map(|line| line.strip_prefix("sec-websocket-key: "))
                    .unwrap();
                let accept = if mode == "bad-handshake" {
                    "invalid".to_owned()
                } else {
                    tungstenite::handshake::derive_accept_key(key.as_bytes())
                };
                socket.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await.unwrap();
                if mode == "bad-handshake" {
                    return;
                }
                let mut ws = WebSocketStream::from_raw_socket(socket, Role::Server, None).await;
                if mode == "close" {
                    ws.close(Some(tungstenite::protocol::CloseFrame {
                        code: tungstenite::protocol::frame::coding::CloseCode::Normal,
                        reason: "PRIVATE-CLOSE-REASON".into(),
                    }))
                    .await
                    .unwrap();
                    return;
                }
                if mode == "controls" {
                    ws.send(Message::Text(serde_json::json!({"meta":{"message":"events"},"data":[{"msg":"\u{1b}\u{202e}\u{9b}"}]}).to_string().into())).await.unwrap();
                } else if mode == "malformed" {
                    ws.send(Message::Text("PRIVATE-BAD-JSON".into()))
                        .await
                        .unwrap();
                } else if mode == "updates" {
                    ws.send(Message::Ping(vec![1].into())).await.unwrap();
                    for category in ["events", "device:sync"] {
                        ws.send(Message::Text(serde_json::json!({"meta":{"message":category},"data":[{"msg":"example", "key":"EVT_TEST", "nested":[{"x_authkey":"fake-device-secret", "syslog_key":"fake-log-secret", "guest_token":"fake-guest-secret", "x_vwirekey":"fake-wire-secret"}]}]}).to_string().into())).await.unwrap();
                    }
                }
                while let Some(Ok(message)) = ws.next().await {
                    match message {
                        Message::Close(_) => {
                            let _ = ws.flush().await;
                            return;
                        }
                        Message::Ping(_) => {
                            let _ = ws.flush().await;
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    WsFixture {
        origin,
        requests,
        task,
    }
}

fn fixture_session(fixture: &WsFixture) -> NetworkSession {
    NetworkSession::from_secret(
        &fixture.origin,
        secret(&fixture.origin),
        ClientOptions::default(),
    )
    .unwrap()
}

#[tokio::test]
async fn websocket_streams_jsonl_and_stops_at_message_limit() {
    let fixture = websocket_fixture("updates").await;
    let mut stream = fixture_session(&fixture).events("office").await.unwrap();
    let mut output = Vec::new();
    unifi_cli::commands::events::watch_to(
        &mut stream,
        true,
        None,
        Some(2),
        &mut output,
        std::future::pending(),
    )
    .await
    .unwrap();
    stream.close().await;
    let text = String::from_utf8(output).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["message"], "events");
    assert_eq!(lines[1]["message"], "device:sync");
    assert_eq!(lines[1]["data"][0]["key"], "EVT_TEST");
    assert_eq!(lines[1]["data"][0]["nested"][0]["x_authkey"], "[REDACTED]");
    for secret in [
        "fake-device-secret",
        "fake-log-secret",
        "fake-guest-secret",
        "fake-wire-secret",
    ] {
        assert!(!text.contains(secret));
    }
    assert!(!text.contains(TOKEN));
    assert!(!text.contains(CSRF));
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn text_stream_summarizes_updates_without_exposing_device_credentials() {
    let fixture = websocket_fixture("updates").await;
    let mut stream = fixture_session(&fixture).events("office").await.unwrap();
    let mut output = Vec::new();
    unifi_cli::commands::events::watch_to(
        &mut stream,
        false,
        None,
        Some(2),
        &mut output,
        std::future::pending(),
    )
    .await
    .unwrap();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "events: 1 event(s): example\ndevice:sync: 1 device update(s)\n"
    );
    stream.close().await;
}

#[tokio::test]
async fn idle_duration_and_interruption_stop_without_a_message() {
    let fixture = websocket_fixture("idle").await;
    for duration in [Some(1), None] {
        let mut stream = fixture_session(&fixture).events("office").await.unwrap();
        let mut output = Vec::new();
        let stop = async move {
            if duration.is_some() {
                std::future::pending::<()>().await;
            }
            Ok(())
        };
        tokio::time::timeout(
            Duration::from_secs(3),
            unifi_cli::commands::events::watch_to(
                &mut stream,
                true,
                duration,
                None,
                &mut output,
                stop,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        stream.close().await;
        assert!(output.is_empty());
    }
}

#[tokio::test]
async fn closed_and_malformed_streams_fail_without_echoing_private_contents() {
    for mode in ["close", "malformed"] {
        let fixture = websocket_fixture(mode).await;
        let mut stream = fixture_session(&fixture).events("office").await.unwrap();
        let err = stream.next().await.unwrap_err();
        assert!(!err.to_string().contains("PRIVATE"));
        if mode == "close" {
            assert!(err.to_string().contains("1000"));
        }
    }
}

#[tokio::test]
async fn rejects_bad_handshake_status_and_site_without_retrying() {
    for mode in ["401", "404", "redirect", "bad-handshake"] {
        let fixture = websocket_fixture(mode).await;
        let session = fixture_session(&fixture);
        assert!(session.events("../other?secret").await.is_err());
        assert!(fixture.requests.lock().unwrap().is_empty());
        let err = session.events("office").await.err().unwrap();
        assert_eq!(matches!(err, ApiError::SessionAuth(_)), mode == "401");
        assert_eq!(matches!(err, ApiError::Unsupported { .. }), mode == "404");
        assert!(!err.to_string().contains("PRIVATE"));
        // Assert in the test task too: a fixture task panic must not look
        // like a successful rejection test just because the socket closed.
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert!(request.starts_with("GET /proxy/network/wss/s/office/events HTTP/1.1\r\n"));
        assert!(request.contains(&format!("cookie: TOKEN={TOKEN}\r\n")));
        assert!(request.contains(&format!("x-csrf-token: {CSRF}\r\n")));
        assert!(request.contains(&format!("origin: {}\r\n", fixture.origin)));
        assert!(request.contains("user-agent: unifi-cli\r\n"));
        assert!(!request.to_lowercase().contains("x-api-key"));
        if mode == "bad-handshake" {
            assert!(err.to_string().contains("handshake"));
        } else if mode == "redirect" {
            assert!(err.to_string().contains("redirect"));
        }
    }
}

#[tokio::test]
async fn heartbeat_requires_a_pong() {
    let fixture = websocket_fixture("idle").await;
    let mut stream = fixture_session(&fixture).events("office").await.unwrap();
    stream.heartbeat().await.unwrap();
    assert!(
        stream
            .heartbeat()
            .await
            .unwrap_err()
            .to_string()
            .contains("heartbeat")
    );
    stream.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_binary_streams_with_ephemeral_credentials_and_no_api_key() {
    let fixture = websocket_fixture("updates").await;
    let host = fixture.origin.clone();
    let output = tokio::task::spawn_blocking(move || {
        let (_root, mut command) = isolated_cli();
        command
            .args([
                "--host",
                &host,
                "--username",
                "test-user",
                "--password",
                "fake-password",
                "--output",
                "json",
                "events",
                "watch",
                "--login",
                "--site",
                "office",
                "--limit",
                "2",
                "--duration",
                "5",
            ])
            .env_remove("UNIFI_API_KEY")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 2);
    for line in stdout.lines() {
        serde_json::from_str::<serde_json::Value>(line).unwrap();
    }
    assert!(!stdout.contains(TOKEN));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("fake-password"));
    assert_eq!(fixture.requests.lock().unwrap().len(), 2);
}

#[test]
fn cli_rejects_invalid_stream_bounds_before_authentication() {
    for args in [
        vec!["--limit", "0"],
        vec!["--duration", "0"],
        vec!["--duration", "86401"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_unifi"))
            .args(["events", "watch"])
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
}

#[test]
#[ignore = "Opt-in native credential-store test using a disposable, uniquely scoped fake session"]
fn native_credential_store_round_trip() {
    use unifi_cli::auth::{OsSessionStore, SessionStore};
    let profile = format!(
        "test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let store = OsSessionStore::new("https://session-test.invalid", Some(&profile)).unwrap();
    struct Cleanup<'a>(&'a OsSessionStore);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = self.0.delete();
        }
    }
    let _cleanup = Cleanup(&store);
    assert!(store.load().unwrap().is_none());
    store.save(&secret("https://session-test.invalid")).unwrap();
    let loaded = store.load().unwrap().unwrap();
    let serialized = zeroize::Zeroizing::new(serde_json::to_string(&loaded).unwrap());
    assert!(serialized.contains(TOKEN));
    assert!(serialized.contains(CSRF));
    assert!(store.delete().unwrap());
    assert!(!store.delete().unwrap());
    assert!(store.load().unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Opt-in native store integration: login, status, stream and logout in separate processes, fake loopback controller only"]
async fn native_session_persists_across_cli_processes() {
    use unifi_cli::auth::{OsSessionStore, SessionStore};
    let fixture = websocket_fixture("updates").await;
    let profile = format!(
        "test-process-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let store = OsSessionStore::new(&fixture.origin, Some(&profile)).unwrap();
    struct Cleanup<'a>(&'a OsSessionStore);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = self.0.delete();
        }
    }
    let _cleanup = Cleanup(&store);
    for args in [
        vec![
            "--username",
            "test-user",
            "--password",
            "fake-password",
            "auth",
            "login",
        ],
        vec!["auth", "status"],
        // A configured username alone must not override the stored session.
        vec![
            "--username",
            "test-user",
            "events",
            "watch",
            "--site",
            "office",
            "--limit",
            "2",
            "--duration",
            "5",
        ],
        vec!["auth", "logout"],
    ] {
        let operation = args.join(" ");
        let host = fixture.origin.clone();
        let profile = profile.clone();
        let output = tokio::task::spawn_blocking(move || {
            let (_root, mut command) = isolated_cli();
            // Native macOS Keychain resolves the login keychain using HOME.
            // Keep the real home for this opt-in test; the unique nonexistent
            // profile prevents loading credentials from the user's config.
            if let Some(home) = std::env::var_os("HOME") {
                command.env("HOME", home);
            }
            command
                .args(["--host", &host, "--profile", &profile])
                .args(args)
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(
            output.status.success(),
            "{operation}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        for bytes in [&output.stdout, &output.stderr] {
            let text = String::from_utf8_lossy(bytes);
            for secret in [TOKEN, CSRF, "fake-password"] {
                assert!(!text.contains(secret));
            }
        }
    }
    assert!(store.load().unwrap().is_none());
    assert_eq!(
        fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.starts_with("POST /api/auth/login "))
            .count(),
        1
    );
}

#[tokio::test]
async fn json_stream_preserves_broken_pipe_as_io_error() {
    struct ClosedPipe;
    impl std::io::Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let fixture = websocket_fixture("updates").await;
    let mut stream = fixture_session(&fixture).events("office").await.unwrap();
    let err = unifi_cli::commands::events::watch_to(
        &mut stream,
        true,
        None,
        Some(1),
        &mut ClosedPipe,
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::BrokenPipe
    );
    stream.close().await;
}

#[tokio::test]
async fn responsive_server_survives_multiple_heartbeats() {
    let fixture = websocket_fixture("idle").await;
    let mut stream = fixture_session(&fixture).events("office").await.unwrap();
    for _ in 0..3 {
        stream.heartbeat().await.unwrap();
        // next() consumes Pong internally, then waits for a data message.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stream.next())
                .await
                .is_err()
        );
    }
    stream.close().await;
}

#[tokio::test]
async fn text_output_escapes_terminal_controls_in_message_and_data() {
    // Unit formatting coverage lives alongside the command's private formatter;
    // this integration check verifies the public text streaming path.
    let fixture = websocket_fixture("controls").await;
    let mut stream = fixture_session(&fixture).events("office").await.unwrap();
    let mut output = Vec::new();
    unifi_cli::commands::events::watch_to(
        &mut stream,
        false,
        None,
        Some(1),
        &mut output,
        std::future::pending(),
    )
    .await
    .unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(!text.contains(['\u{1b}', '\u{9b}', '\u{202e}']));
    assert!(text.contains("\\u{1b}"));
    assert!(text.contains("\\u{9b}"));
    assert!(text.contains("\\u{202e}"));
    stream.close().await;
}

#[tokio::test]
#[ignore = "Opt-in 45-second loopback check that HTTP timeout does not terminate an upgraded stream"]
async fn upgraded_stream_outlives_http_request_timeout() {
    let fixture = websocket_fixture("idle").await;
    let session = fixture_session(&fixture);
    let mut stream = session.events("office").await.unwrap();
    let start = std::time::Instant::now();
    let mut output = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(50),
        unifi_cli::commands::events::watch_to(
            &mut stream,
            true,
            Some(45),
            None,
            &mut output,
            std::future::pending(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(start.elapsed() >= Duration::from_secs(45));
    assert!(output.is_empty());
    stream.close().await;
}

const MFA_COOKIE: &str = "mfa_cookie=fake-challenge-cookie";
const MFA_CODE: &str = "123456";

async fn mfa_fixture(local: bool, completion_status: u16) -> MockServer {
    mfa_fixture_with_cookie(local, completion_status, MFA_COOKIE).await
}

async fn mfa_fixture_with_cookie(local: bool, completion_status: u16, cookie: &str) -> MockServer {
    let server = MockServer::start().await;
    let (status, body) = if local {
        (
            200,
            serde_json::json!({"meta":{"rc":"error","msg":"api.err.Ubic2faTokenRequired"},"data":[{"private":"PRIVATE"}]}),
        )
    } else {
        (
            499,
            serde_json::json!({"code":"MFA_AUTH_REQUIRED","data":{"mfaCookie":cookie},"private":"PRIVATE"}),
        )
    };
    Mock::given(method("POST"))
        .and(path("/api/auth/login"))
        .and(body_json(serde_json::json!({"username":"test-user","password":"fake-password","rememberMe":false})))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .expect(1)
        .mount(&server).await;
    let mut completion_body =
        serde_json::json!({"username":"test-user","password":"fake-password","rememberMe":false});
    completion_body[if local { "ubic_2fa_token" } else { "token" }] = MFA_CODE.into();
    let response = if completion_status == 200 {
        ResponseTemplate::new(200)
            .insert_header(
                "set-cookie",
                format!("TOKEN={TOKEN}; Partitioned; HttpOnly; Secure"),
            )
            .insert_header("x-csrf-token", CSRF)
    } else {
        ResponseTemplate::new(completion_status)
            .set_body_string("PRIVATE fake-challenge-cookie 123456 fake-password")
            .insert_header("location", "https://other.example/should-not-follow")
    };
    Mock::given(method("POST"))
        .and(path("/api/auth/login"))
        .and(body_json(completion_body))
        .respond_with(response)
        .mount(&server)
        .await;
    Mock::given(path("/proxy/network/api/self/sites"))
        .and(header("cookie", format!("TOKEN={TOKEN}")))
        .and(header("x-csrf-token", CSRF))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"meta":{"rc":"ok"},"data":[]})),
        )
        .mount(&server)
        .await;
    server
}

async fn begin_mfa(server: &MockServer) -> unifi_cli::api::session::MfaChallenge {
    match NetworkSession::begin_login(
        &server.uri(),
        "test-user",
        "fake-password",
        ClientOptions::default(),
    )
    .await
    .unwrap()
    {
        unifi_cli::api::session::LoginOutcome::MfaRequired(challenge) => challenge,
        _ => panic!("expected MFA challenge"),
    }
}

fn assert_no_auth_material(text: &str) {
    for secret in [
        MFA_CODE,
        "fake-challenge-cookie",
        TOKEN,
        CSRF,
        "fake-password",
        "PRIVATE",
    ] {
        assert!(
            !text.contains(secret),
            "authentication material escaped redaction"
        );
    }
    assert!(!text.contains("Integrations"));
    assert!(!text.contains("Create or replace the key"));
}

#[tokio::test]
async fn mfa_completes_sso_and_local_authenticator_challenges() {
    for local in [false, true] {
        let server = mfa_fixture(local, 200).await;
        let session = begin_mfa(&server)
            .await
            .complete("test-user", "fake-password", MFA_CODE)
            .await
            .unwrap();
        session.check().await.unwrap();
        let stored = zeroize::Zeroizing::new(serde_json::to_string(session.secret()).unwrap());
        for transient in [
            MFA_CODE,
            "fake-challenge-cookie",
            "fake-password",
            "PRIVATE",
        ] {
            assert!(!stored.contains(transient));
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        let second = &requests[1];
        assert_eq!(
            second.headers.get("cookie").map(|v| v.to_str().unwrap()),
            (!local).then_some(MFA_COOKIE)
        );
        for request in &requests {
            assert_eq!(request.headers["user-agent"], "unifi-cli");
            assert!(!request.headers.contains_key("x-api-key"));
        }
    }
}

#[tokio::test]
async fn mfa_rejection_is_redacted_and_never_retried_or_redirected() {
    for status in [400, 401, 403, 499, 429, 500, 307] {
        let server = mfa_fixture(false, status).await;
        let error = begin_mfa(&server)
            .await
            .complete("test-user", "fake-password", MFA_CODE)
            .await
            .err()
            .unwrap();
        assert_no_auth_material(&error.to_string());
        assert_eq!(
            matches!(error, ApiError::SessionAuth(_)),
            matches!(status, 400 | 401 | 403 | 499)
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        if status == 307 {
            assert!(error.to_string().contains("redirect"));
        }
    }
}

#[tokio::test]
async fn malformed_mfa_challenges_fail_before_code_submission() {
    for body in [
        serde_json::json!({"data":{}}).to_string(),
        serde_json::json!({"data":{"mfaCookie":"mfa_cookie=bad value"}}).to_string(),
        serde_json::json!({"data":{"mfaCookie":"mfa_cookie=PRIVATE\r\nX-Injected: yes"}})
            .to_string(),
        serde_json::json!({"data":{"mfaCookie":"=PRIVATE"}}).to_string(),
        "PRIVATE invalid json".into(),
        "PRIVATE".repeat(10000),
    ] {
        let server = MockServer::start().await;
        Mock::given(path("/api/auth/login"))
            .respond_with(ResponseTemplate::new(499).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;
        let error = NetworkSession::begin_login(
            &server.uri(),
            "test-user",
            "fake-password",
            ClientOptions::default(),
        )
        .await
        .err()
        .unwrap();
        assert_no_auth_material(&error.to_string());
        assert!(matches!(error, ApiError::SessionAuth(_)));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn invalid_mfa_codes_are_rejected_locally() {
    for code in [
        "",
        "12345",
        "1234567",
        "123456789",
        "abcdef",
        "123\n45",
        "１２３４５６",
    ] {
        let server = mfa_fixture(false, 200).await;
        let error = begin_mfa(&server)
            .await
            .complete("test-user", "fake-password", code)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("not submitted"));
        assert_no_auth_material(&error.to_string());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_mfa_uses_environment_code_or_explains_missing_interactive_input() {
    for supplied in [false, true] {
        let server = mfa_fixture(false, 200).await;
        let host = server.uri();
        let output = tokio::task::spawn_blocking(move || {
            let (_root, mut command) = isolated_cli();
            command.args([
                "--host",
                &host,
                "--username",
                "test-user",
                "--password",
                "fake-password",
                "events",
                "watch",
                "--login",
                "--site",
                "office",
                "--duration",
                "1",
            ]);
            if supplied {
                command.env("UNIFI_MFA_CODE", MFA_CODE);
            }
            command.output().unwrap()
        })
        .await
        .unwrap();
        // With a code we reach the WebSocket endpoint (not provided by this
        // fixture); without a code we stop after the initial challenge.
        assert!(!output.status.success());
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), if supplied { 3 } else { 1 });
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_no_auth_material(&stderr);
        if supplied {
            assert!(requests[2].url.path().contains("/wss/s/office/events"));
        } else {
            assert!(stderr.contains("UNIFI_MFA_CODE"));
            assert!(stderr.contains("events watch --login"));
            assert_eq!(
                output.status.code(),
                Some(unifi_cli::output::exit_codes::AUTH_ERROR)
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Opt-in native store integration: fake MFA login, status and logout in separate CLI processes"]
async fn native_mfa_login_saves_only_the_resulting_session() {
    use unifi_cli::auth::{OsSessionStore, SessionStore};
    let server = mfa_fixture(false, 200).await;
    let profile = format!(
        "test-mfa-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let store = OsSessionStore::new(&server.uri(), Some(&profile)).unwrap();
    struct Cleanup<'a>(&'a OsSessionStore);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = self.0.delete();
        }
    }
    let _cleanup = Cleanup(&store);
    for action in ["login", "status", "logout"] {
        let host = server.uri();
        let profile = profile.clone();
        let output = tokio::task::spawn_blocking(move || {
            let (_root, mut command) = isolated_cli();
            // Native Keychain requires the real home. The unique absent
            // profile prevents using any configured personal credentials.
            if let Some(home) = std::env::var_os("HOME") {
                command.env("HOME", home);
            }
            command.args(["--host", &host, "--profile", &profile, "--output", "json"]);
            if action == "login" {
                command
                    .args(["--username", "test-user", "--password", "fake-password"])
                    .env("UNIFI_MFA_CODE", MFA_CODE);
            }
            command.args(["auth", action]).output().unwrap()
        })
        .await
        .unwrap();
        assert!(
            output.status.success(),
            "{action}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_no_auth_material(&String::from_utf8_lossy(&output.stdout));
        assert_no_auth_material(&String::from_utf8_lossy(&output.stderr));
        if action == "login" {
            let session = store.load().unwrap().unwrap();
            let stored = zeroize::Zeroizing::new(serde_json::to_string(&session).unwrap());
            for transient in [MFA_CODE, "fake-challenge-cookie", "fake-password"] {
                assert!(!stored.contains(transient));
            }
        }
    }
    assert!(store.load().unwrap().is_none());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path() == "/api/auth/login")
            .count(),
        2
    );
}

#[tokio::test]
async fn mfa_cookie_attributes_are_removed_before_submission() {
    for suffix in [
        "; path=/; samesite=none; secure; httponly",
        "; another=PRIVATE",
    ] {
        let cookie = format!("{MFA_COOKIE}{suffix}");
        let server = mfa_fixture_with_cookie(false, 200, &cookie).await;
        begin_mfa(&server)
            .await
            .complete("test-user", "fake-password", MFA_CODE)
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].headers["cookie"], MFA_COOKIE);
    }
}

#[tokio::test]
async fn login_access_failure_explains_network_permissions_without_relogin_loop() {
    for status in [200, 401, 403, 404] {
        let server = MockServer::start().await;
        Mock::given(path("/proxy/network/api/self/sites"))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_json(serde_json::json!({"meta":{"rc":"error"},"private":"PRIVATE"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let session = NetworkSession::from_secret(
            &server.uri(),
            secret(&server.uri()),
            ClientOptions::default(),
        )
        .unwrap();
        let error = session.check_after_login().await.unwrap_err();
        assert_no_auth_material(&error.to_string());
        assert!(!error.to_string().contains("`unifi auth login`"));
        if status == 404 {
            assert!(matches!(error, ApiError::Unsupported { .. }));
        } else {
            assert!(matches!(error, ApiError::SessionAuth(_)));
            assert!(error.to_string().contains("Network"));
            assert!(error.to_string().contains("role"));
        }
    }
}
