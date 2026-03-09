use std::{
    fs,
    path::{Path, PathBuf},
};

use indicatif::ProgressBar;

use crate::{
    archive::extract_archive,
    downloader::{download_to_path, download_to_path_with_progress},
    error::{JetPackError, JetPackResult},
    installer::{InstallContext, get_installer_for},
    manifest::RuntimeManifest,
    resolver::{InMemoryVersionStore, VersionResolver, scan_manifest_dir},
    updater::RuntimeUpdater,
};

/// Result of resolving a language + version to a concrete manifest.
#[derive(Debug, Clone)]
pub struct ResolvedManifest {
    /// The canonical language name (e.g. `"cpp"` even if `"c++"` was requested).
    pub canonical_language: String,
    /// The fully qualified version string (e.g. `"3.14.3"` from `"3"`).
    pub resolved_version: String,
    /// The matched manifest.
    pub manifest: RuntimeManifest,
}

/// Result of a successful runtime installation.
#[derive(Debug, Clone)]
pub struct InstallResult {
    /// The canonical language name.
    pub canonical_language: String,
    /// The fully qualified version string.
    pub resolved_version: String,
    /// Path to the extracted runtime root directory.
    pub installed_path: PathBuf,
}

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

    /// Resolve a language name + version string to the concrete manifest.
    ///
    /// Handles alias resolution (e.g. `"c++"` → `"cpp"`) and version
    /// resolution (e.g. `"3"` → `"3.14.3"`) in one call.
    pub fn resolve_manifest(
        &self,
        language: &str,
        version: &str,
    ) -> JetPackResult<ResolvedManifest> {
        let resolver = self.build_resolver()?;
        let canonical_lang = resolver.canonical_language(language).to_owned();
        let resolved_version = resolver.resolve(language, version)?.ok_or_else(|| {
            JetPackError::ManifestNotFound {
                language: language.to_string(),
                version: version.to_string(),
            }
        })?;

        let manifests = self.scan_manifests()?;
        let manifest = manifests
            .into_iter()
            .find(|m| m.language == canonical_lang && m.version == resolved_version)
            .ok_or_else(|| JetPackError::ManifestNotFound {
                language: language.to_string(),
                version: resolved_version.clone(),
            })?;

        Ok(ResolvedManifest {
            canonical_language: canonical_lang,
            resolved_version,
            manifest,
        })
    }

    pub fn install_runtime(
        &self,
        manifest: &RuntimeManifest,
        arch: &str,
    ) -> JetPackResult<PathBuf> {
        let installer = get_installer_for(&manifest.language);
        let ctx = InstallContext {
            runtime_dir: &self.runtime_dir,
            manifest,
            arch,
        };

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

        // Pre-install hook (e.g. Java old-major cleanup)
        installer.pre_install(&ctx)?;

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

        // Post-install hook (e.g. Zig cache dir creation)
        installer.post_install(&ctx, &extracted_path)?;

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
        let installer = get_installer_for(&manifest.language);
        let ctx = InstallContext {
            runtime_dir: &self.runtime_dir,
            manifest,
            arch,
        };

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

        // Pre-install hook (e.g. Java old-major cleanup)
        installer.pre_install(&ctx)?;

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

        // Post-install hook (e.g. Zig cache dir creation)
        installer.post_install(&ctx, &extracted_path)?;

        Ok(extracted_path)
    }

    /// High-level install: resolve version + find manifest + install + run hooks.
    ///
    /// Combines [`resolve_manifest`](Self::resolve_manifest) and
    /// [`install_runtime`](Self::install_runtime) into a single call.
    pub fn full_install(
        &self,
        language: &str,
        version: &str,
        arch: &str,
    ) -> JetPackResult<InstallResult> {
        let resolved = self.resolve_manifest(language, version)?;
        let installed_path = self.install_runtime(&resolved.manifest, arch)?;

        Ok(InstallResult {
            canonical_language: resolved.canonical_language,
            resolved_version: resolved.resolved_version,
            installed_path,
        })
    }

    /// Like [`full_install`](Self::full_install) but reports download progress.
    pub fn full_install_with_progress(
        &self,
        language: &str,
        version: &str,
        arch: &str,
        download_progress: &ProgressBar,
        on_extract: impl FnOnce(),
    ) -> JetPackResult<InstallResult> {
        let resolved = self.resolve_manifest(language, version)?;
        let installed_path = self.install_runtime_with_progress(
            &resolved.manifest,
            arch,
            download_progress,
            on_extract,
        )?;

        Ok(InstallResult {
            canonical_language: resolved.canonical_language,
            resolved_version: resolved.resolved_version,
            installed_path,
        })
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

/// Returns `true` for languages that use Zig as their compiler backend.
pub fn is_zig_language(language: &str) -> bool {
    matches!(language, "c" | "cpp" | "zig")
}

/// Returns the path to the Zig global cache directory for a given runtime
/// root.  The cache lives as a sibling of `root/` so it can be bind-mounted
/// independently (read-write) into the sandbox.
///
/// Layout: `<runtime_install_dir>/<lang>/<version>/zig-cache/`
pub fn zig_cache_dir_for(runtime_root: &Path) -> Option<PathBuf> {
    let cache = runtime_root.parent()?.join("zig-cache");
    if cache.is_dir() { Some(cache) } else { None }
}

/// Create the empty `zig-cache` directory next to the runtime root.
///
/// The actual cache population happens server-side inside a sandbox
/// (see [`jet_server::worker::evaluator`]) so that zig's path-dependent
/// cache keys match the sandbox mount layout.
pub fn prepare_zig_cache_dir(runtime_root: &Path) -> JetPackResult<PathBuf> {
    let cache_dir = runtime_root
        .parent()
        .map(|p| p.join("zig-cache"))
        .unwrap_or_else(|| runtime_root.join("zig-cache"));
    fs::create_dir_all(&cache_dir).map_err(|source| JetPackError::Io {
        path: cache_dir.clone(),
        source,
    })?;
    Ok(cache_dir)
}

/// Extracts a file name from a URL for use as the cache key.
fn archive_file_name(url: &str) -> String {
    url.split('/')
        .next_back()
        .filter(|v| !v.is_empty())
        .unwrap_or("runtime.tar.gz")
        .to_string()
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
