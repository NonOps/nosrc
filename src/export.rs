use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tempfile::Builder;

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

/// Export verified artifacts as one directory publication.
///
/// The destination must not already exist. All files are written to a temporary
/// sibling directory, then a same-filesystem rename publishes the complete bundle.
pub fn export_artifacts(
    out_dir: &Path,
    spec: &ServiceSpec,
    spec_text: &str,
    output: &SynthesisOutput,
    component_bytes: &[u8],
    module_hash: &str,
    trusted_assertions: &[ContractAssertion],
) -> anyhow::Result<PathBuf> {
    let file_name = out_dir
        .file_name()
        .with_context(|| {
            format!(
                "Export destination must name a directory: {}",
                out_dir.display()
            )
        })?;
    let parent = out_dir
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "Failed to create export parent directory at {}",
            parent.display()
        )
    })?;

    match std::fs::symlink_metadata(out_dir) {
        Ok(_) => anyhow::bail!(
            "Export destination {} already exists; remove or move it before exporting",
            out_dir.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("Failed to inspect export destination {}", out_dir.display())
            });
        }
    }

    let staging = Builder::new()
        .prefix(".nosrc-export-")
        .tempdir_in(parent)
        .with_context(|| format!("Failed to create staging directory in {}", parent.display()))?;
    let staging_path = staging.path();

    // Build the complete bundle away from the published destination.
    write_artifact(staging_path, "handler.js", output.handler_code.as_bytes())?;

    let assertions_payload = ExportAssertions {
        trusted_assertions: trusted_assertions.to_vec(),
        generated_assertions: output.assertions.clone(),
    };
    let assertions_json = serde_json::to_vec_pretty(&assertions_payload)?;
    write_artifact(staging_path, "assertions.json", &assertions_json)?;
    write_artifact(staging_path, "component.wasm", component_bytes)?;

    let manifest = ExportManifest {
        service_name: spec.name.clone(),
        spec_sha256: format!("{:x}", Sha256::digest(spec_text.as_bytes())),
        module_hash: module_hash.to_string(),
        component_sha256: format!("{:x}", Sha256::digest(component_bytes)),
        trusted_assertions_count: trusted_assertions.len(),
        generated_assertions_count: output.assertions.len(),
        rationale: output.rationale.clone(),
    };
    let manifest_json = serde_json::to_vec_pretty(&manifest)?;
    write_artifact(staging_path, "manifest.json", &manifest_json)?;

    // The staging directory is a sibling, so publication stays on one filesystem.
    // Rename exposes the complete directory in one operation; existing destinations
    // are rejected rather than replaced with a non-atomic backup/rename sequence.
    let destination = parent.join(file_name);
    std::fs::rename(staging_path, &destination).with_context(|| {
        format!(
            "Failed to publish export directory at {}; destination may already exist",
            destination.display()
        )
    })?;

    Ok(out_dir.to_path_buf())
}

fn write_artifact(directory: &Path, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let path = directory.join(name);
    std::fs::write(&path, bytes).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC_TEXT: &str = r#"# Service: Test
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

    fn fixture() -> (ServiceSpec, SynthesisOutput, Vec<u8>) {
        (
            ServiceSpec::parse(SPEC_TEXT).unwrap(),
            SynthesisOutput {
                handler_code: "export function handle() {}".to_string(),
                code_format: "javascript".to_string(),
                assertions: vec![],
                rationale: Some("test export".to_string()),
            },
            b"component fixture".to_vec(),
        )
    }

    #[test]
    fn exports_all_artifacts_and_publishes_complete_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let out_dir = temp_dir.path().join("nested").join("exported");
        let (spec, output, component_bytes) = fixture();

        let path = export_artifacts(
            &out_dir, &spec, SPEC_TEXT, &output, &component_bytes, "mock_hash_123", &[],
        )
        .unwrap();

        assert_eq!(path, out_dir);
        assert_eq!(
            std::fs::read_to_string(path.join("handler.js")).unwrap(),
            output.handler_code
        );
        assert!(path.join("assertions.json").is_file());
        assert_eq!(std::fs::read(path.join("component.wasm")).unwrap(), component_bytes);
        let manifest: ExportManifest =
            serde_json::from_slice(&std::fs::read(path.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest.service_name, "Test");
        assert_eq!(manifest.module_hash, "mock_hash_123");
        assert_eq!(
            manifest.component_sha256,
            format!("{:x}", Sha256::digest(&component_bytes))
        );
    }

    #[test]
    fn refuses_existing_destination_without_modifying_it() {
        let temp_dir = tempfile::tempdir().unwrap();
        let out_dir = temp_dir.path().join("existing");
        std::fs::create_dir(&out_dir).unwrap();
        std::fs::write(out_dir.join("keep.txt"), "original").unwrap();
        let (spec, output, component_bytes) = fixture();

        let error = export_artifacts(
            &out_dir, &spec, SPEC_TEXT, &output, &component_bytes, "mock_hash_123", &[],
        )
        .unwrap_err();

        assert!(error.to_string().contains("already exists"));
        assert_eq!(
            std::fs::read_to_string(out_dir.join("keep.txt")).unwrap(),
            "original"
        );
        assert!(!out_dir.join("handler.js").exists());
    }

    #[test]
    fn refuses_existing_file_destination() {
        let temp_dir = tempfile::tempdir().unwrap();
        let out_file = temp_dir.path().join("exported");
        std::fs::write(&out_file, "keep").unwrap();
        let (spec, output, component_bytes) = fixture();

        assert!(export_artifacts(
            &out_file, &spec, SPEC_TEXT, &output, &component_bytes, "mock_hash_123", &[],
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(out_file).unwrap(), "keep");
    }
}
