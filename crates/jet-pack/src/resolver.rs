use std::{collections::HashMap, fs, path::Path};

use redis::Commands;
use semver::Version;

use crate::{
    error::{JetPackError, JetPackResult},
    manifest::{RuntimeManifest, parse_manifest_file},
};

pub trait VersionStore {
    fn set_many(&mut self, entries: HashMap<String, String>) -> JetPackResult<()>;
    fn get(&self, key: &str) -> JetPackResult<Option<String>>;
}

#[derive(Debug, Default, Clone)]
pub struct InMemoryVersionStore {
    map: HashMap<String, String>,
}

impl VersionStore for InMemoryVersionStore {
    fn set_many(&mut self, entries: HashMap<String, String>) -> JetPackResult<()> {
        self.map = entries;
        Ok(())
    }

    fn get(&self, key: &str) -> JetPackResult<Option<String>> {
        Ok(self.map.get(key).cloned())
    }
}

#[derive(Debug, Clone)]
pub struct RedisVersionStore {
    client: redis::Client,
    redis_key: String,
}

impl RedisVersionStore {
    pub fn new(redis_url: &str, redis_key: impl Into<String>) -> JetPackResult<Self> {
        let client = redis::Client::open(redis_url).map_err(|source| JetPackError::Redis {
            operation: "client_open".to_string(),
            source,
        })?;

        Ok(Self {
            client,
            redis_key: redis_key.into(),
        })
    }
}

impl VersionStore for RedisVersionStore {
    fn set_many(&mut self, entries: HashMap<String, String>) -> JetPackResult<()> {
        let mut conn = self
            .client
            .get_connection()
            .map_err(|source| JetPackError::Redis {
                operation: "connect".to_string(),
                source,
            })?;

        let _: usize = conn
            .del(&self.redis_key)
            .map_err(|source| JetPackError::Redis {
                operation: "del".to_string(),
                source,
            })?;

        for (key, value) in entries {
            let _: usize = conn
                .hset(&self.redis_key, key, value)
                .map_err(|source| JetPackError::Redis {
                    operation: "hset".to_string(),
                    source,
                })?;
        }

        Ok(())
    }

    fn get(&self, key: &str) -> JetPackResult<Option<String>> {
        let mut conn = self
            .client
            .get_connection()
            .map_err(|source| JetPackError::Redis {
                operation: "connect".to_string(),
                source,
            })?;

        let value = conn
            .hget(&self.redis_key, key)
            .map_err(|source| JetPackError::Redis {
                operation: "hget".to_string(),
                source,
            })?;

        Ok(value)
    }
}

#[derive(Debug, Clone)]
pub struct VersionResolver<S: VersionStore> {
    store: S,
}

impl<S: VersionStore> VersionResolver<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    pub fn initialize_from_manifests(&mut self, manifests: &[RuntimeManifest]) -> JetPackResult<()> {
        let map = build_version_map(manifests)?;
        self.store.set_many(map)?;
        Ok(())
    }

    pub fn initialize_from_manifest_dir(&mut self, dir: &Path) -> JetPackResult<Vec<RuntimeManifest>> {
        let manifests = scan_manifest_dir(dir)?;
        self.initialize_from_manifests(&manifests)?;
        Ok(manifests)
    }

    pub fn resolve(&self, language: &str, requested: &str) -> JetPackResult<Option<String>> {
        self.store.get(&format!("{}:{}", language, requested))
    }
}

pub fn scan_manifest_dir(dir: &Path) -> JetPackResult<Vec<RuntimeManifest>> {
    let mut manifests = Vec::new();
    if !dir.exists() {
        return Ok(manifests);
    }

    for entry in fs::read_dir(dir).map_err(|source| JetPackError::Io {
        path: dir.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| JetPackError::Io {
            path: dir.to_path_buf(),
            source,
        })?;

        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let Some(ext) = path.extension().and_then(|v| v.to_str()) else {
            continue;
        };

        if !matches!(ext, "yaml" | "yml") {
            continue;
        }

        manifests.push(parse_manifest_file(&path)?);
    }

    Ok(manifests)
}

pub fn build_version_map(manifests: &[RuntimeManifest]) -> JetPackResult<HashMap<String, String>> {
    let mut index: HashMap<String, Vec<(Version, String)>> = HashMap::new();

    for manifest in manifests {
        let version = Version::parse(&manifest.version).map_err(|_| JetPackError::InvalidVersion {
            value: manifest.version.clone(),
        })?;
        index
            .entry(manifest.language.clone())
            .or_default()
            .push((version, manifest.version.clone()));
    }

    for versions in index.values_mut() {
        versions.sort_by(|(a, _), (b, _)| b.cmp(a));
    }

    let mut map = HashMap::new();

    for (language, versions) in &index {
        for (_, full) in versions {
            let parsed = Version::parse(full).map_err(|_| JetPackError::InvalidVersion {
                value: full.clone(),
            })?;

            let fragments = [
                parsed.major.to_string(),
                format!("{}.{}", parsed.major, parsed.minor),
                format!("{}.{}.{}", parsed.major, parsed.minor, parsed.patch),
            ];

            for fragment in fragments {
                let key = format!("{}:{}", language, fragment);
                map.entry(key).or_insert_with(|| full.clone());
            }

            map.entry(format!("{}:{}", language, full))
                .or_insert_with(|| full.clone());
        }
    }

    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ExecutionTemplate, RuntimeArchive};
    use std::{collections::HashMap, fs};
    use tempfile::tempdir;

    fn manifest(language: &str, version: &str) -> RuntimeManifest {
        let mut runtimes = HashMap::new();
        runtimes.insert(
            "x86_64".to_string(),
            RuntimeArchive {
                url: "file:///tmp/runtime.tar.gz".to_string(),
                sha256: None,
            },
        );

        RuntimeManifest {
            language: language.to_string(),
            version: version.to_string(),
            aliases: vec![],
            runtimes,
            execute: ExecutionTemplate {
                command: "run".to_string(),
                args: None,
            },
            compile: None,
        }
    }

    #[test]
    fn builds_version_map_resolving_to_latest_patch() {
        let manifests = vec![manifest("python", "3.14.3"), manifest("python", "3.14.2")];
        let map = build_version_map(&manifests).expect("map should build");

        assert_eq!(map.get("python:3").cloned(), Some("3.14.3".to_string()));
        assert_eq!(map.get("python:3.14").cloned(), Some("3.14.3".to_string()));
        assert_eq!(map.get("python:3.14.2").cloned(), Some("3.14.2".to_string()));
        assert_eq!(map.get("python:3.14.3").cloned(), Some("3.14.3".to_string()));
    }

    #[test]
    fn fails_for_invalid_semver_manifest_version() {
        let manifests = vec![manifest("python", "3.14")];
        let err = build_version_map(&manifests);
        assert!(matches!(err, Err(JetPackError::InvalidVersion { .. })));
    }

    #[test]
    fn scans_manifest_directory_for_yaml_files() {
        let dir = tempdir().expect("temp dir should exist");
        let path = dir.path().join("python.yaml");
        fs::write(
            &path,
            r#"
language: python
version: 3.14.3
runtimes:
  x86_64:
    url: file:///tmp/runtime.tar.gz
    sha256: null
execute:
  command: python
  args: ["main.py"]
compile: null
"#,
        )
        .expect("manifest file should be written");

        fs::write(dir.path().join("ignore.txt"), "noop").expect("text file should be written");

        let manifests = scan_manifest_dir(dir.path()).expect("scan should pass");
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].language, "python");
    }

    #[test]
    fn resolver_returns_none_for_missing_key() {
        let mut resolver = VersionResolver::new(InMemoryVersionStore::default());
        resolver
            .initialize_from_manifests(&[manifest("rust", "1.87.0")])
            .expect("resolver init should work");

        assert_eq!(
            resolver
                .resolve("rust", "1.86")
                .expect("resolve should succeed"),
            None
        );
    }
}
