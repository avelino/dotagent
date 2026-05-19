//! Cache of known manifest hashes.
//!
//! Path: `~/.local/state/dotagent/known_manifests.json`.
//!
//! Used by the daemon to detect:
//! - **Manifest drift**: agent loaded today has different sha256 than the
//!   cached one for the same name.
//! - **Phantom agents**: agent name appears that the cache has never seen.
//!
//! The cache is best-effort: an attacker with rwx on
//! `~/.local/state/dotagent/` can rewrite both the cache and the manifest
//! in lockstep. Detection becomes possible only via the audit log
//! (see `docs/security/threat-model.md`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::StateError;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KnownManifests {
    /// `agent_name → entry`.
    #[serde(default)]
    pub entries: BTreeMap<String, KnownManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownManifest {
    pub path: PathBuf,
    pub sha256: String,
    pub first_seen_at_iso: String,
    pub last_seen_at_iso: String,
}

#[derive(Debug, Clone)]
pub struct ManifestCache {
    path: PathBuf,
}

impl ManifestCache {
    pub fn from_home() -> Result<Self, StateError> {
        Ok(Self::with_path(crate::paths::known_manifests_file()))
    }

    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<KnownManifests, StateError> {
        if !self.path.exists() {
            return Ok(KnownManifests::default());
        }
        let f = std::fs::File::open(&self.path)?;
        Ok(serde_json::from_reader(f)?)
    }

    pub fn save(&self, value: &KnownManifests) -> Result<(), StateError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Compute sha256 of a manifest file's bytes (lowercase hex).
pub fn hash_manifest_file(path: &Path) -> Result<String, StateError> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(hex(&h.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(H[(b >> 4) as usize] as char);
        out.push(H[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn cache_roundtrip() {
        let dir = tempdir().unwrap();
        let cache = ManifestCache::with_path(dir.path().join("known.json"));
        let mut k = KnownManifests::default();
        k.entries.insert(
            "x".into(),
            KnownManifest {
                path: PathBuf::from("/tmp/x/agent.toml"),
                sha256: "abc".into(),
                first_seen_at_iso: "2026-01-01T00:00:00+0000".into(),
                last_seen_at_iso: "2026-01-01T00:00:00+0000".into(),
            },
        );
        cache.save(&k).unwrap();
        let loaded = cache.load().unwrap();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries.get("x").unwrap().sha256, "abc");
    }

    #[test]
    fn hash_manifest_file_is_stable() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a.toml");
        std::fs::write(&p, "hello").unwrap();
        let h1 = hash_manifest_file(&p).unwrap();
        let h2 = hash_manifest_file(&p).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }
}
