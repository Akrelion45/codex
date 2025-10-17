use std::path::Path;

use app_test_support::McpProcess;
use app_test_support::to_response;
use chrono::DateTime;
use chrono::Utc;
use codex_app_server_protocol::GetAccountRateLimitsResponse;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::LoginApiKeyParams;
use codex_app_server_protocol::RequestId;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const INVALID_REQUEST_ERROR_CODE: i64 = -32600;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_account_rate_limits_requires_auth() {
    let codex_home = TempDir::new().unwrap_or_else(|err| panic!("create tempdir: {err}"));
    create_config_toml(codex_home.path(), None)
        .unwrap_or_else(|err| panic!("write config.toml: {err}"));

    let mut mcp = McpProcess::new_with_env(codex_home.path(), &[("OPENAI_API_KEY", None)])
        .await
        .expect("spawn mcp process");
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize())
        .await
        .expect("initialize timeout")
        .expect("initialize request");

    let request_id = mcp
        .send_get_account_rate_limits_request()
        .await
        .expect("send account/rateLimits/read");

    let error: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await
    .expect("account/rateLimits/read timeout")
    .expect("account/rateLimits/read error");

    assert_eq!(error.id, RequestId::Integer(request_id));
    assert_eq!(error.error.code, INVALID_REQUEST_ERROR_CODE);
    assert_eq!(
        error.error.message,
        "codex account authentication required to read rate limits"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_account_rate_limits_returns_snapshot() {
    let codex_home = TempDir::new().unwrap_or_else(|err| panic!("create tempdir: {err}"));
    let server = MockServer::start().await;

    let primary_reset_iso = "2025-01-01T00:02:00Z";
    let secondary_reset_iso = "2025-01-01T01:00:00Z";
    let response_body = json!({
        "plan_type": "pro",
        "rate_limit": {
            "allowed": true,
            "limit_reached": false,
            "primary_window": {
                "used_percent": 42,
                "limit_window_seconds": 3600,
                "reset_after_seconds": 120,
                "reset_at": primary_reset_iso,
            },
            "secondary_window": {
                "used_percent": 5,
                "limit_window_seconds": 86400,
                "reset_after_seconds": 43200,
                "reset_at": secondary_reset_iso,
            }
        }
    });

    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .and(header("authorization", "Bearer sk-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
        .mount(&server)
        .await;

    let base_url = server.uri();
    create_config_toml(codex_home.path(), Some(base_url.as_str()))
        .unwrap_or_else(|err| panic!("write config.toml: {err}"));

    let mut mcp = McpProcess::new(codex_home.path())
        .await
        .expect("spawn mcp process");
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize())
        .await
        .expect("initialize timeout")
        .expect("initialize request");

    login_with_api_key(&mut mcp, "sk-test-key").await;

    let request_id = mcp
        .send_get_account_rate_limits_request()
        .await
        .expect("send account/rateLimits/read");

    let response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await
    .expect("account/rateLimits/read timeout")
    .expect("account/rateLimits/read response");

    let received: GetAccountRateLimitsResponse =
        to_response(response).expect("deserialize rate limit response");
    let primary_reset_epoch = DateTime::parse_from_rfc3339(primary_reset_iso)
        .unwrap_or_else(|err| panic!("parse primary reset: {err}"))
        .with_timezone(&Utc)
        .timestamp();
    let secondary_reset_epoch = DateTime::parse_from_rfc3339(secondary_reset_iso)
        .unwrap_or_else(|err| panic!("parse secondary reset: {err}"))
        .with_timezone(&Utc)
        .timestamp();

    let expected = GetAccountRateLimitsResponse {
        rate_limits: RateLimitSnapshot {
            primary: Some(RateLimitWindow {
                used_percent: 42.0,
                window_minutes: Some(60),
                resets_in_seconds: Some(120),
                resets_at: Some(primary_reset_epoch),
            }),
            secondary: Some(RateLimitWindow {
                used_percent: 5.0,
                window_minutes: Some(1440),
                resets_in_seconds: Some(43_200),
                resets_at: Some(secondary_reset_epoch),
            }),
        },
    };
    assert_eq!(received, expected);
}

#[expect(clippy::expect_used)]
async fn login_with_api_key(mcp: &mut McpProcess, api_key: &str) {
    let request_id = mcp
        .send_login_api_key_request(LoginApiKeyParams {
            api_key: api_key.to_string(),
        })
        .await
        .expect("send loginApiKey");

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await
    .expect("loginApiKey timeout")
    .expect("loginApiKey response");
}

fn create_config_toml(codex_home: &Path, chatgpt_base_url: Option<&str>) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    let mut contents = String::from(
        r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "danger-full-access"
"#,
    );

    if let Some(base_url) = chatgpt_base_url {
        contents.push_str("chatgpt_base_url = \"");
        contents.push_str(base_url);
        contents.push_str("\"\n");
    }

    std::fs::write(config_toml, contents)
}
