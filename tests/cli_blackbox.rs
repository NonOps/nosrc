//! Black-box tests that launch the real `nosrc` executable for the default HTTP mode,
//! `export`, and `serve --bundle`, against a local mock model provider.

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::process::{Child, Command};

const SPEC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/minimal/SPEC.md");
const MODEL: &str = "blackbox-model";
const READY_TIMEOUT: Duration = Duration::from_secs(240);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(240);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

struct Provider {
    base_url: String,
    requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Provider {
    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn completion() -> Value {
    let synthesis = json!({
        "handler_code": r#"
            export function handle(request) {
                if (request.method === "GET" && request.path === "/hello") {
                    return {
                        status: 200,
                        headers: [{ name: "content-type", value: new Uint8Array([97, 112, 112, 108, 105, 99, 97, 116, 105, 111, 110, 47, 106, 115, 111, 110]) }],
                        body: { tag: "text", val: JSON.stringify({ message: "hello" }) }
                    };
                }
                return {
                    status: 404,
                    headers: [{ name: "content-type", value: new Uint8Array([116, 101, 120, 116, 47, 112, 108, 97, 105, 110]) }],
                    body: { tag: "text", val: "not found" }
                };
            }
        "#,
        "code_format": "javascript",
        "assertions": [{
            "name": "generated hello assertion",
            "request": {
                "method": "GET",
                "scheme": null,
                "authority": null,
                "path": "/hello",
                "query": null,
                "headers": [],
                "body": []
            },
            "expected_status": 200,
            "body_contains": "hello"
        }],
        "rationale": "Deterministic handler returned by the black-box test provider"
    });
    json!({
        "choices": [{ "message": { "content": serde_json::to_string(&synthesis).unwrap() } }]
    })
}

fn completion_with_handler(handler_code: &str) -> Value {
    let mut response = completion();
    let content = response["choices"][0]["message"]["content"]
        .as_str()
        .expect("mock completion content must be a string");
    let mut synthesis: Value = serde_json::from_str(content).unwrap();
    synthesis["handler_code"] = json!(handler_code);
    response["choices"][0]["message"]["content"] =
        json!(serde_json::to_string(&synthesis).unwrap());
    response
}

async fn start_provider() -> Provider {
    start_provider_with_completion(completion()).await
}

async fn start_provider_with_completion(response: Value) -> Provider {
    let requests = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/v1/chat/completions",
        post({
            let requests = requests.clone();
            move || {
                let requests = requests.clone();
                let response = response.clone();
                async move {
                    requests.fetch_add(1, Ordering::SeqCst);
                    Json(response)
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Provider {
        base_url: format!("http://{address}/v1"),
        requests,
        task,
    }
}

/// A `nosrc` command with every provider variable cleared.
fn nosrc() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nosrc"));
    command
        .env_remove("LLM_API_KEY")
        .env_remove("LLM_BASE_URL")
        .env_remove("LLM_MODEL")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

/// Point the command at the mock provider, optionally with an API key.
fn with_provider(command: &mut Command, provider: &Provider, with_key: bool) {
    command
        .env("LLM_BASE_URL", &provider.base_url)
        .env("LLM_MODEL", MODEL);
    if with_key {
        command.env("LLM_API_KEY", "blackbox-test-key");
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn run_to_completion(mut command: Command) -> std::process::Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    tokio::time::timeout(COMMAND_TIMEOUT, command.output())
        .await
        .expect("nosrc did not exit in time")
        .expect("failed to run nosrc")
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

async fn wait_until_serving(child: &mut Child, port: u16) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("nosrc exited before it started serving: {status}");
        }
        if let Ok(response) = client
            .get(format!("http://127.0.0.1:{port}/healthz"))
            .send()
            .await
        {
            if response.status().is_success() {
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "nosrc did not start serving on port {port} in time"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("failed to build bounded HTTP test client")
}

async fn get_json(port: u16, path: &str) -> (u16, Value) {
    let response = http_client()
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await
        .expect("HTTP test request timed out or failed");
    let status = response.status().as_u16();
    (status, response.json().await.unwrap())
}

async fn export_bundle(provider: &Provider, out_dir: &Path, cache_dir: &Path) {
    let mut command = nosrc();
    with_provider(&mut command, provider, true);
    command
        .arg("export")
        .arg("--spec")
        .arg(SPEC)
        .arg("--out-dir")
        .arg(out_dir)
        .arg("--cache-dir")
        .arg(cache_dir);
    let output = run_to_completion(command).await;
    assert!(
        output.status.success(),
        "export failed: {}",
        stderr_of(&output)
    );
}

#[tokio::test]
async fn export_writes_a_bundle_that_serve_runs_without_credentials() {
    let provider = start_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let bundle = directory.path().join("dist");

    export_bundle(&provider, &bundle, &directory.path().join("cache")).await;
    assert_eq!(provider.requests(), 1);

    for file in [
        "SPEC.md",
        "handler.js",
        "component.wasm",
        "assertions.json",
        "manifest.json",
    ] {
        assert!(bundle.join(file).is_file(), "missing exported {file}");
    }
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(bundle.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["policy_mode"], "operator");
    assert_eq!(manifest["trusted_assertions_count"], 1);
    assert_eq!(manifest["generated_assertions_count"], 1);

    // No provider variables at all: serving a bundle must not need credentials.
    let port = free_port();
    let mut command = nosrc();
    command
        .arg("serve")
        .arg("--bundle")
        .arg(&bundle)
        .arg("--port")
        .arg(port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut server = command.spawn().expect("failed to start nosrc serve");
    wait_until_serving(&mut server, port).await;

    let (status, body) = get_json(port, "/hello").await;
    assert_eq!(status, 200);
    assert_eq!(body, json!({ "message": "hello" }));
    let (status, body) = get_json(port, "/_nosrc/status").await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "running");
    assert!(body["active_generation"].is_number());
    assert_eq!(body["ready"], true);

    server.kill().await.unwrap();
    assert_eq!(
        provider.requests(),
        1,
        "serve must not contact the provider"
    );
}

#[tokio::test]
async fn default_http_mode_synthesizes_then_serves_from_cache_without_an_api_key() {
    let provider = start_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("cache");

    // First start: cache miss, so the provider is called once.
    let port = free_port();
    let mut command = nosrc();
    with_provider(&mut command, &provider, true);
    command
        .arg("--spec")
        .arg(SPEC)
        .arg("--port")
        .arg(port.to_string())
        .arg("--cache-dir")
        .arg(&cache)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut server = command.spawn().expect("failed to start nosrc");
    wait_until_serving(&mut server, port).await;
    let (status, body) = get_json(port, "/hello").await;
    assert_eq!(status, 200);
    assert_eq!(body, json!({ "message": "hello" }));
    server.kill().await.unwrap();
    assert_eq!(provider.requests(), 1);

    // Second start: same provider settings but no API key. The verified cache entry is
    // reused, so startup succeeds and the provider is not contacted again.
    let port = free_port();
    let mut command = nosrc();
    with_provider(&mut command, &provider, false);
    command
        .arg("--spec")
        .arg(SPEC)
        .arg("--port")
        .arg(port.to_string())
        .arg("--cache-dir")
        .arg(&cache)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut server = command.spawn().expect("failed to start nosrc");
    wait_until_serving(&mut server, port).await;
    let (status, body) = get_json(port, "/hello").await;
    assert_eq!(status, 200);
    assert_eq!(body, json!({ "message": "hello" }));
    server.kill().await.unwrap();
    assert_eq!(provider.requests(), 1);
}

#[tokio::test]
async fn export_without_api_key_or_cache_fails_before_contacting_the_provider() {
    let provider = start_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let bundle = directory.path().join("dist");

    let mut command = nosrc();
    with_provider(&mut command, &provider, false);
    command
        .arg("export")
        .arg("--spec")
        .arg(SPEC)
        .arg("--out-dir")
        .arg(&bundle)
        .arg("--cache-dir")
        .arg(directory.path().join("cache"));
    let output = run_to_completion(command).await;

    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("LLM_API_KEY"),
        "unexpected stderr: {}",
        stderr_of(&output)
    );
    assert_eq!(provider.requests(), 0);
    assert!(
        !bundle.exists(),
        "a failed export must not create the bundle"
    );
}

#[tokio::test]
async fn export_refuses_to_overwrite_an_existing_destination() {
    let provider = start_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let bundle = directory.path().join("dist");
    std::fs::create_dir(&bundle).unwrap();
    std::fs::write(bundle.join("keep.txt"), "do not touch").unwrap();

    let mut command = nosrc();
    with_provider(&mut command, &provider, true);
    command
        .arg("export")
        .arg("--spec")
        .arg(SPEC)
        .arg("--out-dir")
        .arg(&bundle)
        .arg("--cache-dir")
        .arg(directory.path().join("cache"));
    let output = run_to_completion(command).await;

    assert!(!output.status.success());
    assert_eq!(
        std::fs::read_to_string(bundle.join("keep.txt")).unwrap(),
        "do not touch"
    );
    assert!(!bundle.join("manifest.json").exists());
}

#[tokio::test]
async fn serve_rejects_a_tampered_component() {
    let provider = start_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let bundle = directory.path().join("dist");
    export_bundle(&provider, &bundle, &directory.path().join("cache")).await;

    let component = bundle.join("component.wasm");
    let mut bytes = std::fs::read(&component).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&component, bytes).unwrap();

    let port = free_port();
    let mut command = nosrc();
    command
        .arg("serve")
        .arg("--bundle")
        .arg(&bundle)
        .arg("--port")
        .arg(port.to_string());
    let output = run_to_completion(command).await;

    assert!(
        !output.status.success(),
        "serve must refuse a bundle whose component digest does not match"
    );
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "offline listener opened for a bundle that failed integrity validation"
    );
}

#[tokio::test]
async fn serve_rejects_a_missing_bundle_directory() {
    let directory = tempfile::tempdir().unwrap();
    let mut command = nosrc();
    command
        .arg("serve")
        .arg("--bundle")
        .arg(directory.path().join("does-not-exist"))
        .arg("--port")
        .arg(free_port().to_string());
    let output = run_to_completion(command).await;
    assert!(!output.status.success());
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_stops_default_http_and_offline_bundle_modes() {
    let provider = start_provider().await;
    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("cache");

    // The default HTTP mode must stop accepting requests and exit after SIGTERM.
    let http_port = free_port();
    let mut command = nosrc();
    with_provider(&mut command, &provider, true);
    command
        .arg("--spec")
        .arg(SPEC)
        .arg("--port")
        .arg(http_port.to_string())
        .arg("--cache-dir")
        .arg(&cache)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut http_server = command.spawn().expect("failed to start default HTTP mode");
    wait_until_serving(&mut http_server, http_port).await;

    let pid = http_server.id().expect("HTTP child must have a PID") as libc::pid_t;
    let result = unsafe { libc::kill(pid, libc::SIGTERM) };
    assert_eq!(result, 0, "failed to send SIGTERM to default HTTP mode");
    let status = tokio::time::timeout(Duration::from_secs(10), http_server.wait())
        .await
        .expect("default HTTP mode did not exit after SIGTERM")
        .unwrap();
    assert!(
        status.success(),
        "default HTTP mode exited unsuccessfully: {status}"
    );
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", http_port))
            .await
            .is_err(),
        "default HTTP listener remained open after SIGTERM"
    );

    // Export once, then verify the independent offline serving process has the
    // same externally observable shutdown behavior without provider credentials.
    let bundle = directory.path().join("offline-bundle");
    export_bundle(&provider, &bundle, &directory.path().join("export-cache")).await;
    let offline_port = free_port();
    let mut command = nosrc();
    command
        .arg("serve")
        .arg("--bundle")
        .arg(&bundle)
        .arg("--port")
        .arg(offline_port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut offline_server = command
        .spawn()
        .expect("failed to start offline bundle serving mode");
    wait_until_serving(&mut offline_server, offline_port).await;

    let pid = offline_server
        .id()
        .expect("offline serving child must have a PID") as libc::pid_t;
    let result = unsafe { libc::kill(pid, libc::SIGTERM) };
    assert_eq!(result, 0, "failed to send SIGTERM to offline serving mode");
    let status = tokio::time::timeout(Duration::from_secs(10), offline_server.wait())
        .await
        .expect("offline serving mode did not exit after SIGTERM")
        .unwrap();
    assert!(
        status.success(),
        "offline serving mode exited unsuccessfully: {status}"
    );
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", offline_port))
            .await
            .is_err(),
        "offline HTTP listener remained open after SIGTERM"
    );

    assert_eq!(
        provider.requests(),
        2,
        "only the default HTTP startup and export should contact the provider"
    );
}

#[tokio::test]
async fn default_http_guest_trap_is_isolated_and_instance_recovers() {
    let handler = r#"
        export function handle(request) {
            if (request.path === "/trap") {
                throw new Error("guest-controlled trap detail must stay private");
            }
            if (request.path === "/hello") {
                return {
                    status: 200,
                    headers: [{ name: "content-type", value: new Uint8Array([97, 112, 112, 108, 105, 99, 97, 116, 105, 111, 110, 47, 106, 115, 111, 110]) }],
                    body: { tag: "text", val: JSON.stringify({ message: "hello" }) }
                };
            }
            return { status: 404, headers: [], body: { tag: "text", val: "not found" } };
        }
    "#;
    let provider = start_provider_with_completion(completion_with_handler(handler)).await;
    let directory = tempfile::tempdir().unwrap();
    let port = free_port();
    let mut command = nosrc();
    with_provider(&mut command, &provider, true);
    command
        .arg("--spec")
        .arg(SPEC)
        .arg("--port")
        .arg(port.to_string())
        .arg("--cache-dir")
        .arg(directory.path().join("cache"))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut server = command.spawn().expect("failed to start default HTTP mode");
    wait_until_serving(&mut server, port).await;

    let trapped = http_client()
        .get(format!("http://127.0.0.1:{port}/trap"))
        .send()
        .await
        .expect("guest-trap HTTP request timed out or failed");
    assert!(
        !trapped.status().is_success(),
        "guest trap unexpectedly returned success"
    );
    let trap_body = trapped.text().await.unwrap();
    assert!(
        !trap_body.contains("guest-controlled trap detail"),
        "raw guest trap text leaked through HTTP: {trap_body}"
    );

    let (status, body) = get_json(port, "/hello").await;
    assert_eq!(status, 200);
    assert_eq!(body, json!({ "message": "hello" }));

    let (status, runtime) = get_json(port, "/_nosrc/status").await;
    assert_eq!(status, 200);
    assert_eq!(runtime["telemetry"]["instance_recreations"], 1);
    assert_eq!(provider.requests(), 1);

    server.kill().await.unwrap();
    let _ = server.wait().await;
}
