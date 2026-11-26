use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use uuid::Uuid;

use crate::artifact_cache::ArtifactCache;
use crate::engine::http_envelope::{RequestEnvelope, ResponseEnvelope};
use crate::engine::quickjs::{CompiledHandler, QuickJsEngine, QuickJsHandler, RuntimeTelemetry};
use crate::error::EngineError;
use crate::export::RepairSummary;
use crate::ingress::{ActiveGeneration, IngressState};
use crate::spec::ServiceSpec;
use crate::synthesizer::{ContractAssertion, LlmConfig, SynthesisOutput, Synthesizer};
use crate::verifier::{AssertionResult, HostProbeVerifier, ShadowVerifier};

use super::policy::AcceptancePolicy;

const MAX_JS_SOURCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_ASSERTIONS: usize = 100;
const MAX_ASSERTION_TOTAL_BYTES: usize = 2 * 1024 * 1024;
const MAX_ASSERTION_NAME_BYTES: usize = 256;
const MAX_ASSERTION_PATH_BYTES: usize = 8192;
const MAX_ASSERTION_BODY_BYTES: usize = 1024 * 1024;
const MAX_ASSERTION_HEADER_COUNT: usize = 100;
const MAX_ASSERTION_HEADER_BYTES: usize = 64 * 1024;
const MAX_ASSERTION_QUERY_COUNT: usize = 100;
const MAX_ASSERTION_JSON_PATH_COUNT: usize = 100;
const MAX_ASSERTION_JSON_PATH_BYTES: usize = 4096;
const MAX_REPLACEMENT_ATTEMPTS: usize = 2;

/// Opaque identifier for one prepared application generation.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct GenerationId(String);

impl GenerationId {
    pub fn from_string(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationState {
    Ready,
    Active,
    Superseded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    Spec,
    DraftSpec,
    DirectJs,
}

fn has_unambiguous_export_provenance(
    draft_spec: bool,
    trusted_assertions: &[ContractAssertion],
) -> bool {
    !(draft_spec && !trusted_assertions.is_empty())
}

#[derive(Clone)]
pub struct GenerationExport {
    pub spec: ServiceSpec,
    pub spec_text: String,
    pub module_hash: String,
    pub output: SynthesisOutput,
    pub component_bytes: Vec<u8>,
    pub trusted_assertions: Vec<ContractAssertion>,
    pub policy_mode: String,
    pub repair_summary: RepairSummary,
}

/// A SPEC generation prepared for export. The separate type guarantees that
/// export callers always have the provenance and artifacts required by the bundle writer.
pub struct ExportablePreparedGeneration {
    pub generation: PreparedGeneration,
    pub export: GenerationExport,
}

pub struct PreparedGeneration {
    pub id: GenerationId,
    pub sequence: u64,
    pub state: GenerationState,
    pub module_hash: String,
    pub js_source: String,
    pub source_type: SourceType,
    pub trusted_results: Vec<AssertionResult>,
    pub model_results: Vec<AssertionResult>,
    /// High-water resource measurements across disposable trusted, host-probe, model, and supplemental verification instances.
    pub verification_telemetry: RuntimeTelemetry,
    pub supplemental_results: Vec<AssertionResult>,
    pub created_at: Instant,
    pub handler: Arc<Mutex<QuickJsHandler>>,
    pub export_data: Option<GenerationExport>,
}

#[derive(Debug, Serialize)]
pub struct GenerationSummary {
    pub generation_id: String,
    pub state: GenerationState,
    pub module_hash: String,
    pub source_type: SourceType,
    pub trusted_assertions_passed: usize,
    pub model_assertions_passed: usize,
    pub model_assertions_failed: usize,
    pub supplemental_assertions_passed: usize,
    pub supplemental_assertions_failed: usize,
    pub age_seconds: u64,
    pub trusted_assertions: Vec<AssertionSummary>,
    pub model_assertions: Vec<AssertionSummary>,
    pub supplemental_assertions: Vec<AssertionSummary>,
    pub telemetry: Option<RuntimeTelemetry>,
}

#[derive(Debug, Serialize)]
pub struct AssertionSummary {
    pub name: String,
    pub passed: bool,
    pub expected_status: u16,
    pub expected_status_max: u16,
    pub actual_status: Option<u16>,
}

#[derive(Debug, Serialize)]
pub struct RuntimeLimits {
    pub fuel_per_invocation: u64,
    pub memory_limit_bytes: usize,
    pub response_limit_bytes: usize,
}

#[derive(Debug, Serialize)]
pub struct AppStatus {
    pub status: &'static str,
    pub active_generation: Option<String>,
    pub previous_generation: Option<String>,
    pub generations: Vec<GenerationSummary>,
    pub runtime_limits: RuntimeLimits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationSuite {
    Operator,
    Generated,
}

#[derive(Debug)]
pub enum PreparationError {
    Runtime(EngineError),
    Verification {
        suite: VerificationSuite,
        results: Vec<AssertionResult>,
    },
}

impl From<EngineError> for PreparationError {
    fn from(error: EngineError) -> Self {
        Self::Runtime(error)
    }
}

fn validate_assertions(assertions: &[ContractAssertion], label: &str) -> Result<(), EngineError> {
    if assertions.len() > MAX_ASSERTIONS {
        return Err(EngineError::Internal(format!(
            "At most {MAX_ASSERTIONS} {label} assertions are accepted"
        )));
    }
    let total_bytes = assertions.iter().fold(0usize, |total, assertion| {
        let request_header_bytes =
            assertion
                .request
                .headers
                .iter()
                .fold(0usize, |bytes, header| {
                    bytes
                        .saturating_add(header.name.len())
                        .saturating_add(header.value.len())
                });
        let query_bytes = assertion.request.query.as_ref().map_or(0, String::len);
        let response_header_bytes = assertion
            .header_matches
            .as_ref()
            .map(|headers| {
                headers.iter().fold(0usize, |bytes, (name, value)| {
                    bytes.saturating_add(name.len()).saturating_add(value.len())
                })
            })
            .unwrap_or_default();
        let response_schema_bytes = assertion
            .response_json_schema
            .as_ref()
            .and_then(|schema| serde_json::to_vec(schema).ok())
            .map_or(0, |schema| schema.len());
        let json_path_bytes =
            assertion
                .json_path_assertions
                .iter()
                .fold(0usize, |bytes, path_assertion| {
                    bytes
                        .saturating_add(path_assertion.path.len())
                        .saturating_add(
                            serde_json::to_vec(&path_assertion.expected)
                                .map_or(0, |value| value.len()),
                        )
                });
        total
            .saturating_add(assertion.name.len())
            .saturating_add(assertion.request.method.len())
            .saturating_add(assertion.request.path.len())
            .saturating_add(assertion.request.body.len())
            .saturating_add(assertion.body_contains.as_ref().map_or(0, String::len))
            .saturating_add(request_header_bytes)
            .saturating_add(query_bytes)
            .saturating_add(response_header_bytes)
            .saturating_add(response_schema_bytes)
            .saturating_add(json_path_bytes)
    });
    if total_bytes > MAX_ASSERTION_TOTAL_BYTES {
        return Err(EngineError::Internal(format!(
            "{label} assertions exceed the 2 MiB aggregate input limit"
        )));
    }
    for assertion in assertions {
        let header_bytes = assertion
            .request
            .headers
            .iter()
            .fold(0usize, |total, header| {
                total
                    .saturating_add(header.name.len())
                    .saturating_add(header.value.len())
            });
        if assertion.name.len() > MAX_ASSERTION_NAME_BYTES
            || assertion.request.path.len() > MAX_ASSERTION_PATH_BYTES
            || assertion.request.body.len() > MAX_ASSERTION_BODY_BYTES
            || assertion
                .body_contains
                .as_ref()
                .is_some_and(|body| body.len() > MAX_ASSERTION_BODY_BYTES)
            || assertion
                .response_json_schema
                .as_ref()
                .and_then(|schema| serde_json::to_vec(schema).ok())
                .is_some_and(|schema| schema.len() > 64 * 1024)
            || assertion.request.headers.len() > MAX_ASSERTION_HEADER_COUNT
            || assertion.json_path_assertions.len() > MAX_ASSERTION_JSON_PATH_COUNT
            || assertion.json_path_assertions.iter().any(|path_assertion| {
                path_assertion.path.len() > MAX_ASSERTION_JSON_PATH_BYTES
                    || crate::spec::json_path_value(&serde_json::Value::Null, &path_assertion.path)
                        .is_err()
            })
            || !(100..=599).contains(&assertion.expected_status)
            || assertion
                .expected_status_max
                .is_some_and(|maximum| maximum < assertion.expected_status || maximum > 599)
            || header_bytes > MAX_ASSERTION_HEADER_BYTES
            || assertion
                .request
                .query
                .as_ref()
                .is_some_and(|q| q.len() > MAX_ASSERTION_QUERY_COUNT * 64)
        {
            return Err(EngineError::Internal(format!(
                "{label} assertion exceeds its input limits"
            )));
        }
        if let Some(schema) = &assertion.response_json_schema {
            if !schema.is_object()
                || jsonschema::draft202012::options()
                    .offline()
                    .with_pattern_options(
                        jsonschema::PatternOptions::fancy_regex()
                            .backtrack_limit(10_000)
                            .size_limit(1024 * 1024)
                            .dfa_size_limit(2 * 1024 * 1024),
                    )
                    .build(schema)
                    .is_err()
            {
                return Err(EngineError::Internal(format!(
                    "{label} assertion contains an invalid Draft 2020-12 response schema"
                )));
            }
        }
    }
    Ok(())
}

fn compile_handler_with_artifact_on_blocking_thread(
    engine: QuickJsEngine,
    source: String,
) -> Result<(CompiledHandler, Vec<u8>), EngineError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            EngineError::Internal(format!(
                "Failed to create componentization runtime: {error}"
            ))
        })?;
    runtime.block_on(engine.compile_handler_with_artifact(&source))
}

fn unique_failed_check_count(results: &[AssertionResult]) -> usize {
    let mut failed_checks = HashSet::new();
    for result in results.iter().filter(|result| !result.passed) {
        failed_checks.insert((result.name.as_str(), result.case_index));
    }
    failed_checks.len()
}

fn build_repair_feedback(error: &PreparationError) -> Option<String> {
    match error {
        PreparationError::Verification { suite, results } => {
            let suite = match suite {
                VerificationSuite::Operator => "operator-owned verification",
                VerificationSuite::Generated => "candidate-generated verification",
            };
            Some(format!(
                "Failure category: verification_failed\nSuite: {suite}\nFailed checks: {}\nDetails and assertion definitions are intentionally withheld. Correct the implementation against the visible SPEC only.",
                unique_failed_check_count(results)
            ))
        }
        PreparationError::Runtime(_) => None,
    }
}

#[cfg(test)]
mod repair_feedback_tests {
    use super::*;

    #[test]
    fn repair_feedback_counts_unique_failed_assertion_cases() {
        let results = vec![
            AssertionResult {
                name: "same assertion".into(),
                case_index: 0,
                passed: false,
                actual_status: Some(500),
                expected_status: 200,
                expected_status_max: 200,
                actual_headers: None,
                actual_body: None,
                failure_kind: None,
                error_message: None,
            },
            AssertionResult {
                name: "same assertion".into(),
                case_index: 0,
                passed: false,
                actual_status: Some(502),
                expected_status: 200,
                expected_status_max: 200,
                actual_headers: None,
                actual_body: None,
                failure_kind: None,
                error_message: None,
            },
            AssertionResult {
                name: "same assertion".into(),
                case_index: 1,
                passed: false,
                actual_status: Some(500),
                expected_status: 200,
                expected_status_max: 200,
                actual_headers: None,
                actual_body: None,
                failure_kind: None,
                error_message: None,
            },
            AssertionResult {
                name: "passed assertion".into(),
                case_index: 0,
                passed: true,
                actual_status: Some(200),
                expected_status: 200,
                expected_status_max: 200,
                actual_headers: None,
                actual_body: None,
                failure_kind: None,
                error_message: None,
            },
        ];
        assert_eq!(unique_failed_check_count(&results), 2);
        let feedback = build_repair_feedback(&PreparationError::Verification {
            suite: VerificationSuite::Generated,
            results,
        })
        .unwrap();
        assert!(feedback.contains("Failed checks: 2"));
        assert!(!feedback.contains("same assertion"));
    }
}

fn validate_synthesis_output(output: &SynthesisOutput) -> Result<(), EngineError> {
    if output.code_format != "javascript" || output.handler_code.trim().is_empty() {
        return Err(EngineError::Internal(
            "Synthesizer returned an empty or unsupported handler artifact".into(),
        ));
    }
    if output.handler_code.len() > MAX_JS_SOURCE_BYTES {
        return Err(EngineError::Internal(
            "Synthesized JavaScript exceeds the 2 MiB source limit".into(),
        ));
    }
    if output.assertions.is_empty() {
        return Err(EngineError::Internal(
            "Synthesizer returned an empty model-generated assertion suite".into(),
        ));
    }
    validate_assertions(&output.assertions, "model-generated")
}

/// Immutable preparation inputs. Cloned while holding the lifecycle lock briefly,
/// then used without that lock during synthesis, componentization, and verification.
#[derive(Clone)]
pub struct PreparationContext {
    engine: QuickJsEngine,
    spec: Option<ServiceSpec>,
    spec_text: Option<String>,
    draft_spec: bool,
    draft_assertions: Vec<ContractAssertion>,
    cache: Option<ArtifactCache>,
    policy: AcceptancePolicy,
}

/// Prepare a production SPEC through the same synthesis, cache, compilation,
/// validation, and verification pipeline used by MCP generation requests.
pub(crate) async fn prepare_spec_generation(
    spec: ServiceSpec,
    spec_text: String,
    cache: Option<ArtifactCache>,
    synthesizer: &Synthesizer,
) -> Result<PreparedGeneration, PreparationError> {
    let policy = AcceptancePolicy::from_spec(&spec).map_err(EngineError::Internal)?;
    policy.validate().map_err(EngineError::Internal)?;
    let context = PreparationContext {
        engine: QuickJsEngine::new()?,
        spec: Some(spec),
        spec_text: Some(spec_text),
        draft_spec: false,
        draft_assertions: Vec::new(),
        cache,
        policy,
    };
    context
        .prepare_from_spec_with_synthesizer(Vec::new(), synthesizer)
        .await
}

/// Prepare a production SPEC and require its export payload before returning.
/// HTTP startup uses prepare_spec_generation; the CLI export command uses this
/// stronger result type so it cannot accidentally handle an absent export payload.
pub(crate) async fn prepare_spec_export(
    spec: ServiceSpec,
    spec_text: String,
    cache: Option<ArtifactCache>,
    synthesizer: &Synthesizer,
) -> Result<ExportablePreparedGeneration, PreparationError> {
    let mut generation = prepare_spec_generation(spec, spec_text, cache, synthesizer).await?;
    let export = generation.export_data.take().ok_or_else(|| {
        EngineError::Internal("SPEC preparation did not produce export provenance".into())
    })?;
    Ok(ExportablePreparedGeneration { generation, export })
}

impl PreparationContext {
    pub async fn prepare_from_js(
        self,
        js_source: String,
        supplemental_assertions: Vec<ContractAssertion>,
    ) -> Result<PreparedGeneration, PreparationError> {
        if js_source.trim().is_empty() {
            return Err(PreparationError::Runtime(EngineError::Internal(
                "JavaScript source must not be empty".into(),
            )));
        }
        if js_source.len() > MAX_JS_SOURCE_BYTES {
            return Err(PreparationError::Runtime(EngineError::Internal(
                "JavaScript source exceeds the 2 MiB limit".into(),
            )));
        }
        validate_assertions(&supplemental_assertions, "supplemental")?;
        self.policy.validate().map_err(EngineError::Internal)?;

        let engine = self.engine.clone();
        let source_for_compile = js_source.clone();
        let (compiled, component_bytes) = tokio::task::spawn_blocking(move || {
            compile_handler_with_artifact_on_blocking_thread(engine, source_for_compile)
        })
        .await
        .map_err(|error| {
            EngineError::Internal(format!("Componentization worker failed: {error}"))
        })??;

        let export_context = self
            .spec
            .clone()
            .zip(self.spec_text.clone())
            .map(|(spec, spec_text)| (spec, spec_text, self.policy.spec_assertions.clone()));
        let engine = self.engine.clone();
        let policy = self.policy.clone();
        let source_for_prepared = js_source.clone();
        let mut prepared = tokio::task::spawn_blocking(move || {
            prepare_compiled(
                engine,
                policy,
                compiled,
                source_for_prepared,
                SourceType::DirectJs,
                Vec::new(),
                supplemental_assertions,
            )
        })
        .await
        .map_err(|error| {
            PreparationError::Runtime(EngineError::Internal(format!(
                "Candidate verification worker failed: {error}"
            )))
        })??;
        if let Some((spec, spec_text, trusted_assertions)) = export_context {
            prepared.export_data = Some(GenerationExport {
                spec,
                spec_text,
                module_hash: prepared.module_hash.clone(),
                output: SynthesisOutput {
                    handler_code: js_source,
                    code_format: "javascript".to_string(),
                    assertions: Vec::new(),
                    rationale: None,
                },
                component_bytes,
                trusted_assertions,
                policy_mode: "operator".to_string(),
                repair_summary: RepairSummary::default(),
            });
        }
        Ok(prepared)
    }

    pub async fn prepare_from_spec(
        self,
        supplemental_assertions: Vec<ContractAssertion>,
    ) -> Result<PreparedGeneration, PreparationError> {
        if self.spec.is_none() {
            return Err(PreparationError::Runtime(EngineError::Internal(
                "No operator SPEC is loaded; provide draft_spec to prepare an agent-authored contract".into(),
            )));
        }
        let synthesizer = Synthesizer::new(LlmConfig::from_env()?);
        self.prepare_from_spec_with_synthesizer(supplemental_assertions, &synthesizer)
            .await
    }

    /// Synthesize from an agent-provided draft SPEC without changing the acceptance policy.
    pub async fn prepare_from_draft_spec(
        self,
        spec_text: String,
        supplemental_assertions: Vec<ContractAssertion>,
    ) -> Result<PreparedGeneration, PreparationError> {
        let synthesizer = Synthesizer::new(LlmConfig::from_env()?);
        self.prepare_from_draft_spec_with_synthesizer(
            spec_text,
            supplemental_assertions,
            synthesizer,
        )
        .await
    }

    /// Testable synthesis seam shared by MCP-first draft preparation and integration tests.
    pub(crate) async fn prepare_from_draft_spec_with_synthesizer(
        mut self,
        spec_text: String,
        supplemental_assertions: Vec<ContractAssertion>,
        synthesizer: Synthesizer,
    ) -> Result<PreparedGeneration, PreparationError> {
        if spec_text.len() > MAX_JS_SOURCE_BYTES {
            return Err(PreparationError::Runtime(EngineError::Internal(
                "Draft SPEC exceeds the 2 MiB input limit".into(),
            )));
        }
        let draft = ServiceSpec::parse(&spec_text).map_err(|error| {
            PreparationError::Runtime(EngineError::Internal(format!(
                "Invalid draft SPEC: {error}"
            )))
        })?;
        if draft.capabilities.state != "volatile"
            || !draft.capabilities.egress.is_empty()
            || !draft.schema.is_empty()
        {
            return Err(PreparationError::Runtime(EngineError::Internal(
                "Draft SPEC requests capabilities unavailable in this runtime".into(),
            )));
        }
        draft.validate_verification().map_err(|error| {
            PreparationError::Runtime(EngineError::Internal(format!(
                "Invalid draft SPEC verification: {error}"
            )))
        })?;
        let draft_policy = AcceptancePolicy::from_spec(&draft).map_err(EngineError::Internal)?;
        // These cases are candidate checks, not operator-trusted acceptance policy.
        self.draft_assertions = draft_policy.spec_assertions;
        // The draft controls synthesis input only; the startup acceptance policy is unchanged.
        self.spec = Some(draft);
        self.spec_text = Some(spec_text);
        self.draft_spec = true;
        self.prepare_from_spec_with_synthesizer(supplemental_assertions, &synthesizer)
            .await
    }

    async fn prepare_from_spec_with_synthesizer(
        self,
        supplemental_assertions: Vec<ContractAssertion>,
        synthesizer: &Synthesizer,
    ) -> Result<PreparedGeneration, PreparationError> {
        validate_assertions(&supplemental_assertions, "supplemental")?;
        self.policy.validate().map_err(EngineError::Internal)?;
        let spec = self
            .spec
            .as_ref()
            .ok_or_else(|| EngineError::Internal("No SPEC is available for synthesis".into()))?
            .clone();

        let spec_text = self.spec_text.clone();
        let synthesis_cache_key = synthesizer.synthesis_cache_key(&spec);
        let cached = match self.cache.clone() {
            Some(cache) => {
                let key = synthesis_cache_key.clone();
                tokio::task::spawn_blocking(move || cache.load(&key))
                    .await
                    .map_err(|error| {
                        EngineError::Internal(format!("Artifact cache worker failed: {error}"))
                    })?
            }
            None => None,
        };

        // Cached synthesis remains reusable across toolchain changes. Component bytes
        // are reused only when their independent identity matches the current build.
        let mut repair_summary = RepairSummary::default();
        let mut cached_candidate = match cached {
            Some(cached) => {
                if let Err(error) = validate_synthesis_output(&cached.synthesis) {
                    repair_summary
                        .failure_categories
                        .push("cached_artifact_invalid".into());
                    tracing::warn!(
                        failure_category = "cached_artifact_invalid",
                        "Cached synthesis artifact was rejected; synthesizing a fresh candidate"
                    );
                    tracing::debug!(%error, "Cached artifact validation failure");
                    None
                } else {
                    let expected_component_key = synthesizer.component_cache_key(&cached.synthesis);
                    let synthesis = cached.synthesis;
                    let component_bytes = cached.component_bytes;
                    let cached_component_key = cached.component_cache_key;
                    let engine = self.engine.clone();

                    if cached_component_key == expected_component_key {
                        let compile_bytes = component_bytes.clone();
                        let compiled = tokio::task::spawn_blocking(move || {
                            engine.compile_component(&compile_bytes)
                        })
                        .await
                        .map_err(|error| {
                            EngineError::Internal(format!(
                                "Cached component compile worker failed: {error}"
                            ))
                        })?;

                        match compiled {
                            Ok(compiled) => Some((synthesis, compiled, component_bytes, true)),
                            Err(error) => {
                                repair_summary
                                    .failure_categories
                                    .push("cached_componentization_failed".into());
                                tracing::warn!(
                                    failure_category = "cached_componentization_failed",
                                    "Cached component failed componentization; synthesizing a fresh candidate"
                                );
                                tracing::debug!(%error, "Cached componentization failure");
                                None
                            }
                        }
                    } else {
                        let source = synthesis.handler_code.clone();
                        let compiled = tokio::task::spawn_blocking(move || {
                            compile_handler_with_artifact_on_blocking_thread(engine, source)
                        })
                        .await
                        .map_err(|error| {
                            EngineError::Internal(format!(
                                "Cached synthesis componentization worker failed: {error}"
                            ))
                        })?;

                        match compiled {
                            Ok((compiled, component_bytes)) => {
                                tracing::debug!(
                                    "Reusing cached synthesis and rebuilding its component for the current toolchain"
                                );
                                Some((synthesis, compiled, component_bytes, false))
                            }
                            Err(error) => {
                                repair_summary
                                    .failure_categories
                                    .push("cached_componentization_failed".into());
                                tracing::warn!(
                                    failure_category = "cached_componentization_failed",
                                    "Cached synthesis could not be componentized with the current toolchain; synthesizing a fresh candidate"
                                );
                                tracing::debug!(%error, "Cached synthesis componentization failure");
                                None
                            }
                        }
                    }
                }
            }
            None => None,
        };

        let source_type = if self.draft_spec {
            SourceType::DraftSpec
        } else {
            SourceType::Spec
        };
        let mut repair_feedback = None;
        let mut accepted = None;
        let mut synthesis_candidate_number = 0usize;

        // attempt is the zero-based replacement index: 0 is the initial candidate,
        // 1 and 2 are the two permitted replacement candidates.
        for attempt in 0..=MAX_REPLACEMENT_ATTEMPTS {
            if cached_candidate.is_some() {
                repair_summary.cached_candidates += 1;
            }
            let (candidate_output, compiled, component_bytes, cache_hit) = if let Some((
                output,
                compiled,
                bytes,
                cache_hit,
            )) =
                cached_candidate.take()
            {
                (output, compiled, bytes, cache_hit)
            } else {
                // Provider transport retries are internal to synthesize and do not
                // consume this candidate-repair budget.
                synthesis_candidate_number += 1;
                repair_summary.synthesis_candidates = synthesis_candidate_number;
                if attempt > 0 {
                    repair_summary.replacement_attempts += 1;
                }
                let output = synthesizer
                    .synthesize_with_feedback(&spec, &[], repair_feedback.as_deref())
                    .await?;
                if let Err(error) = validate_synthesis_output(&output) {
                    if !repair_summary
                        .failure_categories
                        .iter()
                        .any(|item| item == "invalid_candidate_output")
                    {
                        repair_summary
                            .failure_categories
                            .push("invalid_candidate_output".into());
                    }
                    tracing::warn!(
                        candidate_number = attempt + 1,
                        is_replacement = attempt > 0,
                        failure_category = "invalid_candidate_output",
                        "Candidate output failed validation; requesting bounded replacement"
                    );
                    tracing::debug!(%error, "Candidate output validation failure");
                    if attempt < MAX_REPLACEMENT_ATTEMPTS {
                        repair_feedback = Some(
                            "Failure category: invalid_candidate_output\nThe response did not satisfy the required JavaScript artifact and assertion schema. Return a complete valid candidate.".to_string(),
                        );
                        continue;
                    }
                    return Err(error.into());
                }

                let engine = self.engine.clone();
                let source = output.handler_code.clone();
                let compiled = tokio::task::spawn_blocking(move || {
                    compile_handler_with_artifact_on_blocking_thread(engine, source)
                })
                .await
                .map_err(|error| {
                    PreparationError::Runtime(EngineError::Internal(format!(
                        "Componentization worker failed: {error}"
                    )))
                })?;
                match compiled {
                    Ok((compiled, bytes)) => (output, compiled, bytes, false),
                    Err(error) => {
                        if !repair_summary
                            .failure_categories
                            .iter()
                            .any(|item| item == "componentization_failed")
                        {
                            repair_summary
                                .failure_categories
                                .push("componentization_failed".into());
                        }
                        tracing::warn!(
                            candidate_number = attempt + 1,
                            is_replacement = attempt > 0,
                            failure_category = "componentization_failed",
                            "Candidate failed componentization"
                        );
                        tracing::debug!(%error, "Candidate componentization failure");
                        if attempt < MAX_REPLACEMENT_ATTEMPTS {
                            repair_feedback = Some(
                                "Failure category: componentization_failed\nThe generated JavaScript could not be componentized into the required handler module. Correct syntax and module structure; return the complete artifact.".to_string(),
                            );
                            continue;
                        }
                        return Err(PreparationError::Runtime(error));
                    }
                }
            };

            let cache_output = candidate_output.clone();
            let mut prepared_output = candidate_output;
            prepared_output
                .assertions
                .extend(self.draft_assertions.clone());
            if let Err(error) = validate_synthesis_output(&prepared_output) {
                if !repair_summary
                    .failure_categories
                    .iter()
                    .any(|item| item == "invalid_candidate_output")
                {
                    repair_summary
                        .failure_categories
                        .push("invalid_candidate_output".into());
                }
                tracing::warn!(
                    candidate_number = attempt + 1,
                    is_replacement = attempt > 0,
                    failure_category = "invalid_candidate_output",
                    "Candidate failed final artifact validation"
                );
                tracing::debug!(%error, "Final candidate validation failure");
                if attempt < MAX_REPLACEMENT_ATTEMPTS {
                    repair_feedback = Some(
                        "Failure category: invalid_candidate_output\nThe candidate plus draft checks exceeds the accepted artifact limits. Return a valid bounded artifact.".to_string(),
                    );
                    continue;
                }
                return Err(error.into());
            }

            let engine = self.engine.clone();
            let policy = self.policy.clone();
            let model_assertions = prepared_output.assertions.clone();
            let js_source = prepared_output.handler_code.clone();
            let supplemental = supplemental_assertions.clone();
            let verification = tokio::task::spawn_blocking(move || {
                prepare_compiled(
                    engine,
                    policy,
                    compiled,
                    js_source,
                    source_type,
                    model_assertions,
                    supplemental,
                )
            })
            .await
            .map_err(|error| {
                PreparationError::Runtime(EngineError::Internal(format!(
                    "Candidate verification worker failed: {error}"
                )))
            })?;

            match verification {
                Ok(prepared) => {
                    accepted = Some((
                        prepared,
                        prepared_output,
                        cache_output,
                        component_bytes,
                        cache_hit,
                    ));
                    break;
                }
                Err(error) => {
                    if let Some(feedback) = build_repair_feedback(&error) {
                        if !repair_summary
                            .failure_categories
                            .iter()
                            .any(|item| item == "verification_failed")
                        {
                            repair_summary
                                .failure_categories
                                .push("verification_failed".into());
                        }
                        tracing::warn!(
                            candidate_number = attempt + 1,
                            is_replacement = attempt > 0,
                            failure_category = "verification_failed",
                            "Candidate failed verification"
                        );
                        if let PreparationError::Verification { suite, results } = &error {
                            let suite = match suite {
                                VerificationSuite::Operator => "operator",
                                VerificationSuite::Generated => "generated",
                            };
                            for result in results.iter().filter(|result| !result.passed) {
                                tracing::warn!(
                                    suite,
                                    case = result.case_index + 1,
                                    expected_status = result.expected_status,
                                    actual_status = ?result.actual_status,
                                    failure_kind = result.failure_kind.unwrap_or("assertion_failed"),
                                    "Verification case failed"
                                );
                            }
                        }
                        if attempt < MAX_REPLACEMENT_ATTEMPTS {
                            repair_feedback = Some(feedback);
                            continue;
                        }
                    }
                    return Err(error);
                }
            }
        }

        let (mut prepared, output, cache_output, component_bytes, cache_hit) = accepted
            .ok_or_else(|| {
                PreparationError::Runtime(EngineError::Internal(
                    "Synthesis repair attempts were exhausted without an accepted candidate".into(),
                ))
            })?;

        // A draft SPEC may be used as synthesis input while an operator SPEC
        // remains the immutable acceptance policy. The current bundle format has
        // one SPEC.md, so exporting that combination would misattribute the
        // operator-trusted assertions to the draft. Keep the generation usable,
        // but do not make it exportable until the bundle can preserve both sources.
        if let Some(spec_text) = spec_text.filter(|_| {
            has_unambiguous_export_provenance(self.draft_spec, &self.policy.spec_assertions)
        }) {
            prepared.export_data = Some(GenerationExport {
                spec,
                spec_text,
                module_hash: prepared.module_hash.clone(),
                output: output.clone(),
                component_bytes: component_bytes.clone(),
                trusted_assertions: self.policy.spec_assertions.clone(),
                policy_mode: if self.policy.spec_assertions.is_empty() {
                    "draft".to_string()
                } else {
                    "operator".to_string()
                },
                repair_summary: repair_summary.clone(),
            });
        }

        if !cache_hit {
            if let Some(cache) = self.cache.clone() {
                let key = synthesis_cache_key.clone();
                let component_key = synthesizer.component_cache_key(&cache_output);
                let output = cache_output;
                match tokio::task::spawn_blocking(move || {
                    cache.store_with_component_key(&key, &component_key, &output, &component_bytes)
                })
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "Could not persist verified MCP artifact cache entry");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Artifact cache worker failed");
                    }
                }
            }
        }
        Ok(prepared)
    }
}

fn assertion_summaries(results: &[AssertionResult]) -> Vec<AssertionSummary> {
    results
        .iter()
        .map(|result| AssertionSummary {
            name: result.name.clone(),
            passed: result.passed,
            expected_status: result.expected_status,
            expected_status_max: result.expected_status_max,
            actual_status: result.actual_status,
        })
        .collect()
}

fn failed_results(results: &[AssertionResult]) -> Vec<AssertionResult> {
    results
        .iter()
        .filter(|result| !result.passed)
        .cloned()
        .collect()
}

/// Merge high-water measurements from independent disposable verification instances.
fn merge_verification_telemetry(target: &mut RuntimeTelemetry, source: RuntimeTelemetry) {
    target.generation_fuel_high_water = match (
        target.generation_fuel_high_water,
        source.generation_fuel_high_water,
    ) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    };
    target.generation_memory_high_water_bytes = match (
        target.generation_memory_high_water_bytes,
        source.generation_memory_high_water_bytes,
    ) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    };
    target.instance_memory_high_water_bytes = match (
        target.instance_memory_high_water_bytes,
        source.instance_memory_high_water_bytes,
    ) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    };
    target.last_invocation_fuel_consumed = source
        .last_invocation_fuel_consumed
        .or(target.last_invocation_fuel_consumed);
    target.instance_recreations = target
        .instance_recreations
        .saturating_add(source.instance_recreations);
}

pub(super) fn prepare_compiled(
    engine: QuickJsEngine,
    policy: AcceptancePolicy,
    compiled: CompiledHandler,
    js_source: String,
    source_type: SourceType,
    model_assertions: Vec<ContractAssertion>,
    supplemental_assertions: Vec<ContractAssertion>,
) -> Result<PreparedGeneration, PreparationError> {
    validate_assertions(&model_assertions, "model-generated")?;
    validate_assertions(&supplemental_assertions, "supplemental")?;

    let instantiate = |engine: &QuickJsEngine| {
        engine.instantiate_with_limits(
            &compiled,
            policy.invariants.max_fuel,
            policy.invariants.max_memory_bytes,
            policy.invariants.max_response_bytes,
        )
    };

    // Capture resource telemetry from the disposable instances that actually run
    // verification. The fresh production instance below has not handled a request yet.
    let mut verification_telemetry = RuntimeTelemetry::default();

    // Operator-controlled policy is mandatory when configured. MCP-first mode has
    // no trusted application suite yet, but retains the same hard runtime ceilings.
    let trusted_results = if policy.spec_assertions.is_empty() {
        Vec::new()
    } else {
        let mut trusted_candidate = instantiate(&engine)?;
        let results = ShadowVerifier::new()
            .evaluate_handler(&mut trusted_candidate, &policy.spec_assertions)?;
        merge_verification_telemetry(
            &mut verification_telemetry,
            trusted_candidate.telemetry_snapshot(),
        );
        drop(trusted_candidate);
        let failed_trusted = failed_results(&results);
        if !failed_trusted.is_empty() {
            return Err(PreparationError::Verification {
                suite: VerificationSuite::Operator,
                results: failed_trusted,
            });
        }
        results
    };

    // Host-owned adversarial inputs run on their own disposable instance. These
    // probes check the request/response ABI only; operator policy remains the
    // independent source of application-specific acceptance criteria.
    let mut host_candidate = instantiate(&engine)?;
    let host_results = HostProbeVerifier.evaluate_handler(&mut host_candidate);
    merge_verification_telemetry(
        &mut verification_telemetry,
        host_candidate.telemetry_snapshot(),
    );
    drop(host_candidate);
    let failed_host_probes: Vec<String> = host_results
        .iter()
        .filter(|result| !result.passed)
        .map(|result| {
            format!(
                "{}: {}",
                result.name,
                result.error_message.as_deref().unwrap_or("probe failed")
            )
        })
        .collect();
    if !failed_host_probes.is_empty() {
        return Err(PreparationError::Runtime(EngineError::Internal(format!(
            "Host-generated adversarial probes failed: {}",
            failed_host_probes.join("; ")
        ))));
    }

    // The CLI invariant also applies to SPEC-driven MCP synthesis: generated
    // regression assertions must all pass, in sequence, on their own instance.
    let mut model_results = Vec::new();
    if source_type != SourceType::DirectJs {
        let mut model_candidate = instantiate(&engine)?;
        model_results =
            ShadowVerifier::new().evaluate_handler(&mut model_candidate, &model_assertions)?;
        merge_verification_telemetry(
            &mut verification_telemetry,
            model_candidate.telemetry_snapshot(),
        );
        drop(model_candidate);
        let failed_model = failed_results(&model_results);
        if !failed_model.is_empty() {
            return Err(PreparationError::Verification {
                suite: VerificationSuite::Generated,
                results: failed_model,
            });
        }
    }

    // Agent-supplied assertions are informational, but share one disposable
    // instance so stateful sequences (for example POST followed by GET) work.
    let mut supplemental_results = Vec::with_capacity(supplemental_assertions.len());
    if !supplemental_assertions.is_empty() {
        let mut supplemental_candidate = instantiate(&engine)?;
        for assertion in &supplemental_assertions {
            match ShadowVerifier::new()
                .evaluate_handler(&mut supplemental_candidate, std::slice::from_ref(assertion))
            {
                Ok(mut results) => supplemental_results.append(&mut results),
                Err(error) => supplemental_results.push(AssertionResult {
                    name: assertion.name.clone(),
                    case_index: supplemental_results.len(),
                    passed: false,
                    actual_status: None,
                    expected_status: assertion.expected_status,
                    expected_status_max: assertion
                        .expected_status_max
                        .unwrap_or(assertion.expected_status),
                    actual_headers: None,
                    actual_body: None,
                    failure_kind: Some("execution_error"),
                    error_message: Some(error.to_string()),
                }),
            }
        }
        merge_verification_telemetry(
            &mut verification_telemetry,
            supplemental_candidate.telemetry_snapshot(),
        );
    }

    // Production receives a fresh instance only after the trusted suite and,
    // for source=spec, the model-generated suite have passed.
    let handler = instantiate(&engine)?;
    Ok(PreparedGeneration {
        id: GenerationId(Uuid::now_v7().to_string()),
        sequence: 0,
        state: GenerationState::Ready,
        module_hash: compiled.module_hash,
        js_source,
        source_type,
        trusted_results,
        model_results,
        verification_telemetry,
        supplemental_results,
        created_at: Instant::now(),
        handler: Arc::new(Mutex::new(handler)),
        export_data: None,
    })
}

/// Owns one application's prepared and active generations. It is intentionally
/// single-process and single-application; MCP tool calls serialize state changes.
pub struct AppLifecycle {
    engine: QuickJsEngine,
    spec: Option<ServiceSpec>,
    spec_text: Option<String>,
    cache: Option<ArtifactCache>,
    policy: AcceptancePolicy,
    generations: HashMap<GenerationId, PreparedGeneration>,
    active_id: Option<GenerationId>,
    previous_id: Option<GenerationId>,
    next_sequence: u64,
    max_generations: usize,
    ingress_state: Arc<IngressState>,
}

impl AppLifecycle {
    pub fn new(spec: &ServiceSpec, cache: Option<ArtifactCache>) -> Result<Self, EngineError> {
        let policy = AcceptancePolicy::from_spec(spec).map_err(EngineError::Internal)?;
        policy.validate().map_err(EngineError::Internal)?;
        Self::with_policy(Some(spec.clone()), None, cache, policy)
    }

    pub fn new_with_spec_text(
        spec: &ServiceSpec,
        spec_text: String,
        cache: Option<ArtifactCache>,
    ) -> Result<Self, EngineError> {
        let policy = AcceptancePolicy::from_spec(spec).map_err(EngineError::Internal)?;
        policy.validate().map_err(EngineError::Internal)?;
        Self::with_policy(Some(spec.clone()), Some(spec_text), cache, policy)
    }

    pub fn new_mcp_first(cache: Option<ArtifactCache>) -> Result<Self, EngineError> {
        let policy = AcceptancePolicy::baseline();
        policy.validate().map_err(EngineError::Internal)?;
        Self::with_policy(None, None, cache, policy)
    }

    fn with_policy(
        spec: Option<ServiceSpec>,
        spec_text: Option<String>,
        cache: Option<ArtifactCache>,
        policy: AcceptancePolicy,
    ) -> Result<Self, EngineError> {
        Ok(Self {
            engine: QuickJsEngine::new()?,
            spec,
            spec_text,
            cache,
            policy,
            generations: HashMap::new(),
            active_id: None,
            previous_id: None,
            next_sequence: 1,
            max_generations: 5,
            ingress_state: Arc::new(IngressState::new()),
        })
    }

    /// Shared HTTP ingress state. MCP activation and rollback publish to this state.
    pub fn ingress_state(&self) -> Arc<IngressState> {
        self.ingress_state.clone()
    }

    pub fn preparation_context(&self) -> PreparationContext {
        PreparationContext {
            engine: self.engine.clone(),
            spec: self.spec.clone(),
            spec_text: self.spec_text.clone(),
            draft_spec: false,
            draft_assertions: Vec::new(),
            cache: self.cache.clone(),
            policy: self.policy.clone(),
        }
    }

    pub fn export_generation(&self, id: &GenerationId) -> Result<GenerationExport, EngineError> {
        let generation = self
            .generations
            .get(id)
            .ok_or_else(|| EngineError::Internal("Unknown generation ID".into()))?;
        generation.export_data.clone().ok_or_else(|| {
            EngineError::Internal(
                "Generation has no exportable SPEC provenance; exports require either an operator SPEC or a draft SPEC without a separately loaded operator acceptance policy"
                    .into(),
            )
        })
    }

    pub fn insert_prepared(&mut self, mut generation: PreparedGeneration) -> GenerationId {
        generation.sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        let id = generation.id.clone();
        self.generations.insert(id.clone(), generation);
        self.prune_old_generations();
        id
    }

    // Convenience methods for internal callers and lifecycle tests. MCP tool
    // handlers use PreparationContext directly so the lifecycle lock is not held.
    pub async fn prepare_from_js(
        &mut self,
        js_source: &str,
        supplemental_assertions: &[ContractAssertion],
    ) -> Result<GenerationId, PreparationError> {
        let candidate = self
            .preparation_context()
            .prepare_from_js(js_source.to_string(), supplemental_assertions.to_vec())
            .await?;
        Ok(self.insert_prepared(candidate))
    }

    pub async fn prepare_from_spec(
        &mut self,
        supplemental_assertions: &[ContractAssertion],
    ) -> Result<GenerationId, PreparationError> {
        let candidate = self
            .preparation_context()
            .prepare_from_spec(supplemental_assertions.to_vec())
            .await?;
        Ok(self.insert_prepared(candidate))
    }

    /// Atomically activate a generation that has already passed trusted verification.
    pub fn activate(&mut self, id: &GenerationId) -> Result<(), EngineError> {
        if self.policy.spec_assertions.is_empty() {
            return Err(EngineError::Internal(
                "Activation requires an operator-controlled SPEC with at least one trusted verification case; review the draft and restart nosrc with --spec".into(),
            ));
        }
        let candidate = self
            .generations
            .get(id)
            .ok_or_else(|| EngineError::Internal("Unknown generation ID".into()))?;
        if candidate.state != GenerationState::Ready {
            return Err(EngineError::Internal(
                "Only a ready generation can be activated".into(),
            ));
        }

        if let Some(previous) = self.active_id.take() {
            if let Some(generation) = self.generations.get_mut(&previous) {
                generation.state = GenerationState::Superseded;
            }
            self.previous_id = Some(previous);
        }

        let generation = self.generations.get_mut(id).ok_or_else(|| {
            EngineError::Internal("Generation disappeared during activation".into())
        })?;
        generation.state = GenerationState::Active;
        self.active_id = Some(id.clone());
        self.ingress_state.hot_swap(ActiveGeneration {
            generation_id: generation.sequence,
            module_hash: generation.module_hash.clone(),
            handler: generation.handler.clone(),
        });
        Ok(())
    }

    pub fn active_handler(&self) -> Result<Arc<Mutex<QuickJsHandler>>, EngineError> {
        let id = self
            .active_id
            .as_ref()
            .ok_or_else(|| EngineError::Internal("No active generation".into()))?;
        self.generations
            .get(id)
            .map(|generation| generation.handler.clone())
            .ok_or_else(|| EngineError::Internal("Active generation is no longer retained".into()))
    }

    pub fn invoke(
        &self,
        id: &GenerationId,
        request: &RequestEnvelope,
    ) -> Result<ResponseEnvelope, EngineError> {
        let generation = self
            .generations
            .get(id)
            .ok_or_else(|| EngineError::Internal("Unknown generation ID".into()))?;
        let mut handler = generation
            .handler
            .lock()
            .map_err(|_| EngineError::Internal("Generation handler lock was poisoned".into()))?;
        handler.invoke(request)
    }

    pub fn invoke_active(
        &self,
        request: &RequestEnvelope,
    ) -> Result<ResponseEnvelope, EngineError> {
        let id = self
            .active_id
            .as_ref()
            .ok_or_else(|| EngineError::Internal("No active generation".into()))?;
        self.invoke(id, request)
    }

    pub fn get_status(&self) -> AppStatus {
        let now = Instant::now();
        let mut generations: Vec<_> = self
            .generations
            .values()
            .map(|generation| GenerationSummary {
                generation_id: generation.id.as_str().to_string(),
                state: generation.state,
                module_hash: generation.module_hash.clone(),
                source_type: generation.source_type,
                trusted_assertions_passed: generation
                    .trusted_results
                    .iter()
                    .filter(|result| result.passed)
                    .count(),
                model_assertions_passed: generation
                    .model_results
                    .iter()
                    .filter(|result| result.passed)
                    .count(),
                model_assertions_failed: generation
                    .model_results
                    .iter()
                    .filter(|result| !result.passed)
                    .count(),
                supplemental_assertions_passed: generation
                    .supplemental_results
                    .iter()
                    .filter(|result| result.passed)
                    .count(),
                supplemental_assertions_failed: generation
                    .supplemental_results
                    .iter()
                    .filter(|result| !result.passed)
                    .count(),
                age_seconds: now.duration_since(generation.created_at).as_secs(),
                trusted_assertions: assertion_summaries(&generation.trusted_results),
                model_assertions: assertion_summaries(&generation.model_results),
                supplemental_assertions: assertion_summaries(&generation.supplemental_results),
                telemetry: generation
                    .handler
                    .try_lock()
                    .ok()
                    .map(|handler| handler.telemetry_snapshot()),
            })
            .collect();
        generations.sort_by_key(|item| item.generation_id.clone());

        AppStatus {
            status: if self.active_id.is_some() {
                "active"
            } else if self.generations.is_empty() {
                "idle"
            } else {
                "ready"
            },
            active_generation: self.active_id.as_ref().map(|id| id.as_str().to_string()),
            previous_generation: self.previous_id.as_ref().map(|id| id.as_str().to_string()),
            generations,
            runtime_limits: RuntimeLimits {
                fuel_per_invocation: self.policy.invariants.max_fuel,
                memory_limit_bytes: self.policy.invariants.max_memory_bytes,
                response_limit_bytes: self.policy.invariants.max_response_bytes,
            },
        }
    }

    /// Reactivate the immediately preceding generation retained for rollback.
    pub fn rollback(&mut self) -> Result<GenerationId, EngineError> {
        let id = self.previous_id.clone().ok_or_else(|| {
            EngineError::Internal("No superseded generation is available for rollback".into())
        })?;
        let generation = self.generations.get(&id).ok_or_else(|| {
            EngineError::Internal("Previous generation is no longer retained".into())
        })?;
        if generation.state != GenerationState::Superseded {
            return Err(EngineError::Internal(
                "Previous generation is not in superseded state".into(),
            ));
        }

        let current = self.active_id.take();
        if let Some(current_id) = current.as_ref() {
            if let Some(generation) = self.generations.get_mut(current_id) {
                generation.state = GenerationState::Superseded;
            }
        }
        let previous = self.generations.get_mut(&id).ok_or_else(|| {
            EngineError::Internal("Previous generation is no longer retained".into())
        })?;
        previous.state = GenerationState::Active;
        self.active_id = Some(id.clone());
        self.previous_id = current;
        self.ingress_state.hot_swap(ActiveGeneration {
            generation_id: previous.sequence,
            module_hash: previous.module_hash.clone(),
            handler: previous.handler.clone(),
        });
        Ok(id)
    }

    fn prune_old_generations(&mut self) {
        while self.generations.len() > self.max_generations {
            let removable = self
                .generations
                .values()
                .filter(|generation| {
                    Some(&generation.id) != self.active_id.as_ref()
                        && Some(&generation.id) != self.previous_id.as_ref()
                })
                .min_by_key(|generation| generation.created_at)
                .map(|generation| generation.id.clone());
            match removable {
                Some(id) => {
                    self.generations.remove(&id);
                }
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::quickjs::test_support::{self, HANDLER_SOURCE};

    fn test_spec() -> ServiceSpec {
        ServiceSpec::parse(
            "# Service: lifecycle-test\n\n## Capabilities\n- state: volatile\n- egress: []\n\n## Endpoints\n### `GET /health`\n- Response `200`: `healthy`\n\n## Verification\n### `GET /health`\n- Response `200`\n- Body contains: `healthy`\n",
        )
        .unwrap()
    }

    fn request(path: &str) -> RequestEnvelope {
        RequestEnvelope {
            method: "GET".into(),
            scheme: None,
            authority: None,
            path: path.into(),
            query: None,
            headers: vec![],
            body: vec![],
        }
    }

    async fn prepare_fixture(
        lifecycle: &mut AppLifecycle,
        source_type: SourceType,
        model_assertions: Vec<ContractAssertion>,
        supplemental_assertions: Vec<ContractAssertion>,
    ) -> Result<GenerationId, PreparationError> {
        let (engine, compiled) = test_support::compiled().await;
        let engine = engine.clone();
        let compiled = compiled.clone();
        let policy = lifecycle.policy.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            prepare_compiled(
                engine,
                policy,
                compiled,
                HANDLER_SOURCE.to_string(),
                source_type,
                model_assertions,
                supplemental_assertions,
            )
        })
        .await
        .map_err(|error| {
            PreparationError::Runtime(EngineError::Internal(format!(
                "Test preparation worker failed: {error}"
            )))
        })??;
        Ok(lifecycle.insert_prepared(prepared))
    }

    #[test]
    fn repair_feedback_never_exposes_operator_assertion_details() {
        let error = PreparationError::Verification {
            suite: VerificationSuite::Operator,
            results: vec![AssertionResult {
                name: "private operator assertion".into(),
                case_index: 7,
                passed: false,
                actual_status: Some(418),
                expected_status: 200,
                expected_status_max: 200,
                actual_headers: Some(HashMap::from([(
                    "x-private".into(),
                    "secret-header-value".into(),
                )])),
                actual_body: Some("secret expected response".into()),
                failure_kind: Some("body_mismatch"),
                error_message: Some("private diagnostic".into()),
            }],
        };

        let feedback = build_repair_feedback(&error).unwrap();
        assert!(feedback.contains("operator-owned verification"));
        assert!(feedback.contains("Failed checks: 1"));
        for private_value in [
            "private operator assertion",
            "secret-header-value",
            "secret expected response",
            "private diagnostic",
            "418",
            "body_mismatch",
        ] {
            assert!(!feedback.contains(private_value));
        }
    }

    async fn spawn_lifecycle_provider(
        handler: impl Fn(usize) -> (u16, String) + Send + Sync + 'static,
    ) -> (
        String,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let requests = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(handler);
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            post({
                let requests = requests.clone();
                move || {
                    let requests = requests.clone();
                    let handler = handler.clone();
                    async move {
                        let index = requests.fetch_add(1, Ordering::SeqCst);
                        let (status, body) = handler(index);
                        (StatusCode::from_u16(status).unwrap(), body)
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}/v1"), requests, task)
    }

    fn provider_completion(handler_code: &str) -> String {
        let synthesis = serde_json::json!({
            "handler_code": handler_code,
            "code_format": "javascript",
            "assertions": [{
                "name": "generated health regression",
                "request": { "method": "GET", "scheme": null, "authority": null, "path": "/health", "query": null, "headers": [], "body": [] },
                "expected_status": 200,
                "body_contains": "healthy"
            }],
            "rationale": "lifecycle repair test"
        });
        serde_json::json!({
            "choices": [{
                "message": {
                    "content": serde_json::to_string(&synthesis).unwrap()
                }
            }]
        })
        .to_string()
    }

    async fn assert_three_synthesis_candidates_are_requested(
        completion: String,
        model: &str,
    ) -> PreparationError {
        let (url, requests, provider) =
            spawn_lifecycle_provider(move |_| (200, completion.clone())).await;
        let spec = test_spec();
        let context = PreparationContext {
            engine: QuickJsEngine::new().unwrap(),
            spec: Some(spec.clone()),
            spec_text: None,
            draft_spec: false,
            draft_assertions: Vec::new(),
            cache: None,
            policy: AcceptancePolicy::from_spec(&spec).unwrap(),
        };
        let synthesizer = Synthesizer::new(LlmConfig {
            api_key: "test-key".into(),
            base_url: url,
            model: model.into(),
            temperature: 0.0,
        });
        let result = context
            .prepare_from_spec_with_synthesizer(Vec::new(), &synthesizer)
            .await;
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 3);
        provider.abort();
        match result {
            Err(error) => error,
            Ok(_) => panic!("expected all three synthesis candidates to be rejected"),
        }
    }

    fn provider_completion_with_empty_assertions() -> String {
        let synthesis = serde_json::json!({
            "handler_code": HANDLER_SOURCE,
            "code_format": "javascript",
            "assertions": [],
            "rationale": "intentionally invalid assertion suite"
        });
        serde_json::json!({
            "choices": [{
                "message": {
                    "content": serde_json::to_string(&synthesis).unwrap()
                }
            }]
        })
        .to_string()
    }

    #[tokio::test]
    async fn invalid_candidate_output_exhausts_initial_plus_two_replacements() {
        let error = assert_three_synthesis_candidates_are_requested(
            provider_completion_with_empty_assertions(),
            "invalid-output-exhaustion-test",
        )
        .await;
        assert!(matches!(error, PreparationError::Runtime(_)));
    }

    #[tokio::test]
    async fn componentization_failure_exhausts_initial_plus_two_replacements() {
        let error = assert_three_synthesis_candidates_are_requested(
            provider_completion("export function handle(_request) {"),
            "componentization-exhaustion-test",
        )
        .await;
        assert!(matches!(error, PreparationError::Runtime(_)));
    }

    #[tokio::test]
    async fn exhausted_candidate_replacements_stop_after_initial_plus_two_replacements() {
        let (url, requests, provider) = spawn_lifecycle_provider(|_| {
            (
                200,
                provider_completion(
                    r#"export function handle(_request) {
                        return {
                            status: 500,
                            headers: [{ name: "content-type", value: [116,101,120,116,47,112,108,97,105,110] }],
                            body: { tag: "text", val: "candidate rejected" }
                        };
                    }"#,
                ),
            )
        })
        .await;
        let spec = test_spec();
        let engine = QuickJsEngine::new().unwrap();
        let context = PreparationContext {
            engine,
            spec: Some(spec.clone()),
            spec_text: None,
            draft_spec: false,
            draft_assertions: Vec::new(),
            cache: None,
            policy: AcceptancePolicy::from_spec(&spec).unwrap(),
        };
        let synthesizer = Synthesizer::new(LlmConfig {
            api_key: "test-key".into(),
            base_url: url,
            model: "exhausted-repair-test".into(),
            temperature: 0.0,
        });

        let error = match context
            .prepare_from_spec_with_synthesizer(Vec::new(), &synthesizer)
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("expected candidate preparation to fail"),
        };
        assert!(matches!(error, PreparationError::Verification { .. }));
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 3);
        provider.abort();
    }

    #[tokio::test]
    async fn provider_failure_does_not_start_candidate_repair_attempts() {
        let (url, requests, provider) =
            spawn_lifecycle_provider(|_| (401, r#"{"error":"unauthorized"}"#.into())).await;
        let spec = test_spec();
        let engine = QuickJsEngine::new().unwrap();
        let context = PreparationContext {
            engine,
            spec: Some(spec.clone()),
            spec_text: None,
            draft_spec: false,
            draft_assertions: Vec::new(),
            cache: None,
            policy: AcceptancePolicy::from_spec(&spec).unwrap(),
        };
        let synthesizer = Synthesizer::new(LlmConfig {
            api_key: "test-key".into(),
            base_url: url,
            model: "provider-failure-repair-test".into(),
            temperature: 0.0,
        });

        let error = match context
            .prepare_from_spec_with_synthesizer(Vec::new(), &synthesizer)
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("expected candidate preparation to fail"),
        };
        assert!(matches!(error, PreparationError::Runtime(_)));
        // A 401 is not retryable by Synthesizer (only 429 and 5xx are), and
        // provider failures terminate before the candidate-repair loop can retry.
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        provider.abort();
    }

    #[tokio::test]
    async fn cached_candidate_verification_failure_is_repaired_before_cache_replacement() {
        let spec = test_spec();
        let engine = QuickJsEngine::new().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(cache_dir.path());
        let (provider_url, requests, provider) =
            spawn_lifecycle_provider(|_| (200, provider_completion(HANDLER_SOURCE))).await;
        let synthesizer = Synthesizer::new(LlmConfig {
            api_key: "test-key".into(),
            base_url: provider_url.clone(),
            model: "cached-repair-test".into(),
            temperature: 0.0,
        });
        let rejected_source = r#"export function handle(_request) {
            return {
                status: 500,
                headers: [{ name: "content-type", value: [116,101,120,116,47,112,108,97,105,110] }],
                body: { tag: "text", val: "cached candidate rejected" }
            };
        }"#;
        let (_, rejected_component) = engine
            .compile_handler_with_artifact(rejected_source)
            .await
            .unwrap();
        let rejected_output = SynthesisOutput {
            handler_code: rejected_source.into(),
            code_format: "javascript".into(),
            assertions: vec![ContractAssertion {
                name: "cached candidate health regression".into(),
                request: request("/health"),
                expected_status: 200,
                expected_status_max: None,
                body_contains: Some("healthy".into()),
                header_matches: None,
                response_json_schema: None,
                json_path_assertions: Vec::new(),
            }],
            rationale: Some("intentionally failing cached candidate".into()),
        };
        let cache_key = synthesizer.synthesis_cache_key(&spec);
        cache
            .store(&cache_key, &rejected_output, &rejected_component)
            .unwrap();

        let context = PreparationContext {
            engine,
            spec: Some(spec.clone()),
            spec_text: Some("test SPEC provenance".into()),
            draft_spec: false,
            draft_assertions: Vec::new(),
            cache: Some(cache.clone()),
            policy: AcceptancePolicy::from_spec(&spec).unwrap(),
        };

        let prepared = context
            .prepare_from_spec_with_synthesizer(Vec::new(), &synthesizer)
            .await
            .unwrap();
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(prepared.trusted_results.iter().all(|result| result.passed));
        let repair_summary = &prepared.export_data.as_ref().unwrap().repair_summary;
        assert_eq!(repair_summary.cached_candidates, 1);
        assert_eq!(repair_summary.synthesis_candidates, 1);
        assert_eq!(repair_summary.replacement_attempts, 1);
        assert_eq!(
            repair_summary.failure_categories,
            vec!["verification_failed".to_string()]
        );

        let cached = cache.load(&synthesizer.synthesis_cache_key(&spec)).unwrap();
        assert_eq!(cached.synthesis.handler_code, HANDLER_SOURCE);
        assert!(!cached
            .synthesis
            .handler_code
            .contains("cached candidate rejected"));
        provider.abort();
    }

    #[tokio::test]
    async fn rejected_cached_component_falls_back_to_initial_synthesis_candidate() {
        let spec = test_spec();
        let engine = QuickJsEngine::new().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(cache_dir.path());
        let (provider_url, requests, provider) =
            spawn_lifecycle_provider(|_| (200, provider_completion(HANDLER_SOURCE))).await;
        let synthesizer = Synthesizer::new(LlmConfig {
            api_key: "test-key".into(),
            base_url: provider_url,
            model: "cached-component-fallback-test".into(),
            temperature: 0.0,
        });
        let cached_output = SynthesisOutput {
            handler_code: HANDLER_SOURCE.into(),
            code_format: "javascript".into(),
            assertions: vec![ContractAssertion {
                name: "cached assertion".into(),
                request: request("/health"),
                expected_status: 200,
                expected_status_max: None,
                body_contains: Some("healthy".into()),
                header_matches: None,
                response_json_schema: None,
                json_path_assertions: Vec::new(),
            }],
            rationale: Some("invalid cached component fixture".into()),
        };
        let cache_key = synthesizer.synthesis_cache_key(&spec);
        let component_key = synthesizer.component_cache_key(&cached_output);
        cache
            .store_with_component_key(
                &cache_key,
                &component_key,
                &cached_output,
                b"not a valid component",
            )
            .unwrap();

        let context = PreparationContext {
            engine,
            spec: Some(spec.clone()),
            spec_text: Some("test SPEC provenance".into()),
            draft_spec: false,
            draft_assertions: Vec::new(),
            cache: Some(cache),
            policy: AcceptancePolicy::from_spec(&spec).unwrap(),
        };
        let prepared = context
            .prepare_from_spec_with_synthesizer(Vec::new(), &synthesizer)
            .await
            .unwrap();

        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(prepared.trusted_results.iter().all(|result| result.passed));
        let summary = &prepared.export_data.as_ref().unwrap().repair_summary;
        assert_eq!(summary.cached_candidates, 0);
        assert_eq!(summary.synthesis_candidates, 1);
        assert_eq!(summary.replacement_attempts, 0);
        assert_eq!(
            summary.failure_categories,
            vec!["cached_componentization_failed".to_string()]
        );
        provider.abort();
    }

    #[tokio::test]
    async fn rejected_cached_candidate_is_not_replaced_when_all_replacements_fail() {
        let spec = test_spec();
        let engine = QuickJsEngine::new().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(cache_dir.path());
        let rejected_source = r#"export function handle(_request) {
            return {
                status: 500,
                headers: [{ name: "content-type", value: [116,101,120,116,47,112,108,97,105,110] }],
                body: { tag: "text", val: "cached candidate rejected" }
            };
        }"#;
        let (_, rejected_component) = engine
            .compile_handler_with_artifact(rejected_source)
            .await
            .unwrap();
        let rejected_output = SynthesisOutput {
            handler_code: rejected_source.into(),
            code_format: "javascript".into(),
            assertions: vec![ContractAssertion {
                name: "cached candidate health regression".into(),
                request: request("/health"),
                expected_status: 200,
                expected_status_max: None,
                body_contains: Some("healthy".into()),
                header_matches: None,
                response_json_schema: None,
                json_path_assertions: Vec::new(),
            }],
            rationale: Some("intentionally failing cached candidate".into()),
        };
        let (provider_url, requests, provider) =
            spawn_lifecycle_provider(|_| (200, provider_completion(rejected_source))).await;
        let synthesizer = Synthesizer::new(LlmConfig {
            api_key: "test-key".into(),
            base_url: provider_url,
            model: "cached-exhaustion-test".into(),
            temperature: 0.0,
        });
        let cache_key = synthesizer.synthesis_cache_key(&spec);
        cache
            .store(&cache_key, &rejected_output, &rejected_component)
            .unwrap();
        let context = PreparationContext {
            engine,
            spec: Some(spec.clone()),
            spec_text: None,
            draft_spec: false,
            draft_assertions: Vec::new(),
            cache: Some(cache.clone()),
            policy: AcceptancePolicy::from_spec(&spec).unwrap(),
        };

        let error = match context
            .prepare_from_spec_with_synthesizer(Vec::new(), &synthesizer)
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("expected candidate preparation to fail"),
        };
        assert!(matches!(error, PreparationError::Verification { .. }));
        // The cache hit is candidate one; only two replacement candidates are requested.
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 2);
        let still_cached = cache.load(&cache_key).unwrap();
        assert_eq!(still_cached.synthesis.handler_code, rejected_source);
        provider.abort();
    }

    #[test]
    fn synthesized_handler_size_limit_is_enforced_by_shared_validation() {
        let output = SynthesisOutput {
            handler_code: "x".repeat(MAX_JS_SOURCE_BYTES + 1),
            code_format: "javascript".into(),
            assertions: vec![ContractAssertion {
                name: "bounded handler".into(),
                request: request("/health"),
                expected_status: 200,
                expected_status_max: None,
                body_contains: None,
                header_matches: None,
                response_json_schema: None,
                json_path_assertions: Vec::new(),
            }],
            rationale: None,
        };

        let error = validate_synthesis_output(&output).unwrap_err();
        assert!(error.to_string().contains("2 MiB source limit"));
    }

    #[tokio::test]
    async fn oversized_cached_synthesis_is_rejected_and_falls_back_to_provider() {
        let spec = test_spec();
        let engine = QuickJsEngine::new().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(cache_dir.path());
        let synthesizer = Synthesizer::new(LlmConfig {
            api_key: String::new(),
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "oversized-cache-test".into(),
            temperature: 0.0,
        });
        let output = SynthesisOutput {
            handler_code: "x".repeat(MAX_JS_SOURCE_BYTES + 1),
            code_format: "javascript".into(),
            assertions: vec![ContractAssertion {
                name: "cached assertion".into(),
                request: request("/health"),
                expected_status: 200,
                expected_status_max: None,
                body_contains: None,
                header_matches: None,
                response_json_schema: None,
                json_path_assertions: Vec::new(),
            }],
            rationale: Some("legacy oversized cache entry".into()),
        };
        // Invalid component bytes ensure the size check runs before the
        // compile-failure fallback attempts synthesis (which has no API key).
        let cache_key = synthesizer.synthesis_cache_key(&spec);
        cache
            .store(&cache_key, &output, b"not a valid component")
            .unwrap();

        let context = PreparationContext {
            engine,
            spec: Some(spec.clone()),
            spec_text: Some("# Service: lifecycle-test".into()),
            draft_spec: false,
            draft_assertions: Vec::new(),
            cache: Some(cache),
            policy: AcceptancePolicy::from_spec(&spec).unwrap(),
        };
        let error = match context
            .prepare_from_spec_with_synthesizer(Vec::new(), &synthesizer)
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("oversized cached synthesis metadata must be rejected"),
        };

        // The invalid cached synthesis must be rejected, then preparation must
        // attempt fresh synthesis. With no API key configured, that fallback fails
        // before making a provider request.
        assert!(format!("{error:?}").contains("LLM_API_KEY"));
    }

    #[test]
    fn draft_export_is_blocked_when_operator_assertions_have_separate_provenance() {
        let trusted = vec![ContractAssertion {
            name: "operator assertion".into(),
            request: request("/health"),
            expected_status: 200,
            expected_status_max: None,
            body_contains: None,
            header_matches: None,
            response_json_schema: None,
            json_path_assertions: Vec::new(),
        }];

        assert!(has_unambiguous_export_provenance(false, &trusted));
        assert!(has_unambiguous_export_provenance(false, &[]));
        assert!(has_unambiguous_export_provenance(true, &[]));
        assert!(!has_unambiguous_export_provenance(true, &trusted));
    }

    #[test]
    fn generation_ids_are_opaque_and_unique() {
        let first = GenerationId(Uuid::now_v7().to_string());
        let second = GenerationId(Uuid::now_v7().to_string());
        assert_ne!(first, second);
        assert!(!first.as_str().is_empty());
    }

    #[tokio::test]
    async fn candidate_must_pass_operator_spec_before_it_is_retained() {
        let spec = test_spec();
        let mut lifecycle = AppLifecycle::new(&spec, None).unwrap();
        let id = lifecycle
            .prepare_from_js(HANDLER_SOURCE, &[])
            .await
            .unwrap();

        let status = lifecycle.get_status();
        assert!(status.active_generation.is_none());
        assert_eq!(status.generations.len(), 1);
        assert_eq!(status.generations[0].generation_id, id.as_str());
        assert_eq!(status.generations[0].state, GenerationState::Ready);

        lifecycle.activate(&id).unwrap();
        assert_eq!(
            lifecycle.get_status().active_generation.as_deref(),
            Some(id.as_str())
        );
        let response = lifecycle.invoke_active(&request("/health")).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(
            response.body,
            crate::engine::http_envelope::ResponseBody::Text("healthy".into())
        );
    }

    #[tokio::test]
    async fn failed_trusted_verification_does_not_create_a_generation() {
        let spec = ServiceSpec::parse(
            "# Service: lifecycle-failed-trusted\n\n## Capabilities\n- state: volatile\n- egress: []\n\n## Endpoints\n### `GET /health`\n- Response `404`: not found\n\n## Verification\n### `GET /health`\n- Response `404`\n",
        )
        .unwrap();
        let mut lifecycle = AppLifecycle::new(&spec, None).unwrap();

        assert!(
            prepare_fixture(&mut lifecycle, SourceType::DirectJs, Vec::new(), Vec::new())
                .await
                .is_err()
        );
        assert!(lifecycle.get_status().generations.is_empty());
        assert!(lifecycle.get_status().active_generation.is_none());
    }

    #[tokio::test]
    async fn activation_and_rollback_preserve_the_previous_generation() {
        let spec = test_spec();
        let mut lifecycle = AppLifecycle::new(&spec, None).unwrap();

        let first = prepare_fixture(&mut lifecycle, SourceType::DirectJs, Vec::new(), Vec::new())
            .await
            .unwrap();
        lifecycle.activate(&first).unwrap();

        let second = prepare_fixture(&mut lifecycle, SourceType::DirectJs, Vec::new(), Vec::new())
            .await
            .unwrap();
        lifecycle.activate(&second).unwrap();

        let before_rollback = lifecycle.get_status();
        assert_eq!(
            before_rollback.active_generation.as_deref(),
            Some(second.as_str())
        );
        assert_eq!(
            before_rollback.previous_generation.as_deref(),
            Some(first.as_str())
        );

        let rolled_back = lifecycle.rollback().unwrap();
        assert_eq!(rolled_back, first);

        let after_rollback = lifecycle.get_status();
        assert_eq!(
            after_rollback.active_generation.as_deref(),
            Some(first.as_str())
        );
        assert_eq!(
            after_rollback.previous_generation.as_deref(),
            Some(second.as_str())
        );
        let first_state = after_rollback
            .generations
            .iter()
            .find(|generation| generation.generation_id == first.as_str())
            .unwrap();
        let second_state = after_rollback
            .generations
            .iter()
            .find(|generation| generation.generation_id == second.as_str())
            .unwrap();
        assert_eq!(first_state.state, GenerationState::Active);
        assert_eq!(second_state.state, GenerationState::Superseded);
    }

    #[tokio::test]
    async fn fuel_exhaustion_fails_closed_during_candidate_verification() {
        let spec = ServiceSpec::parse(
            "# Service: lifecycle-fuel-test\n\n## Capabilities\n- state: volatile\n- egress: []\n\n## Endpoints\n### `GET /loop`\n- Response `200`\n\n## Verification\n### `GET /loop`\n- Response `200`\n",
        )
        .unwrap();
        let mut lifecycle = AppLifecycle::new(&spec, None).unwrap();

        let error = prepare_fixture(&mut lifecycle, SourceType::DirectJs, Vec::new(), Vec::new())
            .await
            .unwrap_err();
        match error {
            PreparationError::Verification { results, .. } => {
                assert_eq!(results.len(), 1);
                assert!(results[0].actual_status.is_none());
                assert!(results[0]
                    .error_message
                    .as_deref()
                    .unwrap_or_default()
                    .contains("fuel"));
            }
            PreparationError::Runtime(other) => {
                panic!("Expected verification failure, got {other}")
            }
        }
        assert!(lifecycle.get_status().generations.is_empty());
        assert!(lifecycle.get_status().active_generation.is_none());
    }

    #[tokio::test]
    async fn supplemental_assertion_failures_are_informational() {
        let spec = test_spec();
        let mut lifecycle = AppLifecycle::new(&spec, None).unwrap();
        let supplemental = ContractAssertion {
            name: "supplemental expected failure".into(),
            request: request("/error"),
            expected_status: 200,
            expected_status_max: None,
            body_contains: None,
            header_matches: None,
            response_json_schema: None,
            json_path_assertions: Vec::new(),
        };

        let id = prepare_fixture(
            &mut lifecycle,
            SourceType::DirectJs,
            Vec::new(),
            vec![supplemental],
        )
        .await
        .unwrap();
        let status = lifecycle.get_status();
        let generation = status
            .generations
            .iter()
            .find(|generation| generation.generation_id == id.as_str())
            .unwrap();
        assert_eq!(generation.trusted_assertions_passed, 1);
        assert_eq!(generation.supplemental_assertions_failed, 1);
        assert_eq!(generation.state, GenerationState::Ready);
    }

    #[tokio::test]
    async fn supplemental_assertions_share_state_in_sequence() {
        let spec = test_spec();
        let mut lifecycle = AppLifecycle::new(&spec, None).unwrap();
        let first = ContractAssertion {
            name: "first request".into(),
            request: request("/counter"),
            expected_status: 200,
            expected_status_max: None,
            body_contains: Some("1".into()),
            header_matches: None,
            response_json_schema: None,
            json_path_assertions: Vec::new(),
        };
        let second = ContractAssertion {
            name: "second request".into(),
            request: request("/counter"),
            expected_status: 200,
            expected_status_max: None,
            body_contains: Some("2".into()),
            header_matches: None,
            response_json_schema: None,
            json_path_assertions: Vec::new(),
        };

        let id = prepare_fixture(
            &mut lifecycle,
            SourceType::DirectJs,
            Vec::new(),
            vec![first, second],
        )
        .await
        .unwrap();
        let status = lifecycle.get_status();
        let generation = status
            .generations
            .iter()
            .find(|generation| generation.generation_id == id.as_str())
            .unwrap();
        assert_eq!(generation.supplemental_assertions_passed, 2);
        assert_eq!(generation.supplemental_assertions_failed, 0);
    }

    #[tokio::test]
    async fn cached_synthesis_is_reused_when_component_toolchain_identity_changes() {
        let spec = test_spec();
        let engine = QuickJsEngine::new().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(cache_dir.path());
        let synthesizer = Synthesizer::new(LlmConfig {
            api_key: String::new(),
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "toolchain-cache-test".into(),
            temperature: 0.0,
        });
        let output = SynthesisOutput {
            handler_code: HANDLER_SOURCE.into(),
            code_format: "javascript".into(),
            assertions: vec![ContractAssertion {
                name: "cached health assertion".into(),
                request: request("/health"),
                expected_status: 200,
                expected_status_max: None,
                body_contains: Some("healthy".into()),
                header_matches: None,
                response_json_schema: None,
                json_path_assertions: Vec::new(),
            }],
            rationale: Some("cached synthesis".into()),
        };
        let synthesis_key = synthesizer.synthesis_cache_key(&spec);
        cache
            .store_with_component_key(
                &synthesis_key,
                &"a".repeat(64),
                &output,
                b"component from previous toolchain",
            )
            .unwrap();

        let context = PreparationContext {
            engine,
            spec: Some(spec.clone()),
            spec_text: Some("test SPEC provenance".into()),
            draft_spec: false,
            draft_assertions: Vec::new(),
            cache: Some(cache.clone()),
            policy: AcceptancePolicy::from_spec(&spec).unwrap(),
        };
        let prepared = context
            .prepare_from_spec_with_synthesizer(Vec::new(), &synthesizer)
            .await
            .expect("cached synthesis should recompile without an LLM request");

        assert!(prepared.trusted_results.iter().all(|result| result.passed));
        let refreshed = cache.load(&synthesis_key).unwrap();
        assert_eq!(
            refreshed.component_cache_key,
            synthesizer.component_cache_key(&output)
        );
        assert_eq!(refreshed.synthesis.handler_code, HANDLER_SOURCE);
    }

    #[tokio::test]
    async fn spec_preparation_loads_cached_artifact_and_verifies_model_suite() {
        let spec = test_spec();
        let engine = QuickJsEngine::new().unwrap();
        let (_compiled, component_bytes) = engine
            .compile_handler_with_artifact(HANDLER_SOURCE)
            .await
            .unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(cache_dir.path());
        let synthesizer = Synthesizer::new(LlmConfig {
            api_key: "test-key".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "unused-cache-test".into(),
            temperature: 0.0,
        });
        let cache_key = synthesizer.synthesis_cache_key(&spec);
        let output = SynthesisOutput {
            handler_code: HANDLER_SOURCE.to_string(),
            code_format: "javascript".into(),
            assertions: vec![ContractAssertion {
                name: "model health regression".into(),
                request: request("/health"),
                expected_status: 200,
                expected_status_max: None,
                body_contains: Some("healthy".into()),
                header_matches: None,
                response_json_schema: None,
                json_path_assertions: Vec::new(),
            }],
            rationale: Some("preseeded cache fixture".into()),
        };
        let component_key = synthesizer.component_cache_key(&output);
        cache
            .store_with_component_key(&cache_key, &component_key, &output, &component_bytes)
            .unwrap();

        let policy = AcceptancePolicy::from_spec(&spec).unwrap();
        let context = PreparationContext {
            engine,
            spec: Some(spec),
            spec_text: None,
            draft_spec: false,
            draft_assertions: Vec::new(),
            cache: Some(cache),
            policy,
        };
        let prepared = context
            .prepare_from_spec_with_synthesizer(Vec::new(), &synthesizer)
            .await
            .unwrap();

        assert_eq!(prepared.source_type, SourceType::Spec);
        assert_eq!(prepared.model_results.len(), 1);
        assert!(prepared.model_results[0].passed);
        assert_eq!(prepared.trusted_results.len(), 1);
        assert!(prepared.trusted_results[0].passed);
    }

    #[tokio::test]
    async fn spec_synthesis_requires_model_generated_assertions_to_pass() {
        let spec = test_spec();
        let mut lifecycle = AppLifecycle::new(&spec, None).unwrap();
        let failed_model_assertion = ContractAssertion {
            name: "model regression must fail candidate".into(),
            request: request("/health"),
            expected_status: 404,
            expected_status_max: None,
            body_contains: None,
            header_matches: None,
            response_json_schema: None,
            json_path_assertions: Vec::new(),
        };

        let result = prepare_fixture(
            &mut lifecycle,
            SourceType::Spec,
            vec![failed_model_assertion],
            Vec::new(),
        )
        .await;

        match result {
            Err(PreparationError::Verification { results, .. }) => {
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].expected_status, 404);
                assert_eq!(results[0].actual_status, Some(200));
            }
            _ => panic!("Expected model assertion rejection"),
        }
    }
}

#[cfg(test)]
mod evaluation_corpus;
