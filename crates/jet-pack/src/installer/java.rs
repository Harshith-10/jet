use std::{fs, path::Path};

use crate::error::{JetPackError, JetPackResult};

use super::{InstallContext, RuntimeInstaller};

/// Installer for Java (Amazon Corretto).
///
/// **Pre-install**: removes previously installed versions of the same major
/// release so only the latest patch is kept. For example, installing
/// `21.0.10.7.1` will remove an existing `21.0.9.10.1` but leave
/// `17.0.18.9.1` intact.
#[derive(Debug, Clone)]
pub struct JavaInstaller;

impl Default for JavaInstaller {
    fn default() -> Self {
        Self
    }
}

impl RuntimeInstaller for JavaInstaller {
    fn languages(&self) -> &[&str] {
        &["java"]
    }

    fn pre_install(&self, ctx: &InstallContext) -> JetPackResult<()> {
        remove_old_java_major_installs(ctx.runtime_dir, &ctx.manifest.version)
    }
}

/// Remove previously installed Java versions that share the same major as
/// `incoming_version`. This keeps only one patch level per major.
pub(crate) fn remove_old_java_major_installs(
    runtime_dir: &Path,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

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
