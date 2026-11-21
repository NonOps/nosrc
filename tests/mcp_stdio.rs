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
    assert!(names.contains(&"export_app"));

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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Send raw bytes to the HTTP listener and return everything the server answers.
async fn raw_http(port: u16, request: &[u8]) -> String {
    use tokio::io::AsyncReadExt;

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    // The server may close the connection before reading an oversized request, so a
    // failed write is an acceptable outcome here.
    let _ = stream.write_all(request).await;
    let mut response = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(15), stream.read_to_end(&mut response)).await;
    String::from_utf8_lossy(&response).into_owned()
}

fn simple_get(path: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").into_bytes()
}

#[tokio::test]
async fn shared_http_listener_serves_the_generation_activated_over_mcp() {
    let spec = r#"
# Service: MCP shared listener test

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
    let http_port = free_port();

    let mut child = Command::new(env!("CARGO_BIN_EXE_nosrc"))
        .arg("mcp")
        .arg("--spec")
        .arg(&spec_path)
        .arg("--http-port")
        .arg(http_port.to_string())
        .arg("--no-cache")
        .arg("--export-root")
        .arg(directory.path().join("exports"))
        .env_remove("LLM_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to start nosrc MCP server");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stdout = BufReader::new(stdout).lines();

    send_request(
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
    send_notification(
        &mut stdin,
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    // The listener comes up with the process, before any generation is active.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let health = raw_http(http_port, &simple_get("/healthz")).await;
        if health.starts_with("HTTP/1.1 200") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "HTTP listener did not start"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let before = raw_http(http_port, &simple_get("/health")).await;
    assert!(before.starts_with("HTTP/1.1 503"), "{before}");

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
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "prepare_app",
                "arguments": { "source": "direct_js", "js_code": js_code }
            }
        }),
    )
    .await;
    let generation_id = structured_result(&prepared)["generation_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Preparing a generation does not activate it.
    let still_inactive = raw_http(http_port, &simple_get("/health")).await;
    assert!(
        still_inactive.starts_with("HTTP/1.1 503"),
        "{still_inactive}"
    );

    let activated = send_request(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "activate_app",
                "arguments": { "generation_id": generation_id }
            }
        }),
    )
    .await;
    assert_eq!(structured_result(&activated)["state"], "active");

    let after = raw_http(http_port, &simple_get("/health")).await;
    assert!(after.starts_with("HTTP/1.1 200"), "{after}");
    assert!(after.ends_with("healthy"), "{after}");

    // A header far beyond any sane limit must never reach the handler.
    let mut oversized =
        b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nX-Big: ".to_vec();
    oversized.extend(std::iter::repeat_n(b'a', 2 * 1024 * 1024));
    oversized.extend_from_slice(b"\r\n\r\n");
    let rejected = raw_http(http_port, &oversized).await;
    assert!(
        !rejected.starts_with("HTTP/1.1 200"),
        "oversized header was accepted: {}",
        &rejected[..rejected.len().min(200)]
    );

    // The server stays healthy after rejecting the oversized request.
    let health = raw_http(http_port, &simple_get("/healthz")).await;
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");

    child.kill().await.unwrap();
    let _ = child.wait().await;
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_stops_shared_http_and_mcp_process() {
    let directory = tempfile::tempdir().unwrap();
    let http_port = free_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nosrc"))
        .arg("mcp")
        .arg("--http-port")
        .arg(http_port.to_string())
        .arg("--shutdown-timeout")
        .arg("1")
        .arg("--no-cache")
        .arg("--export-root")
        .arg(directory.path().join("exports"))
        .env_remove("LLM_API_KEY")
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
                "clientInfo": { "name": "nosrc-shutdown-test", "version": "0.1.0" }
            }
        }),
    )
    .await;
    assert_eq!(initialized["id"], 1);
    send_notification(
        &mut stdin,
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", http_port))
            .await
            .is_ok()
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "shared HTTP listener did not start"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let health = raw_http(http_port, &simple_get("/healthz")).await;
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");

    let pid = child.id().expect("child process must have a PID") as libc::pid_t;
    let signal_result = unsafe { libc::kill(pid, libc::SIGTERM) };
    assert_eq!(signal_result, 0, "failed to send SIGTERM to nosrc");

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("nosrc did not exit after SIGTERM")
        .unwrap();
    assert!(status.success(), "nosrc exited unsuccessfully: {status}");

    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", http_port))
            .await
            .is_err(),
        "shared HTTP listener remained open after process shutdown"
    );
}
