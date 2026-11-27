use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::synthesizer::SynthesisOutput;

const CACHE_SCHEMA_VERSION: u32 = 3;
const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct CacheManifest {
    schema_version: u32,
    synthesis_key: String,
    component_cache_key: String,
    component_sha256: String,
    synthesis_sha256: String,
    synthesis: SynthesisOutput,
}

#[derive(Debug)]
pub struct CachedArtifact {
    pub synthesis: SynthesisOutput,
    pub component_bytes: Vec<u8>,
    pub component_cache_key: String,
}

/// A best-effort, content-addressed cache with independent synthesis and component
/// identities. Cache entries are never trusted as verification results: callers must
/// rerun both verification suites before activation or export.
#[derive(Clone)]
pub struct ArtifactCache {
    root: PathBuf,
}

impl ArtifactCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn load(&self, synthesis_key: &str) -> Option<CachedArtifact> {
        if !valid_key(synthesis_key) {
            tracing::warn!("Ignoring artifact cache lookup with an invalid synthesis key");
            return None;
        }

        let root_metadata = fs::symlink_metadata(&self.root).ok()?;
        if !root_metadata.file_type().is_dir() {
            tracing::warn!("Ignoring artifact cache root that is not a real directory");
            return None;
        }

        let directory = self.root.join(synthesis_key);
        let directory_metadata = fs::symlink_metadata(&directory).ok()?;
        if !directory_metadata.file_type().is_dir() {
            tracing::warn!(cache_key = %synthesis_key, "Ignoring non-directory artifact cache entry");
            return None;
        }

        let manifest_path = directory.join("manifest.json");
        let component_path = directory.join("component.wasm");
        let manifest_bytes = match read_cache_artifact(&manifest_path, MAX_MANIFEST_BYTES, false) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(cache_key = %synthesis_key, %error, "Ignoring malformed or oversized artifact cache manifest");
                return None;
            }
        };
        let manifest: CacheManifest = match serde_json::from_slice(&manifest_bytes) {
            Ok(manifest) => manifest,
            Err(error) => {
                tracing::warn!(cache_key = %synthesis_key, %error, "Ignoring unreadable artifact cache manifest");
                return None;
            }
        };

        if manifest.schema_version != CACHE_SCHEMA_VERSION
            || manifest.synthesis_key != synthesis_key
            || !valid_key(&manifest.component_cache_key)
        {
            tracing::warn!(cache_key = %synthesis_key, "Ignoring artifact cache entry with incompatible metadata");
            return None;
        }

        let component_bytes = match read_cache_artifact(&component_path, MAX_COMPONENT_BYTES, true)
        {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(cache_key = %synthesis_key, %error, "Ignoring malformed or oversized artifact cache component");
                return None;
            }
        };
        let component_digest = crate::hex_lower(Sha256::digest(&component_bytes));
        if component_digest != manifest.component_sha256 {
            tracing::warn!(cache_key = %synthesis_key, "Ignoring artifact cache entry with a component digest mismatch");
            return None;
        }

        let synthesis_bytes = match serde_json::to_vec(&manifest.synthesis) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(cache_key = %synthesis_key, %error, "Ignoring artifact cache entry with invalid synthesis metadata");
                return None;
            }
        };
        let synthesis_digest = crate::hex_lower(Sha256::digest(&synthesis_bytes));
        if synthesis_digest != manifest.synthesis_sha256 {
            tracing::warn!(cache_key = %synthesis_key, "Ignoring artifact cache entry with a synthesis metadata digest mismatch");
            return None;
        }

        if manifest.synthesis.code_format != "javascript"
            || manifest.synthesis.handler_code.trim().is_empty()
        {
            tracing::warn!(cache_key = %synthesis_key, "Ignoring artifact cache entry with invalid synthesis metadata");
            return None;
        }

        Some(CachedArtifact {
            synthesis: manifest.synthesis,
            component_bytes,
            component_cache_key: manifest.component_cache_key,
        })
    }

    /// Compatibility helper for tests and internal callers without a current component identity.
    /// Such entries are synthesis-only cache hits.
    pub fn store(
        &self,
        synthesis_key: &str,
        synthesis: &SynthesisOutput,
        component_bytes: &[u8],
    ) -> anyhow::Result<()> {
        self.store_with_component_key(synthesis_key, &"0".repeat(64), synthesis, component_bytes)
    }

    pub fn store_with_component_key(
        &self,
        synthesis_key: &str,
        component_cache_key: &str,
        synthesis: &SynthesisOutput,
        component_bytes: &[u8],
    ) -> anyhow::Result<()> {
        anyhow::ensure!(valid_key(synthesis_key), "invalid synthesis cache key");
        anyhow::ensure!(
            valid_key(component_cache_key),
            "invalid component cache key"
        );
        anyhow::ensure!(
            !component_bytes.is_empty() && component_bytes.len() as u64 <= MAX_COMPONENT_BYTES,
            "component artifact is empty or exceeds the cache size limit"
        );

        fs::create_dir_all(&self.root)?;
        let root_metadata = fs::symlink_metadata(&self.root)?;
        anyhow::ensure!(
            root_metadata.file_type().is_dir(),
            "artifact cache root is not a real directory"
        );

        let directory = self.root.join(synthesis_key);
        fs::create_dir_all(&directory)?;
        let metadata = fs::symlink_metadata(&directory)?;
        anyhow::ensure!(
            metadata.file_type().is_dir(),
            "artifact cache entry path is not a directory"
        );

        let synthesis_bytes = serde_json::to_vec(synthesis)?;
        let manifest = CacheManifest {
            schema_version: CACHE_SCHEMA_VERSION,
            synthesis_key: synthesis_key.to_string(),
            component_cache_key: component_cache_key.to_string(),
            component_sha256: crate::hex_lower(Sha256::digest(component_bytes)),
            synthesis_sha256: crate::hex_lower(Sha256::digest(&synthesis_bytes)),
            synthesis: synthesis.clone(),
        };
        let manifest_bytes = serde_json::to_vec(&manifest)?;
        anyhow::ensure!(
            manifest_bytes.len() as u64 <= MAX_MANIFEST_BYTES,
            "artifact cache manifest exceeds the size limit"
        );

        // Publish component bytes first and the manifest last. The manifest is the
        // commit marker, so interrupted writes are treated as cache misses.
        atomic_write(&directory.join("component.wasm"), component_bytes)?;
        atomic_write(&directory.join("manifest.json"), &manifest_bytes)?;
        Ok(())
    }
}

fn valid_key(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Open a cache artifact without following a final-component symlink, then
/// validate and bound reads against the opened file descriptor (not its path).
fn read_cache_artifact(path: &Path, limit: u64, allow_empty: bool) -> std::io::Result<Vec<u8>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT opens the reparse point itself instead
        // of following it to its target.
        options.custom_flags(0x0020_0000);
    }

    let file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "cache artifact is a reparse point",
            ));
        }
    }
    if !metadata.is_file() || metadata.len() > limit || (!allow_empty && metadata.len() == 0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cache artifact is not a regular file or violates its size bound",
        ));
    }

    let mut bytes = Vec::with_capacity(metadata.len().min(limit) as usize);
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit || (!allow_empty && bytes.is_empty()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cache artifact exceeds its size bound or is empty",
        ));
    }
    Ok(bytes)
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
    fn cache_round_trip_preserves_both_identities_and_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(directory.path());
        let synthesis_key = "a".repeat(64);
        let component_key = "b".repeat(64);
        let component = b"fake wasm bytes";

        cache
            .store_with_component_key(&synthesis_key, &component_key, &synthesis(), component)
            .unwrap();
        let loaded = cache.load(&synthesis_key).unwrap();

        assert_eq!(loaded.synthesis.handler_code, synthesis().handler_code);
        assert_eq!(loaded.component_bytes.as_slice(), component);
        assert_eq!(loaded.component_cache_key, component_key);
    }

    #[test]
    fn cache_reuses_synthesis_when_component_toolchain_identity_changes() {
        let directory = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(directory.path());
        let synthesis_key = "c".repeat(64);
        let old_component_key = "d".repeat(64);
        let new_component_key = "e".repeat(64);

        cache
            .store_with_component_key(
                &synthesis_key,
                &old_component_key,
                &synthesis(),
                b"component-v1",
            )
            .unwrap();
        let loaded = cache.load(&synthesis_key).unwrap();

        assert_eq!(loaded.synthesis.handler_code, synthesis().handler_code);
        assert_ne!(loaded.component_cache_key, new_component_key);
    }

    #[test]
    fn cache_rejects_component_digest_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(directory.path());
        let key = "f".repeat(64);
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
        let key = "1".repeat(64);
        cache.store(&key, &synthesis(), b"component").unwrap();

        let manifest_path = directory.path().join(&key).join("manifest.json");
        let manifest = fs::read_to_string(&manifest_path).unwrap();
        fs::write(&manifest_path, manifest.replace("\"test\"", "\"tampered\"")).unwrap();
        assert!(cache.load(&key).is_none());
    }

    #[test]
    fn cache_rejects_legacy_schema_as_a_safe_miss() {
        let directory = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(directory.path());
        let key = "2".repeat(64);
        let entry = directory.path().join(&key);
        fs::create_dir_all(&entry).unwrap();
        fs::write(entry.join("component.wasm"), b"old component").unwrap();
        fs::write(
            entry.join("manifest.json"),
            r#"{"schema_version":1,"cache_key":"2222222222222222222222222222222222222222222222222222222222222222"}"#,
        )
        .unwrap();
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

    #[test]
    fn cache_rejects_oversized_manifest_and_component_files() {
        let directory = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(directory.path());
        let manifest_key = "3".repeat(64);
        cache
            .store(&manifest_key, &synthesis(), b"component")
            .unwrap();
        let manifest = directory.path().join(&manifest_key).join("manifest.json");
        fs::OpenOptions::new()
            .write(true)
            .open(&manifest)
            .unwrap()
            .set_len(MAX_MANIFEST_BYTES + 1)
            .unwrap();
        assert!(cache.load(&manifest_key).is_none());

        let component_key = "4".repeat(64);
        cache
            .store(&component_key, &synthesis(), b"component")
            .unwrap();
        let component = directory.path().join(&component_key).join("component.wasm");
        fs::OpenOptions::new()
            .write(true)
            .open(&component)
            .unwrap()
            .set_len(MAX_COMPONENT_BYTES + 1)
            .unwrap();
        assert!(cache.load(&component_key).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cache_rejects_symlinked_root_for_reads_and_writes() {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir().unwrap();
        let real_cache = ArtifactCache::new(outside.path());
        let key = "5".repeat(64);
        real_cache.store(&key, &synthesis(), b"component").unwrap();

        let parent = tempfile::tempdir().unwrap();
        let linked_root = parent.path().join("cache");
        symlink(outside.path(), &linked_root).unwrap();
        let linked_cache = ArtifactCache::new(&linked_root);

        assert!(linked_cache.load(&key).is_none());
        assert!(linked_cache
            .store(&"6".repeat(64), &synthesis(), b"component")
            .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cache_rejects_symlinked_manifest_and_component_files() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_manifest = outside.path().join("manifest.json");
        let outside_component = outside.path().join("component.wasm");
        fs::write(&outside_manifest, b"{}").unwrap();
        fs::write(&outside_component, b"component").unwrap();

        let cache = ArtifactCache::new(directory.path());
        let manifest_key = "7".repeat(64);
        cache
            .store(&manifest_key, &synthesis(), b"component")
            .unwrap();
        let manifest = directory.path().join(&manifest_key).join("manifest.json");
        fs::remove_file(&manifest).unwrap();
        symlink(&outside_manifest, &manifest).unwrap();
        assert!(cache.load(&manifest_key).is_none());

        // Replacing a symlinked cache artifact must not write through it.
        cache
            .store(&manifest_key, &synthesis(), b"component")
            .unwrap();
        assert_eq!(fs::read(&outside_manifest).unwrap(), b"{}");
        assert!(fs::symlink_metadata(&manifest)
            .unwrap()
            .file_type()
            .is_file());

        let component_key = "8".repeat(64);
        cache
            .store(&component_key, &synthesis(), b"component")
            .unwrap();
        let component = directory.path().join(&component_key).join("component.wasm");
        fs::remove_file(&component).unwrap();
        symlink(&outside_component, &component).unwrap();
        assert!(cache.load(&component_key).is_none());

        // A cache refresh must replace the link itself, leaving the external
        // target untouched and restoring a regular in-root artifact.
        cache
            .store(&component_key, &synthesis(), b"replacement component")
            .unwrap();
        assert_eq!(fs::read(&outside_component).unwrap(), b"component");
        assert!(fs::symlink_metadata(&component)
            .unwrap()
            .file_type()
            .is_file());
    }
}
