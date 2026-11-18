use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout, Command};

async fn send_request(
    stdin: &mut ChildStdin,
    stdout: &mut Lines<BufReader<ChildStdout>>,
    request: Value,
) -> Value {
    let mut bytes = serde_json::to_vec(&request).unwrap();
    bytes.push(b'\n');
    stdin.write_all(&bytes).await.unwrap();
    stdin.flush().await.unwrap();

    let line = tokio::time::timeout(Duration::from_secs(120), stdout.next_line())
        .await
        .expect("timed out waiting for MCP response")
        .unwrap()
        .expect("MCP server closed stdout");
    serde_json::from_str(&line).unwrap()
}

async fn send_notification(stdin: &mut ChildStdin, notification: Value) {
    let mut bytes = serde_json::to_vec(&notification).unwrap();
    bytes.push(b'\n');
    stdin.write_all(&bytes).await.unwrap();
    stdin.flush().await.unwrap();
}

fn structured_result(response: &Value) -> Value {
    if let Some(value) = response.pointer("/result/structuredContent") {
        return value.clone();
    }
    let text = response
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .expect("MCP tool response did not contain structured content or text");
    serde_json::from_str(text).unwrap()
}

#[tokio::test]
async fn stdio_protocol_supports_prepare_status_activate_and_invoke() {
    let spec = r#"
# Service: MCP stdio integration test

## Capabilities
- state: volatile
- egress: []

## Endpoints
### `GET /health`
- Response `200`: `healthy`

## Verification
### `GET /health`
- Response `200`
- Body contains: `healthy`
"#;
    let directory = tempfile::tempdir().unwrap();
    let spec_path = directory.path().join("SPEC.md");
    std::fs::write(&spec_path, spec).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_nosrc"))
        .arg("mcp")
        .arg("--spec")
        .arg(&spec_path)
        .arg("--no-cache")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to start nosrc MCP server");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stdout = BufReader::new(stdout).lines();

    let initialized = send_request(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "nosrc-integration-test", "version": "0.1.0" }
            }
        }),
    )
    .await;
    assert_eq!(initialized["id"], 1);
    assert!(initialized["result"]["protocolVersion"].is_string());
    assert_eq!(initialized["result"]["serverInfo"]["name"], "nosrc");
    assert_eq!(
        initialized["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );

    send_notification(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    )
    .await;

    let tools = send_request(
        &mut stdin,
        &mut stdout,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"prepare_app"));
    assert!(names.contains(&"get_app_status"));
    assert!(names.contains(&"activate_app"));
    assert!(names.contains(&"invoke_app"));

    let js_code = r#"
        export function handle(requestJson) {
            const request = JSON.parse(requestJson);
            if (request.path === "/health") {
                return JSON.stringify({
                    status: 200,
                    headers: { "content-type": "text/plain" },
                    body: "healthy"
                });
            }
            return JSON.stringify({ status: 404, headers: {}, body: "not found" });
        }
    "#;
    let prepared = send_request(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "prepare_app",
                "arguments": { "source": "direct_js", "js_code": js_code }
            }
        }),
    )
    .await;
    assert_eq!(prepared["id"], 3);
    assert_ne!(prepared["result"]["isError"], true);
    let prepared_data = structured_result(&prepared);
    let generation_id = prepared_data["generation_id"].as_str().unwrap().to_string();
    assert_eq!(prepared_data["state"], "ready");
    assert_eq!(prepared_data["trusted_assertions_passed"], 1);

    let status = send_request(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "get_app_status", "arguments": {} }
        }),
    )
    .await;
    let status_data = structured_result(&status);
    assert_eq!(status_data["active_generation"], Value::Null);
    assert_eq!(status_data["generations"][0]["state"], "ready");
    assert!(
        status_data["generations"][0]["trusted_assertions"][0]["passed"]
            .as_bool()
            .unwrap()
    );

    let activated = send_request(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "activate_app",
                "arguments": { "generation_id": generation_id.clone() }
            }
        }),
    )
    .await;
    assert_eq!(structured_result(&activated)["state"], "active");

    let invoked = send_request(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "invoke_app",
                "arguments": { "path": "/health" }
            }
        }),
    )
    .await;
    let response = structured_result(&invoked);
    assert_eq!(response["status"], 200);
    assert_eq!(response["body"], "healthy");

    let final_status = send_request(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "get_app_status", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(
        structured_result(&final_status)["active_generation"].as_str(),
        Some(generation_id.as_str())
    );

    child.kill().await.unwrap();
    let _ = child.wait().await;
}
