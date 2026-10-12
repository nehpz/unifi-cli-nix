use super::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SITES: &str = "/proxy/network/integration/v1/sites";

async fn mount(server: &MockServer, response: ResponseTemplate, requests: u64) {
    Mock::given(method("GET"))
        .and(path(SITES))
        .respond_with(response)
        .expect(requests)
        .mount(server)
        .await;
}

fn success() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "totalCount": 2,
        "data": [{"id": "first-site"}, {"id": "second-site"}]
    }))
}

#[tokio::test]
async fn discovery_selects_first_site_and_caches_without_another_request() {
    let server = MockServer::start().await;
    mount(&server, success(), 1).await;
    let mut client = UnifiClient::new(&server.uri(), "test-key").unwrap();

    assert_eq!(client.ensure_site_id().await.unwrap(), "first-site");
    // Even an unavailable controller must not matter once discovery is cached.
    assert_eq!(client.ensure_site_id().await.unwrap(), "first-site");
    assert_eq!(client.site_id.as_deref(), Some("first-site"));
}

#[tokio::test]
async fn discovery_errors_leave_cache_empty_and_allow_retry() {
    let server = MockServer::start().await;
    let mut client = UnifiClient::new(&server.uri(), "test-key").unwrap();
    let responses = [
        (ResponseTemplate::new(401), "auth"),
        (ResponseTemplate::new(403), "auth"),
        (ResponseTemplate::new(404), "not-found"),
        (ResponseTemplate::new(500), "api"),
        (ResponseTemplate::new(204), "unsupported"),
        (
            ResponseTemplate::new(200).set_body_string("<html>login</html>"),
            "unsupported",
        ),
        (
            ResponseTemplate::new(200).set_body_raw("{broken", "application/json"),
            "decode",
        ),
        (
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"totalCount": 0, "data": []})),
            "no-sites",
        ),
        (
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"totalCount": 1, "data": [{}]})),
            "decode",
        ),
        (
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"totalCount": 1, "data": [{"id": null}]})),
            "decode",
        ),
    ];

    for (response, expected) in responses {
        mount(&server, response, 1).await;
        let error = client.ensure_site_id().await.unwrap_err();
        match (expected, &error) {
            ("auth", ApiError::Auth(_))
            | ("not-found", ApiError::NotFound(_))
            | ("api", ApiError::Api { status: 500, .. })
            | ("unsupported", ApiError::Unsupported { .. }) => {}
            ("decode", ApiError::Other(message)) => {
                assert!(message.contains("Failed to decode"), "{message}");
            }
            ("no-sites", ApiError::Other(message)) => {
                assert!(message.contains("No sites found"), "{message}");
            }
            _ => panic!("expected {expected}, got {error:?}"),
        }
        assert!(client.site_id.is_none());
        server.verify().await;
        server.reset().await;
    }

    mount(&server, success(), 1).await;
    assert_eq!(client.ensure_site_id().await.unwrap(), "first-site");
}

#[tokio::test]
async fn discovery_transport_failure_allows_retry() {
    let server = MockServer::start().await;
    let mut client = UnifiClient::new(&server.uri(), "test-key").unwrap();
    // An explicitly closed local port avoids relying on an external network.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    client.base_url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    assert!(matches!(
        client.ensure_site_id().await,
        Err(ApiError::Http(_))
    ));
    assert!(client.site_id.is_none());

    client.base_url = server.uri();
    mount(&server, success(), 1).await;
    assert_eq!(client.ensure_site_id().await.unwrap(), "first-site");
}

#[tokio::test]
async fn cancelling_discovery_leaves_cache_empty_and_allows_retry() {
    let server = MockServer::start().await;
    let mut client = UnifiClient::new(&server.uri(), "test-key").unwrap();
    // Signal receipt before dropping the pending future, rather than racing a
    // short timeout against dispatch on a busy CI machine.
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    Mock::given(method("GET"))
        .and(path(SITES))
        .respond_with(move |_: &wiremock::Request| {
            sent.send(()).unwrap();
            success().set_delay(std::time::Duration::from_secs(60))
        })
        .expect(1)
        .mount(&server)
        .await;
    {
        let discovery = client.ensure_site_id();
        tokio::pin!(discovery);
        tokio::select! {
            result = &mut discovery => panic!("delayed discovery finished: {result:?}"),
            signal = received.recv() => assert_eq!(signal, Some(())),
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                panic!("discovery request never arrived");
            }
        }
    }
    assert!(client.site_id.is_none());
    server.verify().await;
    server.reset().await;
    mount(&server, success(), 1).await;
    assert_eq!(client.ensure_site_id().await.unwrap(), "first-site");
}

/// Bounded, deterministic mutation testing of the actual HTTP boundary. This
/// is a repeatable regression test, not a coverage-guided fuzzing campaign.
#[tokio::test]
async fn discovery_mutated_responses_never_panic_or_corrupt_cache() {
    let server = MockServer::start().await;
    let seeds: &[&[u8]] = &[
        br#"{"totalCount":1,"data":[{"id":"site"}]}"#,
        br#"{"totalCount":0,"data":[]}"#,
        br#"{"totalCount":1,"data":[{"id":""}]}"#,
        br#"{"totalCount":1,"data":[{"id":null}]}"#,
        br#"{"totalCount":1,"data":[{"id":42}]}"#,
        br#"{"totalCount":1,"data":[{}]}"#,
        br#"{"totalCount":1,"data":null}"#,
        br#"{"totalCount":-1,"data":[{"id":"site"}]}"#,
        br#"{"totalCount":18446744073709551616,"data":[]}"#,
        br#"{"data":[{"id":"site"}]}"#,
        br#"{"totalCount":1,"data":[{"id":"a"},{"id":null}]}"#,
        br#"{"totalCount":1,"data":[{"id":"\u0000/../?x=#\ud83d\ude00"}]}"#,
        b"",
        b"null",
        b"[]",
        b"\xff\xfe\x00",
    ];
    let mut corpus: Vec<Vec<u8>> = seeds.iter().map(|seed| seed.to_vec()).collect();
    // Include every prefix of every seed to exercise truncated responses.
    for seed in seeds {
        for len in 0..seed.len() {
            corpus.push(seed[..len].to_vec());
        }
    }
    let mut random = 0x5eed_u64;
    for iteration in 0..1024 {
        let mut body = seeds[iteration % seeds.len()].to_vec();
        for _ in 0..1 + iteration % 8 {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let index = random as usize % (body.len() + 1);
            let byte = (random >> 32) as u8;
            if index == body.len() {
                body.push(byte);
            } else {
                body[index] = byte;
            }
        }
        corpus.push(body);
    }

    for (index, body) in corpus.iter().enumerate() {
        mount(
            &server,
            ResponseTemplate::new(200).set_body_raw(body.clone(), "application/json"),
            1,
        )
        .await;
        let mut client = UnifiClient::new(&server.uri(), "test-key").unwrap();
        match client.ensure_site_id().await.map(str::to_owned) {
            Ok(id) => {
                assert_eq!(client.site_id.as_deref(), Some(id.as_str()), "case {index}");
                assert_eq!(client.ensure_site_id().await.unwrap(), id, "case {index}");
            }
            Err(_) => assert!(client.site_id.is_none(), "case {index}: {body:?}"),
        }
        server.verify().await;
        server.reset().await;
    }
    eprintln!("checked {} discovery response mutations", corpus.len());
}
