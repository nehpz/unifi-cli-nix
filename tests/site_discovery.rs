use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_failures_exit_with_structured_errors_instead_of_panicking() {
    let server = MockServer::start().await;
    let cases = [
        (
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"totalCount": 0, "data": []})),
            1,
            "general_error",
        ),
        (
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"totalCount": 1, "data": [{"id": null}]})),
            1,
            "general_error",
        ),
        (
            ResponseTemplate::new(200).set_body_raw("{broken", "application/json"),
            1,
            "general_error",
        ),
        (ResponseTemplate::new(403), 3, "auth_error"),
    ];

    for (response, exit_code, kind) in cases {
        Mock::given(method("GET"))
            .and(path("/proxy/network/integration/v1/sites"))
            .respond_with(response)
            .expect(1)
            .mount(&server)
            .await;

        let output = std::process::Command::new(env!("CARGO_BIN_EXE_unifi"))
            .args([
                "--host",
                &server.uri(),
                "--api-key",
                "test-key",
                "--output",
                "json",
                "clients",
                "list",
            ])
            .output()
            .expect("run unifi against the mock controller");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(exit_code), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
        let error: serde_json::Value =
            serde_json::from_slice(&output.stderr).expect("stderr must be a JSON error envelope");
        assert_eq!(error["error"]["kind"], kind, "{error}");
        server.verify().await;
        server.reset().await;
    }
}
