use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use indicatif::ProgressBar;

use crate::{
    archive::extract_archive,
    downloader::{download_to_path, download_to_path_with_progress},
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
    pub download_dir: PathBuf,
}

impl PackageManager {
    pub fn new(runtime_dir: impl Into<PathBuf>, manifest_dir: impl Into<PathBuf>) -> Self {
        let runtime_dir = runtime_dir.into();
        let download_dir = runtime_dir.join("downloads");
        Self {
            runtime_dir,
            manifest_dir: manifest_dir.into(),
            download_dir,
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

        // Download archive to shared cache directory
        let cached_archive = self.download_to_cache(&archive.url)?;

        // Extract from cache into version-specific directory
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

        let extracted_path = target_dir.join("root");
        extract_archive(&cached_archive, &extracted_path)?;
        flatten_single_top_level_dir(&extracted_path)?;

        Ok(extracted_path)
    }

    /// Like `install_runtime` but reports download progress to an `indicatif::ProgressBar`.
    pub fn install_runtime_with_progress(
        &self,
        manifest: &RuntimeManifest,
        arch: &str,
        download_progress: &ProgressBar,
        on_extract: impl FnOnce(),
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

        // Download archive to shared cache directory (skip if cached)
        let cached_archive =
            self.download_to_cache_with_progress(&archive.url, download_progress)?;

        // Signal that download is complete, extraction is starting
        on_extract();

        // Extract from cache into version-specific directory
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

        let extracted_path = target_dir.join("root");
        extract_archive(&cached_archive, &extracted_path)?;
        flatten_single_top_level_dir(&extracted_path)?;

        Ok(extracted_path)
    }

    /// Returns the path to the cached archive, downloading only if not already present.
    fn download_to_cache(&self, url: &str) -> JetPackResult<PathBuf> {
        let file_name = archive_file_name(url);
        let cached_path = self.download_dir.join(&file_name);

        if cached_path.exists() {
            return Ok(cached_path);
        }

        fs::create_dir_all(&self.download_dir).map_err(|source| JetPackError::Io {
            path: self.download_dir.clone(),
            source,
        })?;

        download_to_path(url, &cached_path)?;
        Ok(cached_path)
    }

    /// Like `download_to_cache` but reports progress. Completes immediately if cached.
    fn download_to_cache_with_progress(
        &self,
        url: &str,
        progress: &ProgressBar,
    ) -> JetPackResult<PathBuf> {
        let file_name = archive_file_name(url);
        let cached_path = self.download_dir.join(&file_name);

        if cached_path.exists() {
            let len = fs::metadata(&cached_path).map(|m| m.len()).unwrap_or(0);
            progress.set_length(len);
            progress.set_position(len);
            return Ok(cached_path);
        }

        fs::create_dir_all(&self.download_dir).map_err(|source| JetPackError::Io {
            path: self.download_dir.clone(),
            source,
        })?;

        download_to_path_with_progress(url, &cached_path, progress)?;
        Ok(cached_path)
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

    pub fn update_manifests_with_updater<U: RuntimeUpdater + ?Sized>(
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

    /// Remove an installed runtime for a specific language and version.
    /// Does not remove cached downloads.
    pub fn uninstall_runtime(&self, language: &str, version: &str) -> JetPackResult<bool> {
        let target_dir = self.runtime_dir.join(language).join(version);

        if !target_dir.exists() {
            return Ok(false);
        }

        fs::remove_dir_all(&target_dir).map_err(|source| JetPackError::Io {
            path: target_dir.clone(),
            source,
        })?;

        // Clean up empty language directory if no versions remain
        let language_dir = self.runtime_dir.join(language);
        if language_dir.exists() {
            let is_empty = fs::read_dir(&language_dir)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false);
            if is_empty {
                let _ = fs::remove_dir(&language_dir);
            }
        }

        Ok(true)
    }

    /// Remove all cached download archives. Returns the number of files removed.
    pub fn clean_downloads(&self) -> JetPackResult<usize> {
        if !self.download_dir.exists() {
            return Ok(0);
        }

        let mut count = 0;
        for entry in fs::read_dir(&self.download_dir).map_err(|source| JetPackError::Io {
            path: self.download_dir.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| JetPackError::Io {
                path: self.download_dir.clone(),
                source,
            })?;

            let path = entry.path();
            if path.is_file() {
                fs::remove_file(&path).map_err(|source| JetPackError::Io {
                    path: path.clone(),
                    source,
                })?;
                count += 1;
            }
        }

        // Remove the now-empty downloads directory itself
        let _ = fs::remove_dir(&self.download_dir);

        Ok(count)
    }
}

/// Minimal C++ source used to warm up Zig's global cache.
///
/// `#include <iostream>` forces Zig to decompress the bundled libc++
/// headers, which is the expensive part (~10 s on first run).
const WARMUP_CPP_SOURCE: &str = r#"#include <iostream>
int main() { std::cout << "ok" << std::endl; return 0; }
"#;

/// Returns `true` for languages that use Zig as their compiler backend.
pub fn is_zig_language(language: &str) -> bool {
    matches!(language, "c" | "cpp" | "zig")
}

/// Warm up Zig's global cache by compiling a trivial C++ program.
///
/// Zig lazily decompresses bundled libc/libc++ headers on first use,
/// which can take 10+ seconds inside a sandboxed container (where the
/// cache is discarded after each run).  Running a single warm-up
/// compilation at install time populates a `zig-cache` directory inside
/// the runtime root so that the sandbox can bind-mount it read-only.
/// Returns the path to the Zig global cache directory for a given runtime
/// root.  The cache lives as a sibling of `root/` so it can be bind-mounted
/// independently (read-write) into the sandbox.
///
/// Layout: `<runtime_install_dir>/<lang>/<version>/zig-cache/`
pub fn zig_cache_dir_for(runtime_root: &Path) -> Option<PathBuf> {
    let cache = runtime_root.parent()?.join("zig-cache");
    if cache.is_dir() { Some(cache) } else { None }
}

pub fn warm_zig_cache(runtime_root: &Path) -> JetPackResult<()> {
    let zig_binary = runtime_root.join("zig");
    if !zig_binary.exists() {
        return Ok(());
    }

    // Place cache as sibling of root/ so it can be separately bind-mounted.
    let cache_dir = runtime_root
        .parent()
        .map(|p| p.join("zig-cache"))
        .unwrap_or_else(|| runtime_root.join("zig-cache"));
    fs::create_dir_all(&cache_dir).map_err(|source| JetPackError::Io {
        path: cache_dir.clone(),
        source,
    })?;

    // Create a temporary workspace with a trivial C++ file.
    let tmp = tempfile::tempdir().map_err(|source| JetPackError::Io {
        path: PathBuf::from("/tmp"),
        source,
    })?;
    let cpp_file = tmp.path().join("warmup.cpp");
    fs::write(&cpp_file, WARMUP_CPP_SOURCE).map_err(|source| JetPackError::Io {
        path: cpp_file.clone(),
        source,
    })?;

    let output = Command::new(&zig_binary)
        .args(["c++", "warmup.cpp", "-o", "main", "-O3"])
        .current_dir(tmp.path())
        .env("ZIG_GLOBAL_CACHE_DIR", &cache_dir)
        .output()
        .map_err(|source| JetPackError::Io {
            path: zig_binary.clone(),
            source,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(JetPackError::Serialization {
            message: format!("zig cache warm-up failed: {}", stderr.trim()),
        });
    }

    Ok(())
}

/// Extracts a file name from a URL for use as the cache key.
fn archive_file_name(url: &str) -> String {
    url.split('/')
        .next_back()
        .filter(|v| !v.is_empty())
        .unwrap_or("runtime.tar.gz")
        .to_string()
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
                        jvm_flags: None,
                    },
                    starter_code: None,
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
                jvm_flags: None,
            },
            compile: None,
            starter_code: None,
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

    #[test]
    fn install_caches_archive_in_download_dir() {
        let dir = tempdir().expect("temp dir should exist");
        let runtime_dir = dir.path().join("runtimes");
        let manifest_dir = dir.path().join("manifests");
        let source_archive = dir.path().join("python.tar.gz");
        create_runtime_archive(&source_archive);

        let manager = PackageManager::new(&runtime_dir, &manifest_dir);
        let archive_url = format!("file://{}", source_archive.display());
        let manifest = build_manifest(archive_url);

        // First install downloads to cache
        manager
            .install_runtime(&manifest, "x86_64")
            .expect("first install should pass");

        let cached = runtime_dir.join("downloads").join("python.tar.gz");
        assert!(cached.exists(), "archive should be cached in downloads/");

        // Remove the original source to prove second install uses cache
        fs::remove_file(&source_archive).expect("remove source");

        // Second install should succeed from cache alone
        let extracted = manager
            .install_runtime(&manifest, "x86_64")
            .expect("second install should use cache");

        assert!(extracted.join("bin/runtime.txt").exists());
    }

    #[test]
    fn shared_archive_reused_across_languages() {
        let dir = tempdir().expect("temp dir should exist");
        let runtime_dir = dir.path().join("runtimes");
        let manifest_dir = dir.path().join("manifests");
        let source_archive = dir.path().join("zig-linux.tar.gz");
        create_runtime_archive(&source_archive);

        let manager = PackageManager::new(&runtime_dir, &manifest_dir);
        let archive_url = format!("file://{}", source_archive.display());

        // Two different "languages" share the same archive URL
        let c_manifest = {
            let mut m = build_manifest(archive_url.clone());
            m.language = "c".to_string();
            m.version = "0.13.0".to_string();
            m
        };
        let cpp_manifest = {
            let mut m = build_manifest(archive_url);
            m.language = "cpp".to_string();
            m.version = "0.13.0".to_string();
            m
        };

        manager
            .install_runtime(&c_manifest, "x86_64")
            .expect("c install should pass");

        // Remove original to prove cache is used
        fs::remove_file(&source_archive).expect("remove source");

        let extracted = manager
            .install_runtime(&cpp_manifest, "x86_64")
            .expect("cpp install should reuse cached archive");

        assert!(extracted.join("bin/runtime.txt").exists());
    }

    #[test]
    fn uninstall_removes_runtime_directory() {
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
        assert!(extracted.exists());

        let removed = manager
            .uninstall_runtime("python", "3.14.3")
            .expect("uninstall should pass");
        assert!(removed);
        assert!(!extracted.exists());

        // Language directory should also be cleaned up
        assert!(!runtime_dir.join("python").exists());
    }

    #[test]
    fn uninstall_returns_false_for_missing_runtime() {
        let dir = tempdir().expect("temp dir should exist");
        let manager =
            PackageManager::new(dir.path().join("runtimes"), dir.path().join("manifests"));

        let removed = manager
            .uninstall_runtime("python", "99.99.99")
            .expect("uninstall should not error");
        assert!(!removed);
    }

    #[test]
    fn uninstall_preserves_download_cache() {
        let dir = tempdir().expect("temp dir should exist");
        let runtime_dir = dir.path().join("runtimes");
        let manifest_dir = dir.path().join("manifests");
        let source_archive = dir.path().join("python.tar.gz");
        create_runtime_archive(&source_archive);

        let manager = PackageManager::new(&runtime_dir, &manifest_dir);
        let manifest = build_manifest(format!("file://{}", source_archive.display()));

        manager
            .install_runtime(&manifest, "x86_64")
            .expect("install should pass");

        let cached = runtime_dir.join("downloads").join("python.tar.gz");
        assert!(cached.exists());

        manager
            .uninstall_runtime("python", "3.14.3")
            .expect("uninstall should pass");

        // Download cache should still exist
        assert!(
            cached.exists(),
            "uninstall should not remove cached downloads"
        );
    }

    #[test]
    fn clean_removes_cached_downloads() {
        let dir = tempdir().expect("temp dir should exist");
        let runtime_dir = dir.path().join("runtimes");
        let manifest_dir = dir.path().join("manifests");
        let source_archive = dir.path().join("python.tar.gz");
        create_runtime_archive(&source_archive);

        let manager = PackageManager::new(&runtime_dir, &manifest_dir);
        let manifest = build_manifest(format!("file://{}", source_archive.display()));

        manager
            .install_runtime(&manifest, "x86_64")
            .expect("install should pass");

        let cached = runtime_dir.join("downloads").join("python.tar.gz");
        assert!(cached.exists());

        let count = manager.clean_downloads().expect("clean should pass");
        assert_eq!(count, 1);
        assert!(!cached.exists());
        assert!(
            !runtime_dir.join("downloads").exists(),
            "empty downloads dir should be removed"
        );

        // Installed runtime should still exist
        assert!(
            runtime_dir
                .join("python")
                .join("3.14.3")
                .join("root")
                .exists()
        );
    }

    #[test]
    fn clean_on_empty_cache_returns_zero() {
        let dir = tempdir().expect("temp dir should exist");
        let manager =
            PackageManager::new(dir.path().join("runtimes"), dir.path().join("manifests"));

        let count = manager.clean_downloads().expect("clean should pass");
        assert_eq!(count, 0);
    }
}
