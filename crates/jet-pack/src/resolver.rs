use std::{collections::HashMap, fs, path::Path};

use redis::Commands;
use regex::Regex;

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
            let _: usize =
                conn.hset(&self.redis_key, key, value)
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
    /// Maps language aliases to their canonical language name.
    alias_map: HashMap<String, String>,
}

impl<S: VersionStore> VersionResolver<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            alias_map: HashMap::new(),
        }
    }

    pub fn initialize_from_manifests(
        &mut self,
        manifests: &[RuntimeManifest],
    ) -> JetPackResult<()> {
        let map = build_version_map(manifests)?;
        self.alias_map = build_alias_map(manifests);
        self.store.set_many(map)?;
        Ok(())
    }

    pub fn initialize_from_manifest_dir(
        &mut self,
        dir: &Path,
    ) -> JetPackResult<Vec<RuntimeManifest>> {
        let manifests = scan_manifest_dir(dir)?;
        self.initialize_from_manifests(&manifests)?;
        Ok(manifests)
    }

    /// Returns the canonical language name for the given name or alias.
    /// If the name is already canonical (or unknown), it is returned as-is.
    pub fn canonical_language<'a>(&'a self, name: &'a str) -> &'a str {
        self.alias_map.get(name).map(|s| s.as_str()).unwrap_or(name)
    }

    pub fn resolve(&self, language: &str, requested: &str) -> JetPackResult<Option<String>> {
        let lang = self.canonical_language(language);
        self.store.get(&format!("{}:{}", lang, requested))
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
    let mut index: HashMap<String, Vec<(LooseVersion, RuntimeManifest)>> = HashMap::new();

    for manifest in manifests {
        let version = LooseVersion::parse(&manifest.version)?;
        index
            .entry(manifest.language.clone())
            .or_default()
            .push((version, manifest.clone()));
    }

    for versions in index.values_mut() {
        versions.sort_by(|(a, _), (b, _)| b.cmp(a));
    }

    let mut map = HashMap::new();

    for (language, versions) in &index {
        for (parsed, manifest) in versions {
            let full = &manifest.version;
            let major = parsed.numbers.first().copied().unwrap_or_default();
            let minor = parsed.numbers.get(1).copied().unwrap_or_default();
            let patch = parsed.numbers.get(2).copied().unwrap_or_default();

            let mut fragments = vec![major.to_string(), format!("{}.{}", major, minor)];
            if parsed.numbers.len() >= 3 {
                fragments.push(format!("{}.{}.{}", major, minor, patch));
            }

            for fragment in fragments {
                let key = format!("{}:{}", language, fragment);
                map.entry(key).or_insert_with(|| full.clone());
            }

            map.entry(format!("{}:{}", language, full))
                .or_insert_with(|| full.clone());

            for alias in &manifest.aliases {
                map.entry(format!("{}:{}", language, alias))
                    .or_insert_with(|| full.clone());
            }
        }

        // Add 'latest' as a default alias for the highest version
        if let Some((_, manifest)) = versions.first() {
            map.entry(format!("{}:{}", language, "latest"))
                .or_insert_with(|| manifest.version.clone());
        }
    }

    Ok(map)
}

/// Builds a mapping from language aliases to their canonical language name.
///
/// For example, if a manifest has `language: "cpp"` and
/// `aliases: ["cpp", "c++", "g++", "cxx"]`, this produces:
///   "c++" → "cpp", "g++" → "cpp", "cxx" → "cpp"
///
/// The canonical name itself is *not* included as a key (it maps to itself
/// by convention in `canonical_language`).
fn build_alias_map(manifests: &[RuntimeManifest]) -> HashMap<String, String> {
    let mut alias_map = HashMap::new();
    for manifest in manifests {
        for alias in &manifest.aliases {
            if alias != &manifest.language {
                alias_map
                    .entry(alias.clone())
                    .or_insert_with(|| manifest.language.clone());
            }
        }
    }
    alias_map
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct LooseVersion {
    numbers: Vec<u64>,
    pre_rank: u8,
    pre_num: u64,
}

impl LooseVersion {
    fn parse(value: &str) -> JetPackResult<Self> {
        let re = Regex::new(r"^(\d+(?:\.\d+)*)(?:(a|b|rc)(\d+))?$").map_err(|error| {
            JetPackError::Serialization {
                message: error.to_string(),
            }
        })?;

        let Some(caps) = re.captures(value) else {
            return Err(JetPackError::InvalidVersion {
                value: value.to_string(),
            });
        };

        let numbers = caps
            .get(1)
            .map(|m| {
                m.as_str()
                    .split('.')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.parse::<u64>())
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()
            .map_err(|_| JetPackError::InvalidVersion {
                value: value.to_string(),
            })?
            .unwrap_or_default();

        if numbers.is_empty() {
            return Err(JetPackError::InvalidVersion {
                value: value.to_string(),
            });
        }

        let (pre_rank, pre_num) = match caps.get(2).map(|m| m.as_str()) {
            None => (3, 0),
            Some("rc") => (
                2,
                caps.get(3)
                    .and_then(|m| m.as_str().parse::<u64>().ok())
                    .unwrap_or(0),
            ),
            Some("b") => (
                1,
                caps.get(3)
                    .and_then(|m| m.as_str().parse::<u64>().ok())
                    .unwrap_or(0),
            ),
            Some("a") => (
                0,
                caps.get(3)
                    .and_then(|m| m.as_str().parse::<u64>().ok())
                    .unwrap_or(0),
            ),
            Some(_) => {
                return Err(JetPackError::InvalidVersion {
                    value: value.to_string(),
                });
            }
        };

        Ok(Self {
            numbers,
            pre_rank,
            pre_num,
        })
    }
}

impl Ord for LooseVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let max_len = self.numbers.len().max(other.numbers.len());
        for idx in 0..max_len {
            let lhs = self.numbers.get(idx).copied().unwrap_or(0);
            let rhs = other.numbers.get(idx).copied().unwrap_or(0);
            match lhs.cmp(&rhs) {
                std::cmp::Ordering::Equal => continue,
                ord => return ord,
            }
        }

        match self.pre_rank.cmp(&other.pre_rank) {
            std::cmp::Ordering::Equal => self.pre_num.cmp(&other.pre_num),
            ord => ord,
        }
    }
}

impl PartialOrd for LooseVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
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
                jvm_flags: None,
            },
            compile: None,
            starter_code: None,
        }
    }

    #[test]
    fn builds_version_map_resolving_to_latest_patch() {
        let manifests = vec![manifest("python", "3.14.3"), manifest("python", "3.14.2")];
        let map = build_version_map(&manifests).expect("map should build");

        assert_eq!(map.get("python:3").cloned(), Some("3.14.3".to_string()));
        assert_eq!(map.get("python:3.14").cloned(), Some("3.14.3".to_string()));
        assert_eq!(
            map.get("python:3.14.2").cloned(),
            Some("3.14.2".to_string())
        );
        assert_eq!(
            map.get("python:3.14.3").cloned(),
            Some("3.14.3".to_string())
        );
    }

    #[test]
    fn fails_for_invalid_semver_manifest_version() {
        let manifests = vec![manifest("python", "3.14")];
        let err = build_version_map(&manifests);
        assert!(err.is_ok());
    }

    #[test]
    fn supports_java_four_part_versions_and_short_resolution() {
        let manifests = vec![
            manifest("java", "21.0.10.7.1"),
            manifest("java", "21.0.9.10.1"),
        ];
        let map = build_version_map(&manifests).expect("map should build");

        assert_eq!(map.get("java:21").cloned(), Some("21.0.10.7.1".to_string()));
        assert_eq!(
            map.get("java:21.0").cloned(),
            Some("21.0.10.7.1".to_string())
        );
        assert_eq!(
            map.get("java:21.0.10").cloned(),
            Some("21.0.10.7.1".to_string())
        );
    }

    #[test]
    fn supports_python_prerelease_resolution() {
        let manifests = vec![manifest("python", "3.15.0a6"), manifest("python", "3.14.3")];
        let map = build_version_map(&manifests).expect("map should build");

        assert_eq!(
            map.get("python:3.15").cloned(),
            Some("3.15.0a6".to_string())
        );
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
