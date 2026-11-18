use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use uuid::Uuid;

use crate::artifact_cache::ArtifactCache;
use crate::engine::http_envelope::{RequestEnvelope, ResponseEnvelope};
use crate::engine::quickjs::{CompiledHandler, QuickJsEngine, QuickJsHandler};
use crate::error::EngineError;
use crate::ingress::{ActiveGeneration, IngressState};
use crate::spec::ServiceSpec;
use crate::synthesizer::{ContractAssertion, LlmConfig, SynthesisOutput, Synthesizer};
use crate::verifier::{AssertionResult, ShadowVerifier};

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

#[derive(Clone)]
pub struct GenerationExport {
    pub spec: ServiceSpec,
    pub spec_text: String,
    pub module_hash: String,
    pub output: SynthesisOutput,
    pub component_bytes: Vec<u8>,
    pub trusted_assertions: Vec<ContractAssertion>,
    pub policy_mode: String,
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
}

#[derive(Debug, Serialize)]
pub struct AssertionSummary {
    pub name: String,
    pub passed: bool,
    pub expected_status: u16,
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

#[derive(Debug)]
pub enum PreparationError {
    Runtime(EngineError),
    Verification(Vec<AssertionResult>),
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
        let request_header_bytes = assertion
            .request
            .headers
            .iter()
            .fold(0usize, |bytes, (name, value)| {
                bytes.saturating_add(name.len()).saturating_add(value.len())
            });
        let query_bytes = assertion
            .request
            .query
            .iter()
            .fold(0usize, |bytes, (name, value)| {
                bytes.saturating_add(name.len()).saturating_add(value.len())
            });
        let response_header_bytes = assertion
            .header_matches
            .as_ref()
            .map(|headers| {
                headers.iter().fold(0usize, |bytes, (name, value)| {
                    bytes.saturating_add(name.len()).saturating_add(value.len())
                })
            })
            .unwrap_or_default();
        total
            .saturating_add(assertion.name.len())
            .saturating_add(assertion.request.method.len())
            .saturating_add(assertion.request.path.len())
            .saturating_add(assertion.request.body.as_ref().map_or(0, String::len))
            .saturating_add(assertion.body_contains.as_ref().map_or(0, String::len))
            .saturating_add(request_header_bytes)
            .saturating_add(query_bytes)
            .saturating_add(response_header_bytes)
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
            .fold(0usize, |total, (name, value)| {
                total.saturating_add(name.len()).saturating_add(value.len())
            });
        if assertion.name.len() > MAX_ASSERTION_NAME_BYTES
            || assertion.request.path.len() > MAX_ASSERTION_PATH_BYTES
            || assertion
                .request
                .body
                .as_ref()
                .is_some_and(|body| body.len() > MAX_ASSERTION_BODY_BYTES)
            || assertion
                .body_contains
                .as_ref()
                .is_some_and(|body| body.len() > MAX_ASSERTION_BODY_BYTES)
            || assertion.request.headers.len() > MAX_ASSERTION_HEADER_COUNT
            || header_bytes > MAX_ASSERTION_HEADER_BYTES
            || assertion.request.query.len() > MAX_ASSERTION_QUERY_COUNT
        {
            return Err(EngineError::Internal(format!(
                "{label} assertion exceeds its input limits"
            )));
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

fn validate_mcp_synthesis(output: &SynthesisOutput) -> Result<(), EngineError> {
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
    cache: Option<ArtifactCache>,
    policy: AcceptancePolicy,
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
            .map(|(spec, spec_text)| {
                (
                    spec,
                    spec_text,
                    self.policy.spec_assertions.clone(),
                )
            });
        let engine = self.engine.clone();
        let policy = self.policy.clone();
        let mut prepared = tokio::task::spawn_blocking(move || {
            prepare_compiled(
                engine,
                policy,
                compiled,
                js_source.clone(),
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
        self.prepare_from_spec_with_synthesizer(supplemental_assertions, synthesizer)
            .await
    }

    /// Synthesize from an agent-provided draft SPEC without changing the acceptance policy.
    pub async fn prepare_from_draft_spec(
        mut self,
        spec_text: String,
        supplemental_assertions: Vec<ContractAssertion>,
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
        // The draft controls synthesis input only; the startup acceptance policy is unchanged.
        self.spec = Some(draft);
        self.spec_text = Some(spec_text);
        self.draft_spec = true;
        self.prepare_from_spec(supplemental_assertions).await
    }

    async fn prepare_from_spec_with_synthesizer(
        self,
        supplemental_assertions: Vec<ContractAssertion>,
        synthesizer: Synthesizer,
    ) -> Result<PreparedGeneration, PreparationError> {
        validate_assertions(&supplemental_assertions, "supplemental")?;
        self.policy.validate().map_err(EngineError::Internal)?;
        let spec = self
            .spec
            .as_ref()
            .ok_or_else(|| EngineError::Internal("No SPEC is available for synthesis".into()))?
            .clone();

        let spec_text = self.spec_text.clone().ok_or_else(|| {
            EngineError::Internal("Original SPEC text is unavailable for artifact export".into())
        })?;
        let cache_key = synthesizer.artifact_cache_key(&spec);
        let cached = match self.cache.clone() {
            Some(cache) => {
                let key = cache_key.clone();
                tokio::task::spawn_blocking(move || cache.load(&key))
                    .await
                    .map_err(|error| {
                        EngineError::Internal(format!("Artifact cache worker failed: {error}"))
                    })?
            }
            None => None,
        };

        let (output, compiled, component_bytes, cache_hit) = match cached {
            Some(cached) => {
                let engine = self.engine.clone();
                let component_bytes = cached.component_bytes;
                let compile_bytes = component_bytes.clone();
                let compiled =
                    tokio::task::spawn_blocking(move || engine.compile_component(&compile_bytes))
                        .await
                        .map_err(|error| {
                            EngineError::Internal(format!(
                                "Cached component compile worker failed: {error}"
                            ))
                        })?;
                match compiled {
                    Ok(compiled) => (cached.synthesis, compiled, component_bytes, true),
                    Err(error) => {
                        tracing::warn!(%error, "Cached MCP artifact failed compilation; synthesizing again");
                        let output = synthesizer.synthesize(&spec, &[]).await?;
                        validate_mcp_synthesis(&output)?;
                        let engine = self.engine.clone();
                        let source = output.handler_code.clone();
                        let (compiled, bytes) = tokio::task::spawn_blocking(move || {
                            compile_handler_with_artifact_on_blocking_thread(engine, source)
                        })
                        .await
                        .map_err(|error| {
                            EngineError::Internal(format!(
                                "Componentization worker failed: {error}"
                            ))
                        })??;
                        (output, compiled, bytes, false)
                    }
                }
            }
            None => {
                let output = synthesizer.synthesize(&spec, &[]).await?;
                validate_mcp_synthesis(&output)?;
                let engine = self.engine.clone();
                let source = output.handler_code.clone();
                let (compiled, bytes) = tokio::task::spawn_blocking(move || {
                    compile_handler_with_artifact_on_blocking_thread(engine, source)
                })
                .await
                .map_err(|error| {
                    EngineError::Internal(format!("Componentization worker failed: {error}"))
                })??;
                (output, compiled, bytes, false)
            }
        };

        validate_mcp_synthesis(&output)?;
        let engine = self.engine.clone();
        let policy = self.policy.clone();
        let model_assertions = output.assertions.clone();
        let js_source = output.handler_code.clone();
        let source_type = if self.draft_spec {
            SourceType::DraftSpec
        } else {
            SourceType::Spec
        };
        let prepared = tokio::task::spawn_blocking(move || {
            prepare_compiled(
                engine,
                policy,
                compiled,
                js_source,
                source_type,
                model_assertions,
                supplemental_assertions,
            )
        })
        .await
        .map_err(|error| {
            PreparationError::Runtime(EngineError::Internal(format!(
                "Candidate verification worker failed: {error}"
            )))
        })??;

        let mut prepared = prepared;
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
        });

        if !cache_hit {
            if let Some(cache) = self.cache.clone() {
                let key = cache_key.clone();
                let output = output.clone();
                match tokio::task::spawn_blocking(move || {
                    cache.store(&key, &output, &component_bytes)
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

    // Operator-controlled policy is mandatory when configured. MCP-first mode has
    // no trusted application suite yet, but retains the same hard runtime ceilings.
    let trusted_results = if policy.spec_assertions.is_empty() {
        Vec::new()
    } else {
        let mut trusted_candidate = instantiate(&engine)?;
        let results = ShadowVerifier::new()
            .evaluate_handler(&mut trusted_candidate, &policy.spec_assertions)?;
        drop(trusted_candidate);
        let failed_trusted = failed_results(&results);
        if !failed_trusted.is_empty() {
            return Err(PreparationError::Verification(failed_trusted));
        }
        results
    };

    // The CLI invariant also applies to SPEC-driven MCP synthesis: generated
    // regression assertions must all pass, in sequence, on their own instance.
    let mut model_results = Vec::new();
    if source_type != SourceType::DirectJs {
        let mut model_candidate = instantiate(&engine)?;
        model_results =
            ShadowVerifier::new().evaluate_handler(&mut model_candidate, &model_assertions)?;
        drop(model_candidate);
        let failed_model = failed_results(&model_results);
        if !failed_model.is_empty() {
            return Err(PreparationError::Verification(failed_model));
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
                    passed: false,
                    actual_status: None,
                    expected_status: assertion.expected_status,
                    error_message: Some(error.to_string()),
                }),
            }
        }
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
                "Generation has no exportable SPEC contract; prepare it with draft_spec first"
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
            path: path.into(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: None,
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
        assert_eq!(response.body.as_deref(), Some("healthy"));
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
            PreparationError::Verification(results) => {
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
            body_contains: None,
            header_matches: None,
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
            body_contains: Some("1".into()),
            header_matches: None,
        };
        let second = ContractAssertion {
            name: "second request".into(),
            request: request("/counter"),
            expected_status: 200,
            body_contains: Some("2".into()),
            header_matches: None,
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
        let cache_key = synthesizer.artifact_cache_key(&spec);
        let output = SynthesisOutput {
            handler_code: HANDLER_SOURCE.to_string(),
            code_format: "javascript".into(),
            assertions: vec![ContractAssertion {
                name: "model health regression".into(),
                request: request("/health"),
                expected_status: 200,
                body_contains: Some("healthy".into()),
                header_matches: None,
            }],
            rationale: Some("preseeded cache fixture".into()),
        };
        cache.store(&cache_key, &output, &component_bytes).unwrap();

        let policy = AcceptancePolicy::from_spec(&spec).unwrap();
        let context = PreparationContext {
            engine,
            spec: Some(spec),
            cache: Some(cache),
            policy,
        };
        let prepared = context
            .prepare_from_spec_with_synthesizer(Vec::new(), synthesizer)
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
            body_contains: None,
            header_matches: None,
        };

        let result = prepare_fixture(
            &mut lifecycle,
            SourceType::Spec,
            vec![failed_model_assertion],
            Vec::new(),
        )
        .await;

        match result {
            Err(PreparationError::Verification(results)) => {
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].expected_status, 404);
                assert_eq!(results[0].actual_status, Some(200));
            }
            _ => panic!("Expected model assertion rejection"),
        }
    }
}
