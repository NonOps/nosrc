use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use crate::spec::ServiceSpec;
use crate::synthesizer::{ContractAssertion, SynthesisOutput};

#[derive(Debug, Serialize, Deserialize)]
pub struct ExportManifest {
    pub service_name: String,
    pub spec_sha256: String,
    pub module_hash: String,
    pub component_sha256: String,
    pub trusted_assertions_count: usize,
    pub generated_assertions_count: usize,
    pub rationale: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExportAssertions {
    pub trusted_assertions: Vec<ContractAssertion>,
    pub generated_assertions: Vec<ContractAssertion>,
}

/// Export verified artifacts to disk for code review, audit trail, and version control.
pub fn export_artifacts(
    out_dir: &Path,
    spec: &ServiceSpec,
    spec_text: &str,
    output: &SynthesisOutput,
    component_bytes: &[u8],
    module_hash: &str,
    trusted_assertions: &[ContractAssertion],
) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("Failed to create export directory at {}", out_dir.display()))?;

    // 1. Export verified JavaScript handler
    let handler_path = out_dir.join("handler.js");
    std::fs::write(&handler_path, &output.handler_code)
        .with_context(|| format!("Failed to write {}", handler_path.display()))?;

    // 2. Export contract assertions (trusted + generated)
    let assertions_payload = ExportAssertions {
        trusted_assertions: trusted_assertions.to_vec(),
        generated_assertions: output.assertions.clone(),
    };
    let assertions_json = serde_json::to_string_pretty(&assertions_payload)?;
    let assertions_path = out_dir.join("assertions.json");
    std::fs::write(&assertions_path, assertions_json)
        .with_context(|| format!("Failed to write {}", assertions_path.display()))?;

    // 3. Export compiled Wasm component
    let component_path = out_dir.join("component.wasm");
    std::fs::write(&component_path, component_bytes)
        .with_context(|| format!("Failed to write {}", component_path.display()))?;

    // 4. Export manifest metadata
    let spec_sha256 = format!("{:x}", Sha256::digest(spec_text.as_bytes()));
    let component_sha256 = format!("{:x}", Sha256::digest(component_bytes));
    let manifest = ExportManifest {
        service_name: spec.name.clone(),
        spec_sha256,
        module_hash: module_hash.to_string(),
        component_sha256,
        trusted_assertions_count: trusted_assertions.len(),
        generated_assertions_count: output.assertions.len(),
        rationale: output.rationale.clone(),
    };
    let manifest_json = serde_json::to_string_pretty(&manifest)?;
    let manifest_path = out_dir.join("manifest.json");
    std::fs::write(&manifest_path, manifest_json)
        .with_context(|| format!("Failed to write {}", manifest_path.display()))?;

    Ok(out_dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_all_artifacts_to_target_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let out_dir = temp_dir.path().join("exported");

        let spec_text = r#"# Service: Test
## Capabilities
- state: volatile
- egress: []

## Endpoints
### `GET /test`
- Rules: Return ok
- Response `200`: text/plain

## Verification
- case: test
  method: GET
  path: /test
  response:
    status: 200
"#;
        let spec = ServiceSpec::parse(spec_text).unwrap();
        let output = SynthesisOutput {
            handler_code: "export function handle() {}".to_string(),
            code_format: "javascript".to_string(),
            assertions: vec![],
            rationale: Some("test export".to_string()),
        };
        let component_bytes = b"\0asm\x01\0\0\0";
        let module_hash = "mock_hash_123";

        let path = export_artifacts(
            &out_dir,
            &spec,
            spec_text,
            &output,
            component_bytes,
            module_hash,
            &[],
        )
        .unwrap();

        assert!(path.join("handler.js").is_file());
        assert_eq!(
            std::fs::read_to_string(path.join("handler.js")).unwrap(),
            output.handler_code
        );
        assert!(path.join("assertions.json").is_file());
        assert!(path.join("component.wasm").is_file());
        assert!(path.join("manifest.json").is_file());

        let manifest: ExportManifest =
            serde_json::from_str(&std::fs::read_to_string(path.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest.service_name, "Test");
        assert_eq!(manifest.module_hash, module_hash);
    }
}
