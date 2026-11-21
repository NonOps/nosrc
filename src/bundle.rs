use anyhow::{bail, Context};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::engine::quickjs::{QuickJsEngine, DEFAULT_COMPONENT_FUEL};
use crate::export::{ExportAssertions, ExportManifest};
use crate::ingress::ActiveGeneration;
use crate::verifier::ShadowVerifier;

const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_HANDLER_BYTES: u64 = 4 * 1024 * 1024;
const MAX_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ASSERTIONS_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SPEC_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ASSERTIONS_PER_SUITE: usize = 100;

/// Load a previously exported bundle, verify its integrity and both assertion
/// suites, then create a fresh production handler. This path never synthesizes
/// code, reads LLM credentials, or makes provider requests.
pub fn load_and_verify(bundle_dir: &Path) -> anyhow::Result<(ActiveGeneration, BundleSummary)> {
    let directory_metadata = fs::symlink_metadata(bundle_dir).with_context(|| {
        format!(
            "Failed to inspect bundle directory {}",
            bundle_dir.display()
        )
    })?;
    if !directory_metadata.file_type().is_dir() {
        bail!(
            "Bundle path {} must be a real directory, not a symlink or file",
            bundle_dir.display()
        );
    }

    // Read and validate every required artifact as a regular, bounded file.
    let manifest_bytes = read_bundle_file(bundle_dir, "manifest.json", MAX_MANIFEST_BYTES)?;
    let assertions_bytes = read_bundle_file(bundle_dir, "assertions.json", MAX_ASSERTIONS_BYTES)?;
    let component_bytes = read_bundle_file(bundle_dir, "component.wasm", MAX_COMPONENT_BYTES)?;
    let handler_bytes = read_bundle_file(bundle_dir, "handler.js", MAX_HANDLER_BYTES)?;

    if handler_bytes.is_empty() {
        bail!("Bundle handler.js must not be empty");
    }

    let manifest: ExportManifest = serde_json::from_slice(&manifest_bytes)
        .context("Bundle manifest.json is not a valid export manifest")?;
    let spec_digest = validate_digest("spec_sha256", &manifest.spec_sha256)?;
    let spec_bytes = read_optional_bundle_file(bundle_dir, "SPEC.md", MAX_SPEC_BYTES)?;
    if manifest.policy_mode == "draft" && spec_bytes.is_none() {
        bail!("Draft-policy bundle must contain SPEC.md");
    }
    if let Some(spec_bytes) = spec_bytes {
        let embedded_spec_digest = crate::hex_lower(Sha256::digest(&spec_bytes));
        if embedded_spec_digest != spec_digest {
            bail!("Bundle SPEC.md SHA-256 does not match manifest spec_sha256");
        }
    }
    let module_digest = validate_digest("module_hash", &manifest.module_hash)?;
    let component_manifest_digest =
        validate_digest("component_sha256", &manifest.component_sha256)?;
    if manifest.service_name.trim().is_empty() {
        bail!("Bundle manifest service_name must not be empty");
    }

    let component_digest = crate::hex_lower(Sha256::digest(&component_bytes));
    if component_digest != component_manifest_digest {
        bail!("Bundle component.wasm SHA-256 does not match manifest component_sha256");
    }
    if component_digest != module_digest {
        bail!("Bundle component.wasm SHA-256 does not match manifest module_hash");
    }

    let assertions: ExportAssertions = serde_json::from_slice(&assertions_bytes)
        .context("Bundle assertions.json is not a valid assertion document")?;
    match manifest.policy_mode.as_str() {
        "operator" if assertions.trusted_assertions.is_empty() => {
            bail!("Operator-policy bundle trusted assertion suite must not be empty");
        }
        "draft" if !assertions.trusted_assertions.is_empty() => {
            bail!("Draft-policy bundle must not contain operator-trusted assertions");
        }
        "draft" if assertions.generated_assertions.is_empty() => {
            bail!("Draft-policy bundle generated assertion suite must not be empty");
        }
        "operator" | "draft" => {}
        other => bail!("Unsupported bundle policy_mode '{other}'"),
    }
    if assertions.trusted_assertions.len() > MAX_ASSERTIONS_PER_SUITE
        || assertions.generated_assertions.len() > MAX_ASSERTIONS_PER_SUITE
    {
        bail!("Bundle assertion suite exceeds the {MAX_ASSERTIONS_PER_SUITE} assertion limit");
    }
    if assertions.trusted_assertions.len() != manifest.trusted_assertions_count {
        bail!("Bundle trusted assertion count does not match manifest");
    }
    if assertions.generated_assertions.len() != manifest.generated_assertions_count {
        bail!("Bundle generated assertion count does not match manifest");
    }

    // The engine independently compiles the component and derives its own hash.
    let engine = QuickJsEngine::new()?;
    let compiled = engine.compile_component(&component_bytes)?;
    if compiled.module_hash != module_digest {
        bail!("Compiled component hash does not match manifest module_hash");
    }

    // Keep non-empty suites isolated, matching source-driven startup semantics.
    // Empty suites are valid only where the manifest policy permits them: operator
    // direct-JS exports have no generated suite, while draft exports have no trusted
    // suite. Policy validation above rejects empty required suites.
    let trusted_results = if assertions.trusted_assertions.is_empty() {
        Vec::new()
    } else {
        let mut trusted_candidate = engine.instantiate(&compiled, DEFAULT_COMPONENT_FUEL)?;
        let results = ShadowVerifier::new()
            .verify_handler(&mut trusted_candidate, &assertions.trusted_assertions)
            .context("Bundle trusted assertion verification failed")?;
        drop(trusted_candidate);
        results
    };

    let generated_results = if assertions.generated_assertions.is_empty() {
        Vec::new()
    } else {
        let mut generated_candidate = engine.instantiate(&compiled, DEFAULT_COMPONENT_FUEL)?;
        let results = ShadowVerifier::new()
            .verify_handler(&mut generated_candidate, &assertions.generated_assertions)
            .context("Bundle generated assertion verification failed")?;
        drop(generated_candidate);
        results
    };

    // Only construct the serving instance after all integrity and verification checks pass.
    let handler = engine.instantiate(&compiled, DEFAULT_COMPONENT_FUEL)?;
    let generation = ActiveGeneration {
        generation_id: 1,
        module_hash: compiled.module_hash,
        handler: Arc::new(Mutex::new(handler)),
    };

    Ok((
        generation,
        BundleSummary {
            service_name: manifest.service_name,
            trusted_assertions: trusted_results.len(),
            generated_assertions: generated_results.len(),
            bundle_path: bundle_dir.to_path_buf(),
        },
    ))
}

#[derive(Debug, Clone)]
pub struct BundleSummary {
    pub service_name: String,
    pub trusted_assertions: usize,
    pub generated_assertions: usize,
    pub bundle_path: PathBuf,
}

fn read_optional_bundle_file(
    directory: &Path,
    name: &str,
    max_bytes: u64,
) -> anyhow::Result<Option<Vec<u8>>> {
    let path = directory.join(name);
    match fs::symlink_metadata(&path) {
        Ok(_) => read_bundle_file(directory, name, max_bytes).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to inspect optional bundle artifact {}",
                path.display()
            )
        }),
    }
}

fn read_bundle_file(directory: &Path, name: &str, max_bytes: u64) -> anyhow::Result<Vec<u8>> {
    let path = directory.join(name);
    let mut options = OpenOptions::new();
    options.read(true);

    // O_NOFOLLOW makes the path lookup and open one operation with respect to
    // the final path component, preventing a symlink swap on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }

    // On Windows, open the reparse point itself rather than following it.
    // The opened handle is then inspected for FILE_ATTRIBUTE_REPARSE_POINT.
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    // Other targets have no implemented no-follow primitive. Fail closed
    // rather than silently weakening the bundle's regular-file guarantee.
    #[cfg(not(any(unix, windows)))]
    bail!("Offline bundle serving is not supported on this platform: safe no-follow file opening is unavailable");

    let file = options.open(&path).with_context(|| {
        format!(
            "Failed to open bundle artifact {} without following symlinks",
            path.display()
        )
    })?;

    // Validate the opened object, not metadata obtained from a path lookup
    // that could refer to a different file by the time it is opened.
    let metadata = file.metadata().with_context(|| {
        format!(
            "Failed to inspect opened bundle artifact {}",
            path.display()
        )
    })?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            bail!(
                "Bundle artifact {} must not be a Windows reparse point",
                path.display()
            );
        }
    }
    if !metadata.file_type().is_file() {
        bail!(
            "Bundle artifact {} must be a regular file, not a symlink or special file",
            path.display()
        );
    }
    if metadata.len() > max_bytes {
        bail!(
            "Bundle artifact {} exceeds the {} byte limit",
            path.display(),
            max_bytes
        );
    }

    let mut bytes = Vec::with_capacity(metadata.len().min(max_bytes) as usize);
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Failed to read bundle artifact {}", path.display()))?;
    if bytes.len() as u64 > max_bytes {
        bail!(
            "Bundle artifact {} exceeds the {} byte limit",
            path.display(),
            max_bytes
        );
    }
    Ok(bytes)
}

fn validate_digest(field: &str, digest: &str) -> anyhow::Result<String> {
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Bundle manifest {field} must be a 64-character SHA-256 hex digest");
    }
    Ok(digest.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::http_envelope::RequestEnvelope;
    use crate::engine::quickjs::test_support;
    use crate::export::{ExportAssertions, ExportManifest};
    use crate::synthesizer::ContractAssertion;
    use std::collections::HashMap;

    fn assertion(name: &str) -> ContractAssertion {
        ContractAssertion {
            name: name.to_string(),
            request: RequestEnvelope {
                method: "GET".to_string(),
                path: "/health".to_string(),
                query: HashMap::new(),
                headers: HashMap::new(),
                body: None,
            },
            expected_status: 200,
            body_contains: Some("healthy".to_string()),
            header_matches: None,
        }
    }

    async fn write_valid_bundle(directory: &Path) {
        let engine = QuickJsEngine::new().unwrap();
        let (_, component_bytes) = engine
            .compile_handler_with_artifact(test_support::HANDLER_SOURCE)
            .await
            .unwrap();
        let component_hash = crate::hex_lower(Sha256::digest(&component_bytes));
        let manifest = ExportManifest {
            service_name: "Bundle test".into(),
            spec_sha256: "a".repeat(64),
            module_hash: component_hash.clone(),
            component_sha256: component_hash,
            trusted_assertions_count: 1,
            generated_assertions_count: 1,
            rationale: Some("test bundle".into()),
            policy_mode: "operator".into(),
        };
        let assertions = ExportAssertions {
            trusted_assertions: vec![assertion("trusted")],
            generated_assertions: vec![assertion("generated")],
        };
        fs::write(directory.join("handler.js"), test_support::HANDLER_SOURCE).unwrap();
        fs::write(directory.join("component.wasm"), component_bytes).unwrap();
        fs::write(
            directory.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(
            directory.join("assertions.json"),
            serde_json::to_vec(&assertions).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn loads_bundle_and_reverifies_both_suites() {
        let directory = tempfile::tempdir().unwrap();
        write_valid_bundle(directory.path()).await;

        let (generation, summary) = load_and_verify(directory.path()).unwrap();

        assert_eq!(summary.service_name, "Bundle test");
        assert_eq!(summary.trusted_assertions, 1);
        assert_eq!(summary.generated_assertions, 1);
        assert_eq!(generation.generation_id, 1);
    }

    #[tokio::test]
    async fn loads_draft_bundle_without_operator_trusted_assertions() {
        let directory = tempfile::tempdir().unwrap();
        write_valid_bundle(directory.path()).await;

        let manifest_path = directory.path().join("manifest.json");
        let mut manifest: ExportManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest.policy_mode = "draft".into();
        manifest.trusted_assertions_count = 0;
        let spec_text = "# Agent draft SPEC\n";
        manifest.spec_sha256 = crate::hex_lower(Sha256::digest(spec_text.as_bytes()));
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        let assertions_path = directory.path().join("assertions.json");
        let mut assertions: ExportAssertions =
            serde_json::from_slice(&fs::read(&assertions_path).unwrap()).unwrap();
        assertions.trusted_assertions.clear();
        fs::write(&assertions_path, serde_json::to_vec(&assertions).unwrap()).unwrap();
        fs::write(directory.path().join("SPEC.md"), spec_text).unwrap();

        let (_, summary) = load_and_verify(directory.path()).unwrap();
        assert_eq!(summary.trusted_assertions, 0);
        assert_eq!(summary.generated_assertions, 1);
    }

    #[tokio::test]
    async fn rejects_draft_bundle_without_embedded_spec() {
        let directory = tempfile::tempdir().unwrap();
        write_valid_bundle(directory.path()).await;

        let manifest_path = directory.path().join("manifest.json");
        let mut manifest: ExportManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest.policy_mode = "draft".into();
        manifest.trusted_assertions_count = 0;
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        let assertions_path = directory.path().join("assertions.json");
        let mut assertions: ExportAssertions =
            serde_json::from_slice(&fs::read(&assertions_path).unwrap()).unwrap();
        assertions.trusted_assertions.clear();
        fs::write(&assertions_path, serde_json::to_vec(&assertions).unwrap()).unwrap();

        let error = load_and_verify(directory.path()).err().unwrap();
        assert!(error
            .to_string()
            .contains("Draft-policy bundle must contain SPEC.md"));
    }

    #[tokio::test]
    async fn accepts_uppercase_manifest_digests() {
        let directory = tempfile::tempdir().unwrap();
        write_valid_bundle(directory.path()).await;
        let path = directory.path().join("manifest.json");
        let mut manifest: ExportManifest =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        manifest.module_hash.make_ascii_uppercase();
        manifest.component_sha256.make_ascii_uppercase();
        manifest.spec_sha256.make_ascii_uppercase();
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        let (_, summary) = load_and_verify(directory.path()).unwrap();
        assert_eq!(summary.service_name, "Bundle test");
    }

    #[tokio::test]
    async fn rejects_component_digest_mismatch_before_execution() {
        let directory = tempfile::tempdir().unwrap();
        write_valid_bundle(directory.path()).await;
        fs::write(directory.path().join("component.wasm"), b"tampered").unwrap();

        let error = load_and_verify(directory.path()).err().unwrap();
        assert!(error.to_string().contains("SHA-256"));
    }

    #[tokio::test]
    async fn rejects_assertion_count_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        write_valid_bundle(directory.path()).await;
        let path = directory.path().join("manifest.json");
        let mut manifest: ExportManifest =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        manifest.generated_assertions_count += 1;
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        let error = load_and_verify(directory.path()).err().unwrap();
        assert!(error.to_string().contains("count does not match"));
    }

    #[tokio::test]
    async fn refuses_to_serve_when_an_assertion_fails() {
        let directory = tempfile::tempdir().unwrap();
        write_valid_bundle(directory.path()).await;
        let path = directory.path().join("assertions.json");
        let mut assertions: ExportAssertions =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assertions.generated_assertions[0].expected_status = 503;
        fs::write(&path, serde_json::to_vec(&assertions).unwrap()).unwrap();

        let error = load_and_verify(directory.path()).err().unwrap();
        assert!(error.to_string().contains("verification failed"));
    }

    #[tokio::test]
    async fn rejects_symlinked_artifacts() {
        #[cfg(unix)]
        {
            let directory = tempfile::tempdir().unwrap();
            write_valid_bundle(directory.path()).await;
            let outside = tempfile::NamedTempFile::new().unwrap();
            fs::remove_file(directory.path().join("handler.js")).unwrap();
            std::os::unix::fs::symlink(outside.path(), directory.path().join("handler.js"))
                .unwrap();

            let error = load_and_verify(directory.path()).err().unwrap();
            assert!(format!("{error:#}").contains("Failed to open bundle artifact"));
        }

        #[cfg(windows)]
        {
            let directory = tempfile::tempdir().unwrap();
            write_valid_bundle(directory.path()).await;
            let outside = tempfile::NamedTempFile::new().unwrap();
            let artifact = directory.path().join("handler.js");
            fs::remove_file(&artifact).unwrap();

            // Creating file symlinks may require Developer Mode or elevated
            // privileges. If unavailable, this host cannot execute this test.
            if std::os::windows::fs::symlink_file(outside.path(), &artifact).is_ok() {
                let error = load_and_verify(directory.path()).err().unwrap();
                assert!(format!("{error:#}").contains("reparse point"));
            }
        }
    }
}
