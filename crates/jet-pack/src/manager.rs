use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{
    archive::extract_archive,
    downloader::download_to_path,
    error::{JetPackError, JetPackResult},
    manifest::RuntimeManifest,
    resolver::{InMemoryVersionStore, VersionResolver, scan_manifest_dir},
    updater::RuntimeUpdater,
};

#[derive(Debug, Clone)]
pub struct ManifestSource {
    pub file_name: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct PackageManager {
    pub runtime_dir: PathBuf,
    pub manifest_dir: PathBuf,
}

impl PackageManager {
    pub fn new(runtime_dir: impl Into<PathBuf>, manifest_dir: impl Into<PathBuf>) -> Self {
        Self {
            runtime_dir: runtime_dir.into(),
            manifest_dir: manifest_dir.into(),
        }
    }

    pub fn scan_manifests(&self) -> JetPackResult<Vec<RuntimeManifest>> {
        scan_manifest_dir(&self.manifest_dir)
    }

    pub fn build_resolver(&self) -> JetPackResult<VersionResolver<InMemoryVersionStore>> {
        let manifests = self.scan_manifests()?;
        let mut resolver = VersionResolver::new(InMemoryVersionStore::default());
        resolver.initialize_from_manifests(&manifests)?;
        Ok(resolver)
    }

    pub fn install_runtime(
        &self,
        manifest: &RuntimeManifest,
        arch: &str,
    ) -> JetPackResult<PathBuf> {
        let archive = manifest
            .runtimes
            .get(arch)
            .ok_or_else(|| JetPackError::MissingArchive {
                language: manifest.language.clone(),
                version: manifest.version.clone(),
                arch: arch.to_string(),
            })?;

        fs::create_dir_all(&self.runtime_dir).map_err(|source| JetPackError::Io {
            path: self.runtime_dir.clone(),
            source,
        })?;

        if manifest.language == "java" {
            remove_old_java_major_installs(&self.runtime_dir, &manifest.version)?;
        }

        let target_dir = self
            .runtime_dir
            .join(&manifest.language)
            .join(&manifest.version);

        if target_dir.exists() {
            fs::remove_dir_all(&target_dir).map_err(|source| JetPackError::Io {
                path: target_dir.clone(),
                source,
            })?;
        }

        fs::create_dir_all(&target_dir).map_err(|source| JetPackError::Io {
            path: target_dir.clone(),
            source,
        })?;

        let file_name = archive
            .url
            .split('/')
            .next_back()
            .filter(|v| !v.is_empty())
            .unwrap_or("runtime.tar.gz");

        let archive_path = target_dir.join(file_name);
        download_to_path(&archive.url, &archive_path)?;

        let extracted_path = target_dir.join("root");
        extract_archive(&archive_path, &extracted_path)?;
        flatten_single_top_level_dir(&extracted_path)?;

        Ok(extracted_path)
    }

    pub fn update_manifests(&self, sources: &[ManifestSource]) -> JetPackResult<Vec<PathBuf>> {
        fs::create_dir_all(&self.manifest_dir).map_err(|source| JetPackError::Io {
            path: self.manifest_dir.clone(),
            source,
        })?;

        let mut updated = Vec::new();
        for source in sources {
            let destination = self.manifest_dir.join(&source.file_name);
            download_to_path(&source.url, &destination)?;
            updated.push(destination);
        }

        Ok(updated)
    }

    pub fn update_manifests_with_updater<U: RuntimeUpdater>(
        &self,
        updater: &U,
    ) -> JetPackResult<Vec<PathBuf>> {
        fs::create_dir_all(&self.manifest_dir).map_err(|source| JetPackError::Io {
            path: self.manifest_dir.clone(),
            source,
        })?;

        let generated = updater.fetch_updated_manifests()?;
        let mut paths = Vec::new();

        for item in generated {
            let path = self.manifest_dir.join(item.file_name);
            let yaml = serde_yaml::to_string(&item.manifest).map_err(|error| {
                JetPackError::Serialization {
                    message: error.to_string(),
                }
            })?;
            fs::write(&path, yaml).map_err(|source| JetPackError::Io {
                path: path.clone(),
                source,
            })?;
            paths.push(path);
        }

        Ok(paths)
    }
}

fn remove_old_java_major_installs(
    runtime_dir: &PathBuf,
    incoming_version: &str,
) -> JetPackResult<()> {
    let major = incoming_version
        .split('.')
        .next()
        .ok_or_else(|| JetPackError::InvalidVersion {
            value: incoming_version.to_string(),
        })?;

    let java_dir = runtime_dir.join("java");
    if !java_dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(&java_dir).map_err(|source| JetPackError::Io {
        path: java_dir.clone(),
        source,
    })? {
        let entry = entry.map_err(|source| JetPackError::Io {
            path: java_dir.clone(),
            source,
        })?;

        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let Some(version) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        if version == incoming_version {
            continue;
        }

        if version.split('.').next() == Some(major) {
            fs::remove_dir_all(&path).map_err(|source| JetPackError::Io {
                path: path.clone(),
                source,
            })?;
        }
    }

    Ok(())
}

fn flatten_single_top_level_dir(extracted_path: &Path) -> JetPackResult<()> {
    let entries = fs::read_dir(extracted_path).map_err(|source| JetPackError::Io {
        path: extracted_path.to_path_buf(),
        source,
    })?;

    let mut child_dirs = Vec::new();
    let mut non_dirs = 0usize;

    for entry in entries {
        let entry = entry.map_err(|source| JetPackError::Io {
            path: extracted_path.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir() {
            child_dirs.push(path);
        } else {
            non_dirs += 1;
        }
    }

    if child_dirs.len() != 1 || non_dirs != 0 {
        return Ok(());
    }

    let single_dir = child_dirs.pop().expect("single root child exists");
    let single_name = single_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if matches!(
        single_name,
        "bin" | "lib" | "include" | "share" | "etc" | "usr"
    ) {
        return Ok(());
    }
    let nested_entries = fs::read_dir(&single_dir).map_err(|source| JetPackError::Io {
        path: single_dir.clone(),
        source,
    })?;

    for nested in nested_entries {
        let nested = nested.map_err(|source| JetPackError::Io {
            path: single_dir.clone(),
            source,
        })?;
        let from = nested.path();
        let to = extracted_path.join(nested.file_name());
        fs::rename(&from, &to).map_err(|source| JetPackError::Io { path: from, source })?;
    }

    fs::remove_dir(&single_dir).map_err(|source| JetPackError::Io {
        path: single_dir,
        source,
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ExecutionTemplate, RuntimeArchive};
    use flate2::{Compression, write::GzEncoder};
    use std::{collections::HashMap, fs, path::Path};
    use tar::Builder;
    use tempfile::tempdir;

    use crate::updater::{RuntimeUpdater, UpdatedManifest};

    struct FakeUpdater;

    impl RuntimeUpdater for FakeUpdater {
        fn language(&self) -> &'static str {
            "fake"
        }

        fn fetch_updated_manifests(&self) -> JetPackResult<Vec<UpdatedManifest>> {
            Ok(vec![UpdatedManifest {
                file_name: "python-3.14.3.yaml".to_string(),
                manifest: RuntimeManifest {
                    language: "python".to_string(),
                    version: "3.14.3".to_string(),
                    aliases: vec!["py".to_string()],
                    runtimes: HashMap::new(),
                    compile: None,
                    execute: ExecutionTemplate {
                        command: "python3".to_string(),
                        args: Some(vec!["{file}".to_string()]),
                    },
                },
            }])
        }
    }

    fn build_manifest(archive_url: String) -> RuntimeManifest {
        let mut runtimes = HashMap::new();
        runtimes.insert(
            "x86_64".to_string(),
            RuntimeArchive {
                url: archive_url,
                sha256: None,
            },
        );

        RuntimeManifest {
            language: "python".to_string(),
            version: "3.14.3".to_string(),
            aliases: vec!["3".to_string()],
            runtimes,
            execute: ExecutionTemplate {
                command: "python".to_string(),
                args: Some(vec!["main.py".to_string()]),
            },
            compile: None,
        }
    }

    fn create_runtime_archive(path: &Path) {
        let tar_gz = fs::File::create(path).expect("archive file should be created");
        let encoder = GzEncoder::new(tar_gz, Compression::default());
        let mut tar = Builder::new(encoder);

        let content = b"print('ok')";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();

        tar.append_data(&mut header, "bin/runtime.txt", &content[..])
            .expect("append data should pass");
        tar.finish().expect("archive should finish");
    }

    #[test]
    fn installs_runtime_from_file_url_archive() {
        let dir = tempdir().expect("temp dir should exist");
        let runtime_dir = dir.path().join("runtimes");
        let manifest_dir = dir.path().join("manifests");
        let source_archive = dir.path().join("python.tar.gz");
        create_runtime_archive(&source_archive);

        let manager = PackageManager::new(&runtime_dir, &manifest_dir);
        let manifest = build_manifest(format!("file://{}", source_archive.display()));

        let extracted = manager
            .install_runtime(&manifest, "x86_64")
            .expect("install should pass");

        let file = extracted.join("bin/runtime.txt");
        assert!(file.exists());
    }

    #[test]
    fn install_fails_for_missing_arch() {
        let dir = tempdir().expect("temp dir should exist");
        let manager =
            PackageManager::new(dir.path().join("runtimes"), dir.path().join("manifests"));
        let manifest = build_manifest("file:///tmp/does-not-matter.tar.gz".to_string());

        let err = manager.install_runtime(&manifest, "arm64");
        assert!(matches!(err, Err(JetPackError::MissingArchive { .. })));
    }

    #[test]
    fn updates_manifests_from_sources() {
        let dir = tempdir().expect("temp dir should exist");
        let source_manifest = dir.path().join("python.yaml");
        fs::write(
            &source_manifest,
            "language: python\nversion: 3.14.3\nruntimes: {}\nexecute: { command: python }\n",
        )
        .expect("manifest source should be written");

        let manager =
            PackageManager::new(dir.path().join("runtimes"), dir.path().join("manifests"));

        let updated = manager
            .update_manifests(&[ManifestSource {
                file_name: "python.yaml".to_string(),
                url: format!("file://{}", source_manifest.display()),
            }])
            .expect("update should pass");

        assert_eq!(updated.len(), 1);
        assert!(updated[0].exists());
    }

    #[test]
    fn updates_manifests_from_runtime_updater_output() {
        let dir = tempdir().expect("temp dir should exist");
        let manager =
            PackageManager::new(dir.path().join("runtimes"), dir.path().join("manifests"));

        let written = manager
            .update_manifests_with_updater(&FakeUpdater)
            .expect("updater write should pass");

        assert_eq!(written.len(), 1);
        assert!(written[0].exists());
    }

    #[test]
    fn java_install_removes_old_major_versions() {
        let dir = tempdir().expect("temp dir should exist");
        let java_base = dir.path().join("runtimes").join("java");
        fs::create_dir_all(java_base.join("21.0.9.10.1")).expect("old version dir");
        fs::create_dir_all(java_base.join("17.0.18.9.1")).expect("different major dir");

        remove_old_java_major_installs(&dir.path().join("runtimes"), "21.0.10.7.1")
            .expect("prune should pass");

        assert!(!java_base.join("21.0.9.10.1").exists());
        assert!(java_base.join("17.0.18.9.1").exists());
    }
}
