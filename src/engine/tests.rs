use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Arc;

use crate::engine::host_abi::{
    is_private_or_restricted_ip, HostCapabilities, ERR_CAPABILITY_DISABLED,
};
use crate::engine::http_envelope::{ErrorEnvelope, RequestEnvelope};
use crate::engine::{WasmEngine, DEFAULT_FUEL_BUDGET};
use crate::error::HostAbiError;

#[test]
fn test_ssrf_ip_filtering() {
    let loopback: IpAddr = "127.0.0.1".parse().unwrap();
    assert!(is_private_or_restricted_ip(&loopback));

    let private_10: IpAddr = "10.0.0.1".parse().unwrap();
    let private_172: IpAddr = "172.16.0.1".parse().unwrap();
    let private_192: IpAddr = "192.168.1.1".parse().unwrap();
    assert!(is_private_or_restricted_ip(&private_10));
    assert!(is_private_or_restricted_ip(&private_172));
    assert!(is_private_or_restricted_ip(&private_192));

    let link_local: IpAddr = "169.254.169.254".parse().unwrap();
    assert!(is_private_or_restricted_ip(&link_local));

    let public_ip: IpAddr = "93.184.216.34".parse().unwrap();
    assert!(!is_private_or_restricted_ip(&public_ip));
}

#[test]
fn test_egress_allowlist_enforcement() {
    let mut allowlisted = HashSet::new();
    allowlisted.insert("api.github.com".to_string());

    let caps = HostCapabilities::new_external(allowlisted);

    let err = caps
        .validate_egress_url("https://evil.com/data")
        .unwrap_err();
    match err {
        HostAbiError::EgressForbidden { domain } => assert_eq!(domain, "evil.com"),
        other => panic!("Expected EgressForbidden, got: {:?}", other),
    }

    let err = caps
        .validate_egress_url("http://169.254.169.254/latest/meta-data")
        .unwrap_err();
    match err {
        HostAbiError::SsrfBlocked(ip) => assert_eq!(ip, "169.254.169.254"),
        other => panic!("Expected SsrfBlocked, got: {:?}", other),
    }

    let ok_url = caps
        .validate_egress_url("https://api.github.com/repos")
        .unwrap();
    assert_eq!(ok_url.host_str(), Some("api.github.com"));
}

#[test]
fn test_wasm_fuel_metering_prevents_infinite_loop() {
    let engine = WasmEngine::new().expect("Failed to initialize engine");

    let wat = r#"
        (module
            (func (export "spin")
                (loop
                    (br 0)
                )
            )
        )
    "#;
    let module = engine.compile(wat.as_bytes()).expect("Compilation failed");

    let caps = HostCapabilities::new_external(HashSet::new());
    let (mut store, instance) = engine
        .instantiate(&module, caps, 5_000)
        .expect("Instantiation failed");

    let spin = instance
        .get_typed_func::<(), ()>(&mut store, "spin")
        .expect("Function not found");

    let result = spin.call(&mut store, ());
    assert!(
        result.is_err(),
        "Expected infinite loop to trap from fuel exhaustion"
    );

    let err = result.unwrap_err();
    let is_out_of_fuel = if let Some(trap) = err.downcast_ref::<wasmtime::Trap>() {
        *trap == wasmtime::Trap::OutOfFuel
    } else {
        err.root_cause().to_string().to_lowercase().contains("fuel")
    };
    assert!(
        is_out_of_fuel,
        "Expected fuel exhaustion trap, got: {:?}",
        err
    );
}

#[test]
fn test_host_abi_state_mode_switch() {
    let engine = WasmEngine::new().expect("Engine init failed");

    let wat = r#"
        (module
            (import "nosrc_host" "has_embedded_state" (func $has_embedded (result i32)))
            (func (export "check_state") (result i32)
                (call $has_embedded)
            )
        )
    "#;
    let module = engine.compile(wat.as_bytes()).expect("Compile failed");

    let sqlite = rusqlite::Connection::open_in_memory().unwrap();
    let embedded_caps = HostCapabilities::new_embedded(sqlite, None, HashSet::new());
    let (mut store_embedded, instance_embedded) = engine
        .instantiate(&module, embedded_caps, DEFAULT_FUEL_BUDGET)
        .unwrap();

    let check_state_embedded = instance_embedded
        .get_typed_func::<(), i32>(&mut store_embedded, "check_state")
        .unwrap();
    assert_eq!(
        check_state_embedded.call(&mut store_embedded, ()).unwrap(),
        1
    );

    let external_caps = HostCapabilities::new_external(HashSet::new());
    let (mut store_external, instance_external) = engine
        .instantiate(&module, external_caps, DEFAULT_FUEL_BUDGET)
        .unwrap();

    let check_state_external = instance_external
        .get_typed_func::<(), i32>(&mut store_external, "check_state")
        .unwrap();
    assert_eq!(
        check_state_external.call(&mut store_external, ()).unwrap(),
        0
    );
}

#[tokio::test]
async fn test_host_abi_embedded_kv_operations() {
    let engine = WasmEngine::new().expect("Engine init failed");

    let wat = r#"
        (module
            (import "nosrc_host" "host_kv_set" (func $kv_set (param i32 i32 i32 i32) (result i32)))
            (import "nosrc_host" "host_kv_get" (func $kv_get (param i32 i32) (result i32)))
            (import "nosrc_host" "host_kv_delete" (func $kv_delete (param i32 i32) (result i32)))
            (import "nosrc_host" "response_read" (func $resp_read (param i32 i32) (result i32)))
            (memory (export "memory") 1)

            (data (i32.const 0) "runtime")
            (data (i32.const 10) "nosrc")

            (func (export "test_kv_flow") (result i32)
                (call $kv_set (i32.const 0) (i32.const 7) (i32.const 10) (i32.const 5))
                drop

                (call $kv_get (i32.const 0) (i32.const 7))
                drop

                (call $resp_read (i32.const 30) (i32.const 10))
                drop

                (call $kv_delete (i32.const 0) (i32.const 7))
            )
        )
    "#;
    let module = engine.compile(wat.as_bytes()).expect("Compile failed");

    let tmp_dir = std::env::temp_dir();
    let db_path = tmp_dir.join(format!("nosrc_test_kv_{}.redb", uuid::Uuid::now_v7()));
    let db = redb::Database::create(&db_path).unwrap();
    let kv_arc = Arc::new(db);

    let sqlite = rusqlite::Connection::open_in_memory().unwrap();
    let embedded_caps =
        HostCapabilities::new_embedded(sqlite, Some(kv_arc.clone()), HashSet::new());

    let (mut store, instance) = engine
        .instantiate(&module, embedded_caps, DEFAULT_FUEL_BUDGET)
        .unwrap();

    let test_flow = instance
        .get_typed_func::<(), i32>(&mut store, "test_kv_flow")
        .unwrap();

    let delete_result = test_flow.call(&mut store, ()).unwrap();
    assert_eq!(delete_result, 1, "Expected successful deletion (1)");

    let caps = HostCapabilities::new_embedded(
        rusqlite::Connection::open_in_memory().unwrap(),
        Some(kv_arc),
        HashSet::new(),
    );
    assert_eq!(caps.kv_get("runtime").unwrap(), None);

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_host_abi_embedded_sql_operations() {
    let engine = WasmEngine::new().expect("Engine init failed");

    let wat = r#"
        (module
            (import "nosrc_host" "host_db_execute" (func $db_exec (param i32 i32 i32 i32) (result i32)))
            (import "nosrc_host" "host_db_query" (func $db_query (param i32 i32 i32 i32) (result i32)))
            (import "nosrc_host" "response_read" (func $resp_read (param i32 i32) (result i32)))
            (memory (export "memory") 1)

            (data (i32.const 0) "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
            (data (i32.const 60) "INSERT INTO users (name) VALUES (?)")
            (data (i32.const 105) "[\"alice\"]")
            (data (i32.const 120) "SELECT id, name FROM users")

            (func (export "test_sql_flow") (result i32)
                (call $db_exec (i32.const 0) (i32.const 54) (i32.const 0) (i32.const 0))
                drop

                (call $db_exec (i32.const 60) (i32.const 35) (i32.const 105) (i32.const 9))
                drop

                (call $db_query (i32.const 120) (i32.const 26) (i32.const 0) (i32.const 0))
            )
        )
    "#;
    let module = engine.compile(wat.as_bytes()).expect("Compile failed");

    let sqlite = rusqlite::Connection::open_in_memory().unwrap();
    let embedded_caps = HostCapabilities::new_embedded(sqlite, None, HashSet::new());

    let (mut store, instance) = engine
        .instantiate(&module, embedded_caps, DEFAULT_FUEL_BUDGET)
        .unwrap();

    let test_flow = instance
        .get_typed_func::<(), i32>(&mut store, "test_sql_flow")
        .unwrap();

    let query_len = test_flow.call(&mut store, ()).unwrap();
    assert!(query_len > 0, "Query should return non-empty JSON length");

    let last_resp = store.data().last_response.clone();
    let json_val: serde_json::Value = serde_json::from_slice(&last_resp).unwrap();
    let array = json_val.as_array().unwrap();
    assert_eq!(array.len(), 1);
    assert_eq!(array[0]["name"], "alice");
    assert_eq!(array[0]["id"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_host_abi_external_mode_disables_embedded_state() {
    let engine = WasmEngine::new().expect("Engine init failed");

    let wat = r#"
        (module
            (import "nosrc_host" "host_kv_set" (func $kv_set (param i32 i32 i32 i32) (result i32)))
            (import "nosrc_host" "host_db_execute" (func $db_exec (param i32 i32 i32 i32) (result i32)))
            (memory (export "memory") 1)

            (data (i32.const 0) "foo")
            (data (i32.const 10) "bar")

            (func (export "test_disabled_kv") (result i32)
                (call $kv_set (i32.const 0) (i32.const 3) (i32.const 10) (i32.const 3))
            )

            (func (export "test_disabled_db") (result i32)
                (call $db_exec (i32.const 0) (i32.const 3) (i32.const 0) (i32.const 0))
            )
        )
    "#;
    let module = engine.compile(wat.as_bytes()).expect("Compile failed");

    let external_caps = HostCapabilities::new_external(HashSet::new());
    let (mut store, instance) = engine
        .instantiate(&module, external_caps, DEFAULT_FUEL_BUDGET)
        .unwrap();

    let test_kv = instance
        .get_typed_func::<(), i32>(&mut store, "test_disabled_kv")
        .unwrap();
    assert_eq!(
        test_kv.call(&mut store, ()).unwrap(),
        ERR_CAPABILITY_DISABLED
    );

    let test_db = instance
        .get_typed_func::<(), i32>(&mut store, "test_disabled_db")
        .unwrap();
    assert_eq!(
        test_db.call(&mut store, ()).unwrap(),
        ERR_CAPABILITY_DISABLED
    );
}

#[tokio::test]
async fn test_host_abi_rejects_multi_statement_sql() {
    let sqlite = rusqlite::Connection::open_in_memory().unwrap();
    let embedded_caps = HostCapabilities::new_embedded(sqlite, None, HashSet::new());

    let injection_attempt = "SELECT 1; DROP TABLE users;";
    let err = embedded_caps
        .db_query(injection_attempt, &serde_json::Value::Null)
        .await
        .unwrap_err();

    match err {
        HostAbiError::DatabaseError(msg) => {
            assert!(
                msg.contains("Multi-statement SQL queries are forbidden"),
                "Expected multi-statement rejection, got: {}",
                msg
            );
        }
        other => panic!("Expected DatabaseError, got: {:?}", other),
    }
}

#[test]
fn test_invoke_handler_e2e_envelope() {
    let engine = WasmEngine::new().expect("Failed to create engine");

    // Guest WAT module exporting `handle_request`:
    // Returns `{"status":201,"headers":{"x-nosrc":"active"},"body":"created"}` (len 64)
    // Written at offset 1024
    let resp_str = r#"{"status":201,"headers":{"x-nosrc":"active"},"body":"created"}"#;
    let resp_escaped = resp_str.replace('"', "\\\"");

    // In this test, guest handle_request receives req_ptr (1024), copies its response at offset 2048,
    // and calls host_set_response(resp_ptr, resp_len) to cleanly transmit across Host ABI
    let wat = format!(
        r#"
        (module
            (import "nosrc_host" "host_set_response" (func $set_resp (param i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 2048) "{}")

            (func (export "handle_request") (param i32 i32) (result i32)
                (call $set_resp (i32.const 2048) (i32.const {}))
            )
        )
    "#,
        resp_escaped,
        resp_str.len()
    );

    let module = engine.compile(wat.as_bytes()).expect("WAT compile failed");

    let mut req = RequestEnvelope {
        method: "POST".to_string(),
        path: "/users".to_string(),
        query: HashMap::new(),
        headers: HashMap::new(),
        body: Some(r#"{"name":"bob"}"#.to_string()),
    };
    req.headers
        .insert("content-type".to_string(), "application/json".to_string());

    let caps = HostCapabilities::new_external(HashSet::new());
    let resp = engine
        .invoke_handler(&module, caps, &req, DEFAULT_FUEL_BUDGET)
        .expect("Invoke handler failed");

    assert_eq!(resp.status, 201);
    assert_eq!(resp.headers.get("x-nosrc"), Some(&"active".to_string()));
    assert_eq!(resp.body, Some("created".to_string()));

    // Verify converting into Axum HTTP response
    let axum_resp = resp.into_axum_response();
    assert_eq!(axum_resp.status(), axum::http::StatusCode::CREATED);
}

#[test]
fn test_error_envelope_mapping() {
    let fuel_err = crate::error::EngineError::Trap(crate::error::SandboxTrap::FuelExhausted);
    let env = ErrorEnvelope::from_engine_error(&fuel_err);

    assert_eq!(env.status, 504);
    assert_eq!(env.error_type, "FuelExhausted");

    let axum_resp = env.into_axum_response();
    assert_eq!(axum_resp.status(), axum::http::StatusCode::GATEWAY_TIMEOUT);
}
