//! Deterministic evaluation corpus for the SPEC synthesis and verification pipeline.
//!
//! Reference handlers are first verified offline. A local mock provider then returns
//! those same handlers through the real synthesis/componentization/verification path.
//! The ignored live test uses the configured provider and writes the same redacted report.

use super::*;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const CORPUS: &[(&str, &str, &str)] = &[
    (
        "normalize-json",
        include_str!("../../../evaluation/corpus/normalize-json/SPEC.md"),
        include_str!("../../../evaluation/corpus/normalize-json/handler.js"),
    ),
    (
        "required-field",
        include_str!("../../../evaluation/corpus/required-field/SPEC.md"),
        include_str!("../../../evaluation/corpus/required-field/handler.js"),
    ),
    (
        "status-headers",
        include_str!("../../../evaluation/corpus/status-headers/SPEC.md"),
        include_str!("../../../evaluation/corpus/status-headers/handler.js"),
    ),
    (
        "malformed-json",
        include_str!("../../../evaluation/corpus/malformed-json/SPEC.md"),
        include_str!("../../../evaluation/corpus/malformed-json/handler.js"),
    ),
    (
        "query-parameters",
        include_str!("../../../evaluation/corpus/query-parameters/SPEC.md"),
        include_str!("../../../evaluation/corpus/query-parameters/handler.js"),
    ),
    (
        "created-resource",
        include_str!("../../../evaluation/corpus/created-resource/SPEC.md"),
        include_str!("../../../evaluation/corpus/created-resource/handler.js"),
    ),
    (
        "plain-text",
        include_str!("../../../evaluation/corpus/plain-text/SPEC.md"),
        include_str!("../../../evaluation/corpus/plain-text/handler.js"),
    ),
    (
        "empty-response",
        include_str!("../../../evaluation/corpus/empty-response/SPEC.md"),
        include_str!("../../../evaluation/corpus/empty-response/handler.js"),
    ),
    (
        "filter-list",
        include_str!("../../../evaluation/corpus/filter-list/SPEC.md"),
        include_str!("../../../evaluation/corpus/filter-list/handler.js"),
    ),
    (
        "path-resource",
        include_str!("../../../evaluation/corpus/path-resource/SPEC.md"),
        include_str!("../../../evaluation/corpus/path-resource/handler.js"),
    ),
];

#[derive(Clone)]
struct MockCandidate {
    handler: String,
    assertions: Vec<ContractAssertion>,
}

fn assertions_from_spec(spec: &ServiceSpec) -> Vec<ContractAssertion> {
    spec.verification
        .iter()
        .enumerate()
        .map(|(index, case)| ContractAssertion {
            name: format!("mock corpus case {}", index + 1),
            request: RequestEnvelope {
                method: case.method.clone(),
                scheme: None,
                authority: None,
                path: case.path.clone(),
                query: (!case.query.is_empty()).then(|| {
                    let mut serializer = form_urlencoded::Serializer::new(String::new());
                    for (k, v) in &case.query {
                        serializer.append_pair(k, v);
                    }
                    serializer.finish()
                }),
                headers: case
                    .headers
                    .iter()
                    .map(|(name, value)| crate::engine::http_envelope::Header {
                        name: name.clone(),
                        value: value.as_bytes().to_vec(),
                    })
                    .collect(),
                body: case.body.as_deref().unwrap_or("").as_bytes().to_vec(),
            },
            expected_status: case.expected_status.unwrap_or(200),
            body_contains: case.body_contains.clone(),
            header_matches: (!case.response_headers.is_empty())
                .then(|| case.response_headers.clone()),
        })
        .collect()
}

fn corpus_candidates() -> HashMap<String, MockCandidate> {
    CORPUS
        .iter()
        .map(|(slug, spec_text, handler)| {
            let spec = ServiceSpec::parse(spec_text)
                .unwrap_or_else(|error| panic!("invalid corpus SPEC {slug}: {error}"));
            assert_eq!(spec.name, format!("corpus-{slug}"));
            let assertions = assertions_from_spec(&spec);
            (
                spec.name,
                MockCandidate {
                    handler: (*handler).to_string(),
                    assertions,
                },
            )
        })
        .collect()
}

async fn mock_completion(
    State(candidates): State<Arc<HashMap<String, MockCandidate>>>,
    Json(request): Json<Value>,
) -> Json<Value> {
    let prompt = request["messages"][1]["content"]
        .as_str()
        .unwrap_or_default();
    // This guards the policy boundary: operator-owned verification cases must not
    // be copied into the model-visible SPEC prompt.
    assert!(!prompt.contains("## Verification"));
    assert!(!prompt.contains("Body contains:"));
    let candidate = candidates
        .iter()
        .find_map(|(name, candidate)| prompt.contains(name).then_some(candidate))
        .expect("mock provider received an unknown SPEC");
    let output = json!({
        "handler_code": candidate.handler,
        "code_format": "javascript",
        "assertions": candidate.assertions,
        "rationale": "Reference implementation returned by the deterministic evaluation provider"
    });
    Json(json!({
        "choices": [{
            "message": {
                "content": serde_json::to_string(&output).expect("serialize mock completion")
            }
        }]
    }))
}

#[derive(Clone)]
struct RepairMockState {
    candidates: Arc<HashMap<String, MockCandidate>>,
    requests: Arc<AtomicUsize>,
}

async fn mock_repair_completion(
    State(state): State<RepairMockState>,
    Json(request): Json<Value>,
) -> Json<Value> {
    let prompt = request["messages"][1]["content"]
        .as_str()
        .unwrap_or_default();
    assert!(!prompt.contains("## Verification"));
    assert!(!prompt.contains("Body contains:"));
    let candidate = state
        .candidates
        .iter()
        .find_map(|(name, candidate)| prompt.contains(name).then_some(candidate))
        .expect("repair mock received an unknown SPEC");
    let attempt = state.requests.fetch_add(1, Ordering::SeqCst);
    let handler = if attempt == 0 {
        "export function handle(_request) { return { status: 500, headers: [], body: { tag: \"text\", val: \"deliberate first-pass failure\" } }; }"
    } else {
        assert!(
            prompt.contains("Failure category: verification_failed"),
            "replacement request omitted the host-authored failure category"
        );
        candidate.handler.as_str()
    };
    let output = json!({
        "handler_code": handler,
        "code_format": "javascript",
        "assertions": candidate.assertions,
        "rationale": "Deterministic repair-path fixture"
    });
    Json(
        json!({"choices":[{"message":{"content":serde_json::to_string(&output).expect("serialize repair completion")}}]}),
    )
}

async fn start_repair_mock_provider() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let state = RepairMockState {
        candidates: Arc::new(corpus_candidates()),
        requests: requests.clone(),
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_repair_completion))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind repair mock provider");
    let address = listener.local_addr().expect("repair mock provider address");
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}/v1"), requests, task)
}

async fn start_mock_provider() -> (String, tokio::task::JoinHandle<()>) {
    let candidates = Arc::new(corpus_candidates());
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_completion))
        .with_state(candidates);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock evaluation provider");
    let address = listener.local_addr().expect("mock provider address");
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}/v1"), task)
}

/// Stable report vocabulary for cross-run comparisons.
fn canonical_failure_category(raw: &str) -> &'static str {
    match raw {
        "cached_artifact_invalid" => "cache_artifact_invalid",
        "cached_componentization_failed" => "cache_component_invalid",
        "invalid_candidate_output" => "candidate_invalid",
        "componentization_failed" => "componentization_failed",
        "verification_failed" => "verification_failed",
        _ => "other",
    }
}

fn canonical_failure_categories(raw: &[String]) -> Vec<&'static str> {
    let mut categories = Vec::new();
    for item in raw {
        let category = canonical_failure_category(item);
        if !categories.contains(&category) {
            categories.push(category);
        }
    }
    categories
}

#[cfg(test)]
mod failure_category_tests {
    use super::*;

    #[test]
    fn report_categories_use_a_stable_deduplicated_vocabulary() {
        let raw = vec![
            "cached_artifact_invalid".into(),
            "verification_failed".into(),
            "verification_failed".into(),
            "unrecognized_internal_value".into(),
        ];
        assert_eq!(
            canonical_failure_categories(&raw),
            vec!["cache_artifact_invalid", "verification_failed", "other"]
        );
    }
}

fn report_entry(
    slug: &str,
    generation: &PreparedGeneration,
    started: Instant,
    replacement_attempts: usize,
    failure_categories: Vec<String>,
) -> Value {
    let telemetry = &generation.verification_telemetry;
    let trusted_passed = generation
        .trusted_results
        .iter()
        .all(|result| result.passed);
    let model_passed = generation.model_results.iter().all(|result| result.passed);
    json!({
        "spec": slug,
        "first_pass_verified": replacement_attempts == 0 && trusted_passed && model_passed,
        "repair_attempts": replacement_attempts,
        "failed_check_categories": canonical_failure_categories(&failure_categories),
        "trusted_checks": {
            "passed": generation.trusted_results.iter().filter(|result| result.passed).count(),
            "total": generation.trusted_results.len()
        },
        "model_checks": {
            "passed": generation.model_results.iter().filter(|result| result.passed).count(),
            "total": generation.model_results.len()
        },
        "fuel_high_water": telemetry.generation_fuel_high_water,
        "memory_high_water_bytes": telemetry.generation_memory_high_water_bytes,
        "duration_ms": started.elapsed().as_millis()
    })
}

fn write_report(mode: &str, entries: Vec<Value>) {
    let report = json!({
        "schema_version": 1,
        "mode": mode,
        "generated_at_unix_ms": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        "scope": "These results describe only the checked-in SPEC evaluation corpus.",
        "specs": entries
    });
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/spec-evaluation");
    fs::create_dir_all(&directory).expect("create evaluation report directory");
    let path = directory.join(format!("{mode}-report.json"));
    fs::write(
        &path,
        serde_json::to_vec_pretty(&report).expect("serialize evaluation report"),
    )
    .expect("write evaluation report");
    println!(
        "SPEC evaluation report ({mode}):\n{}",
        serde_json::to_string_pretty(&report).expect("format evaluation report")
    );
    println!("SPEC evaluation JSON written to {}", path.display());
}

#[tokio::test]
#[ignore = "run in the isolated SPEC evaluation CI job"]
async fn spec_evaluation_reference_corpus_verifies_offline_without_provider_or_network() {
    assert_eq!(
        CORPUS.len(),
        10,
        "the initial evaluation corpus must contain 10 SPECs"
    );
    let mut entries = Vec::with_capacity(CORPUS.len());

    for (slug, spec_text, handler_source) in CORPUS {
        let spec = ServiceSpec::parse(spec_text)
            .unwrap_or_else(|error| panic!("invalid corpus SPEC {slug}: {error}"));
        spec.validate_verification()
            .unwrap_or_else(|error| panic!("invalid verification policy in {slug}: {error}"));
        let lifecycle = AppLifecycle::new_with_spec_text(&spec, (*spec_text).to_string(), None)
            .unwrap_or_else(|error| panic!("cannot initialize {slug}: {error}"));
        let started = Instant::now();
        let generation = lifecycle
            .preparation_context()
            .prepare_from_js((*handler_source).to_string(), Vec::new())
            .await
            .unwrap_or_else(|error| panic!("reference handler failed for {slug}: {error:?}"));
        assert!(
            generation
                .trusted_results
                .iter()
                .all(|result| result.passed),
            "trusted verification failed for {slug}: {:?}",
            generation.trusted_results
        );
        assert!(
            generation
                .verification_telemetry
                .generation_fuel_high_water
                .is_some(),
            "verification telemetry omitted all disposable verification invocations for {slug}"
        );
        entries.push(report_entry(slug, &generation, started, 0, Vec::new()));
    }

    write_report("offline", entries);
}

#[tokio::test]
#[ignore = "run in the isolated SPEC evaluation CI job"]
async fn spec_evaluation_host_probe_telemetry_is_retained_without_other_verification_suites() {
    // MCP-first mode has no operator SPEC or trusted assertions. Direct JS also
    // skips model assertions, leaving the host-generated probes as the only
    // disposable verification instance contributing telemetry.
    let lifecycle = AppLifecycle::new_mcp_first(None).expect("initialize MCP-first lifecycle");
    let generation = lifecycle
        .preparation_context()
        .prepare_from_js(
            "export function handle(_request) { return { status: 200, headers: [], body: { tag: \"text\", val: \"ok\" } }; }".to_string(),
            Vec::new(),
        )
        .await
        .expect("host probes should accept a valid response envelope");

    assert!(generation.trusted_results.is_empty());
    assert!(generation.model_results.is_empty());
    assert!(
        generation
            .verification_telemetry
            .generation_fuel_high_water
            .is_some(),
        "host-probe invocation telemetry must be retained even when no trusted or model suite runs"
    );
}
#[tokio::test]
#[ignore = "run in the isolated SPEC evaluation CI job"]
async fn spec_evaluation_mock_provider_runs_the_real_synthesis_componentization_and_verification_pipeline(
) {
    let (base_url, provider_task) = start_mock_provider().await;
    let synthesizer = Synthesizer::new(LlmConfig {
        api_key: "mock-evaluation-key".into(),
        base_url,
        model: "nosrc-evaluation-mock".into(),
        temperature: 0.0,
    });
    let mut entries = Vec::with_capacity(CORPUS.len());

    for (slug, spec_text, _) in CORPUS {
        let spec = ServiceSpec::parse(spec_text)
            .unwrap_or_else(|error| panic!("invalid corpus SPEC {slug}: {error}"));
        let lifecycle = AppLifecycle::new_with_spec_text(&spec, (*spec_text).to_string(), None)
            .unwrap_or_else(|error| panic!("cannot initialize {slug}: {error}"));
        let started = Instant::now();
        let generation = lifecycle
            .preparation_context()
            .prepare_from_spec_with_synthesizer(Vec::new(), &synthesizer)
            .await
            .unwrap_or_else(|error| panic!("mock synthesis pipeline failed for {slug}: {error:?}"));
        assert!(
            generation
                .trusted_results
                .iter()
                .all(|result| result.passed),
            "trusted verification failed for {slug}: {:?}",
            generation.trusted_results
        );
        assert!(
            !generation.model_results.is_empty(),
            "mock provider returned no model-visible assertions for {slug}"
        );
        assert_eq!(
            generation.model_results.len(),
            spec.verification.len(),
            "mock provider model assertion count did not cover every verification case for {slug}"
        );
        assert!(
            generation.model_results.iter().all(|result| result.passed),
            "model assertion verification failed for {slug}: {:?}",
            generation.model_results
        );
        let repair = generation
            .export_data
            .as_ref()
            .expect("SPEC pipeline should retain export provenance")
            .repair_summary
            .clone();
        entries.push(report_entry(
            slug,
            &generation,
            started,
            repair.replacement_attempts,
            repair.failure_categories,
        ));
    }

    provider_task.abort();
    write_report("mock", entries);
}

#[tokio::test]
#[ignore = "run in the isolated SPEC evaluation CI job"]
async fn spec_evaluation_mock_provider_exercises_verification_repair_and_replacement() {
    let (base_url, requests, provider_task) = start_repair_mock_provider().await;
    let synthesizer = Synthesizer::new(LlmConfig {
        api_key: "mock-repair-key".into(),
        base_url,
        model: "nosrc-evaluation-repair-mock".into(),
        temperature: 0.0,
    });
    let (slug, spec_text, _) = CORPUS
        .iter()
        .find(|(slug, _, _)| *slug == "normalize-json")
        .expect("repair fixture must exist");
    let spec = ServiceSpec::parse(spec_text)
        .unwrap_or_else(|error| panic!("invalid repair SPEC {slug}: {error}"));
    let lifecycle = AppLifecycle::new_with_spec_text(&spec, (*spec_text).to_string(), None)
        .unwrap_or_else(|error| panic!("cannot initialize repair fixture: {error}"));
    let generation = lifecycle
        .preparation_context()
        .prepare_from_spec_with_synthesizer(Vec::new(), &synthesizer)
        .await
        .unwrap_or_else(|error| panic!("repair synthesis pipeline failed: {error:?}"));

    provider_task.abort();
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "expected initial and replacement responses"
    );
    assert!(generation
        .trusted_results
        .iter()
        .all(|result| result.passed));
    assert!(
        !generation.model_results.is_empty(),
        "repair fixture must exercise model assertions"
    );
    assert!(generation.model_results.iter().all(|result| result.passed));
    let repair = generation
        .export_data
        .as_ref()
        .expect("SPEC pipeline should retain export provenance")
        .repair_summary
        .clone();
    assert_eq!(repair.replacement_attempts, 1);
    assert!(repair
        .failure_categories
        .iter()
        .any(|category| category == "verification_failed"));
}

#[tokio::test]
#[ignore = "requires LLM_API_KEY and optional LLM_BASE_URL/LLM_MODEL; writes a live evaluation report"]
async fn live_provider_evaluation_writes_a_redacted_json_report() {
    let synthesizer =
        Synthesizer::new(LlmConfig::from_env().expect("valid live LLM environment configuration"));
    let mut entries = Vec::with_capacity(CORPUS.len());

    for (slug, spec_text, _) in CORPUS {
        let spec = ServiceSpec::parse(spec_text)
            .unwrap_or_else(|error| panic!("invalid corpus SPEC {slug}: {error}"));
        let lifecycle = AppLifecycle::new_with_spec_text(&spec, (*spec_text).to_string(), None)
            .unwrap_or_else(|error| panic!("cannot initialize {slug}: {error}"));
        let started = Instant::now();
        let generation = lifecycle
            .preparation_context()
            .prepare_from_spec_with_synthesizer(Vec::new(), &synthesizer)
            .await
            .unwrap_or_else(|error| panic!("live synthesis pipeline failed for {slug}: {error:?}"));
        let repair = generation
            .export_data
            .as_ref()
            .expect("SPEC pipeline should retain export provenance")
            .repair_summary
            .clone();
        entries.push(report_entry(
            slug,
            &generation,
            started,
            repair.replacement_attempts,
            repair.failure_categories,
        ));
    }

    write_report("live", entries);
}
