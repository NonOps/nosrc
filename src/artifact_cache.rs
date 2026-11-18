use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::synthesizer::SynthesisOutput;

const CACHE_SCHEMA_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct CacheManifest {
    schema_version: u32,
    cache_key: String,
    component_sha256: String,
    synthesis_sha256: String,
    synthesis: SynthesisOutput,
}

#[derive(Debug)]
pub struct CachedArtifact {
    pub synthesis: SynthesisOutput,
    pub component_bytes: Vec<u8>,
}

/// A best-effort, content-addressed cache. Cache entries are never trusted as
/// verification results: callers must rerun both verification suites.
#[derive(Clone)]
pub struct ArtifactCache {
    root: PathBuf,
}

impl ArtifactCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn load(&self, key: &str) -> Option<CachedArtifact> {
        if !valid_key(key) {
            tracing::warn!("Ignoring artifact cache lookup with an invalid key");
            return None;
        }

        let directory = self.root.join(key);
        let directory_metadata = fs::symlink_metadata(&directory).ok()?;
        if !directory_metadata.file_type().is_dir() {
            tracing::warn!(cache_key = %key, "Ignoring non-directory artifact cache entry");
            return None;
        }

        let manifest_path = directory.join("manifest.json");
        let component_path = directory.join("component.wasm");
        let manifest_metadata = fs::symlink_metadata(&manifest_path).ok()?;
        let component_metadata = fs::symlink_metadata(&component_path).ok()?;

        if !manifest_metadata.file_type().is_file()
            || !component_metadata.file_type().is_file()
            || manifest_metadata.len() > MAX_MANIFEST_BYTES
            || component_metadata.len() == 0
            || component_metadata.len() > MAX_COMPONENT_BYTES
        {
            tracing::warn!(cache_key = %key, "Ignoring malformed or oversized artifact cache entry");
            return None;
        }

        let manifest_bytes = fs::read(&manifest_path).ok()?;
        let manifest: CacheManifest = match serde_json::from_slice(&manifest_bytes) {
            Ok(manifest) => manifest,
            Err(error) => {
                tracing::warn!(cache_key = %key, %error, "Ignoring unreadable artifact cache manifest");
                return None;
            }
        };

        if manifest.schema_version != CACHE_SCHEMA_VERSION || manifest.cache_key != key {
            tracing::warn!(cache_key = %key, "Ignoring artifact cache entry with incompatible metadata");
            return None;
        }

        let component_bytes = fs::read(&component_path).ok()?;
        let digest = format!("{:x}", Sha256::digest(&component_bytes));
        if digest != manifest.component_sha256 {
            tracing::warn!(cache_key = %key, "Ignoring artifact cache entry with a component digest mismatch");
            return None;
        }

        let synthesis_bytes = match serde_json::to_vec(&manifest.synthesis) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(cache_key = %key, %error, "Ignoring artifact cache entry with invalid synthesis metadata");
                return None;
            }
        };
        let synthesis_digest = format!("{:x}", Sha256::digest(&synthesis_bytes));
        if synthesis_digest != manifest.synthesis_sha256 {
            tracing::warn!(cache_key = %key, "Ignoring artifact cache entry with a synthesis metadata digest mismatch");
            return None;
        }

        if manifest.synthesis.code_format != "javascript"
            || manifest.synthesis.handler_code.trim().is_empty()
        {
            tracing::warn!(cache_key = %key, "Ignoring artifact cache entry with invalid synthesis metadata");
            return None;
        }

        Some(CachedArtifact {
            synthesis: manifest.synthesis,
            component_bytes,
        })
    }

    pub fn store(
        &self,
        key: &str,
        synthesis: &SynthesisOutput,
        component_bytes: &[u8],
    ) -> anyhow::Result<()> {
        anyhow::ensure!(valid_key(key), "invalid artifact cache key");
        anyhow::ensure!(
            !component_bytes.is_empty() && component_bytes.len() as u64 <= MAX_COMPONENT_BYTES,
            "component artifact is empty or exceeds the cache size limit"
        );

        let directory = self.root.join(key);
        fs::create_dir_all(&directory)?;
        let metadata = fs::symlink_metadata(&directory)?;
        anyhow::ensure!(
            metadata.file_type().is_dir(),
            "artifact cache entry path is not a directory"
        );

        let manifest = CacheManifest {
            schema_version: CACHE_SCHEMA_VERSION,
            cache_key: key.to_string(),
            component_sha256: format!("{:x}", Sha256::digest(component_bytes)),
            synthesis_sha256: format!("{:x}", Sha256::digest(serde_json::to_vec(synthesis)?)),
            synthesis: synthesis.clone(),
        };
        let manifest_bytes = serde_json::to_vec(&manifest)?;
        anyhow::ensure!(
            manifest_bytes.len() as u64 <= MAX_MANIFEST_BYTES,
            "artifact cache manifest exceeds the size limit"
        );

        // Publish the component first and the manifest last. A manifest is the
        // commit marker, so an interrupted write is treated as a cache miss.
        atomic_write(&directory.join("component.wasm"), component_bytes)?;
        atomic_write(&directory.join("manifest.json"), &manifest_bytes)?;
        Ok(())
    }
}

fn valid_key(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("cache file has no parent directory"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| anyhow::anyhow!("failed to publish cache file: {}", error.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthesis() -> SynthesisOutput {
        SynthesisOutput {
            handler_code: "export function handle() { return '{}'; }".into(),
            code_format: "javascript".into(),
            assertions: Vec::new(),
            rationale: Some("test".into()),
        }
    }

    #[test]
    fn cache_round_trip_preserves_synthesis_and_component() {
        let directory = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(directory.path());
        let key = "a".repeat(64);
        let component = b"fake wasm bytes";

        cache.store(&key, &synthesis(), component).unwrap();
        let loaded = cache.load(&key).unwrap();

        assert_eq!(loaded.synthesis.handler_code, synthesis().handler_code);
        assert_eq!(loaded.component_bytes.as_slice(), component);
    }

    #[test]
    fn cache_rejects_component_digest_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(directory.path());
        let key = "b".repeat(64);
        cache.store(&key, &synthesis(), b"original").unwrap();

        fs::write(
            directory.path().join(&key).join("component.wasm"),
            b"tampered",
        )
        .unwrap();

        assert!(cache.load(&key).is_none());
    }

    #[test]
    fn cache_rejects_modified_synthesis_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(directory.path());
        let key = "c".repeat(64);
        cache.store(&key, &synthesis(), b"component").unwrap();

        let manifest_path = directory.path().join(&key).join("manifest.json");
        let manifest = fs::read_to_string(&manifest_path).unwrap();
        fs::write(&manifest_path, manifest.replace("\"test\"", "\"tampered\"")).unwrap();

        assert!(cache.load(&key).is_none());
    }

    #[test]
    fn cache_rejects_invalid_keys() {
        let directory = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(directory.path());
        assert!(cache.load("../escape").is_none());
        assert!(cache
            .store("../escape", &synthesis(), b"component")
            .is_err());
    }
}
