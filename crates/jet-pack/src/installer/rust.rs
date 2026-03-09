use std::{fs, path::Path};

use crate::error::{JetPackError, JetPackResult};

use super::{InstallContext, RuntimeInstaller};

/// Installer for Rust (stable channel).
///
/// **Post-install**: restructures the extracted unified `rust` tarball into a
/// portable sysroot.  The unified tarball bundles `rustc`, `rust-std`, and
/// `cargo` as separate subdirectories.  This hook merges them into a single
/// `bin/` + `lib/` tree so that `./bin/rustc` can locate the standard library
/// via its built-in relative RPATH (`$ORIGIN/../lib`).
#[derive(Debug, Clone)]
pub struct RustInstaller;

impl RuntimeInstaller for RustInstaller {
    fn languages(&self) -> &[&str] {
        &["rust"]
    }

    fn post_install(&self, _ctx: &InstallContext, installed_path: &Path) -> JetPackResult<()> {
        build_portable_sysroot(installed_path)
    }
}

/// Merges the `rustc/`, `rust-std-*/`, and optionally `cargo/` subdirectories
/// into a flat portable sysroot under `installed_path`.
///
/// After this function returns the directory will look like:
///
/// ```text
/// installed_path/
/// ├── bin/
/// │   ├── rustc
/// │   └── cargo
/// └── lib/
///     ├── librustc_driver.so
///     └── rustlib/
/// ```
pub(crate) fn build_portable_sysroot(installed_path: &Path) -> JetPackResult<()> {
    // 1. Merge rustc/ into root
    let rustc_dir = installed_path.join("rustc");
    if rustc_dir.is_dir() {
        merge_tree(&rustc_dir, installed_path)?;
        fs::remove_dir_all(&rustc_dir).map_err(|source| JetPackError::Io {
            path: rustc_dir,
            source,
        })?;
    }

    // 2. Merge all rust-std-*/ directories into root (typically one per target)
    for entry in read_dir_entries(installed_path)? {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if entry.path().is_dir() && name_str.starts_with("rust-std-") {
            merge_tree(&entry.path(), installed_path)?;
            fs::remove_dir_all(entry.path()).map_err(|source| JetPackError::Io {
                path: entry.path(),
                source,
            })?;
        }
    }

    // 3. Optionally merge cargo/ into root
    let cargo_dir = installed_path.join("cargo");
    if cargo_dir.is_dir() {
        merge_tree(&cargo_dir, installed_path)?;
        fs::remove_dir_all(&cargo_dir).map_err(|source| JetPackError::Io {
            path: cargo_dir,
            source,
        })?;
    }

    // 4. Clean up installer artifacts (install.sh, components, etc.)
    for name in &["install.sh", "components", "rust-installer-version"] {
        let path = installed_path.join(name);
        if path.exists() {
            let _ = fs::remove_file(&path);
        }
    }

    // Remove any remaining manifest.in files from Rust installer components
    clean_manifest_files(installed_path)?;

    Ok(())
}

/// Recursively merge `src_dir` into `dst_dir`.
///
/// Directories are merged; files are moved (overwriting any existing file).
/// `manifest.in` files from Rust installer components are skipped.
fn merge_tree(src_dir: &Path, dst_dir: &Path) -> JetPackResult<()> {
    for entry in read_dir_entries(src_dir)? {
        let name = entry.file_name();
        let src_path = entry.path();
        let dst_path = dst_dir.join(&name);

        if src_path.is_dir() {
            if !dst_path.exists() {
                fs::create_dir_all(&dst_path).map_err(|source| JetPackError::Io {
                    path: dst_path.clone(),
                    source,
                })?;
            }
            merge_tree(&src_path, &dst_path)?;
        } else {
            // Skip manifest.in files from Rust installer components
            if name.to_string_lossy() == "manifest.in" {
                continue;
            }
            fs::rename(&src_path, &dst_path).map_err(|source| JetPackError::Io {
                path: src_path.clone(),
                source,
            })?;
        }
    }

    Ok(())
}

fn read_dir_entries(dir: &Path) -> JetPackResult<Vec<fs::DirEntry>> {
    fs::read_dir(dir)
        .map_err(|source| JetPackError::Io {
            path: dir.to_path_buf(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| JetPackError::Io {
            path: dir.to_path_buf(),
            source,
        })
}

fn clean_manifest_files(dir: &Path) -> JetPackResult<()> {
    for entry in read_dir_entries(dir)? {
        let path = entry.path();
        if path.is_dir() {
            clean_manifest_files(&path)?;
        } else if path.file_name().and_then(|n| n.to_str()) == Some("manifest.in") {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Simulate the directory layout produced by extracting the unified
    /// `rust` tarball after `flatten_single_top_level_dir`.
    #[test]
    fn builds_portable_sysroot_from_unified_tarball() {
        let dir = tempdir().expect("temp dir");
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();

        // Simulate rustc component
        let rustc = root.join("rustc");
        fs::create_dir_all(rustc.join("bin")).unwrap();
        fs::write(rustc.join("bin").join("rustc"), "rustc-binary").unwrap();
        fs::write(rustc.join("bin").join("rustdoc"), "rustdoc-binary").unwrap();
        fs::create_dir_all(rustc.join("lib")).unwrap();
        fs::write(rustc.join("lib").join("librustc_driver.so"), "driver").unwrap();
        fs::write(rustc.join("manifest.in"), "rustc-manifest").unwrap();

        // Simulate rust-std component
        let std_dir = root.join("rust-std-x86_64-unknown-linux-gnu");
        fs::create_dir_all(std_dir.join("lib").join("rustlib").join("x86_64-unknown-linux-gnu").join("lib")).unwrap();
        fs::write(
            std_dir.join("lib").join("rustlib").join("x86_64-unknown-linux-gnu").join("lib").join("libstd.rlib"),
            "std-lib",
        ).unwrap();
        fs::write(std_dir.join("manifest.in"), "std-manifest").unwrap();

        // Simulate cargo component
        let cargo = root.join("cargo");
        fs::create_dir_all(cargo.join("bin")).unwrap();
        fs::write(cargo.join("bin").join("cargo"), "cargo-binary").unwrap();
        fs::write(cargo.join("manifest.in"), "cargo-manifest").unwrap();

        // Simulate installer artifacts
        fs::write(root.join("install.sh"), "#!/bin/sh").unwrap();
        fs::write(root.join("components"), "rustc\nrust-std\ncargo").unwrap();
        fs::write(root.join("rust-installer-version"), "3").unwrap();

        // Run the sysroot builder
        build_portable_sysroot(&root).expect("sysroot build should succeed");

        // Verify portable layout
        assert!(root.join("bin").join("rustc").exists());
        assert!(root.join("bin").join("rustdoc").exists());
        assert!(root.join("bin").join("cargo").exists());
        assert!(root.join("lib").join("librustc_driver.so").exists());
        assert!(root.join("lib").join("rustlib").join("x86_64-unknown-linux-gnu").join("lib").join("libstd.rlib").exists());

        // Verify cleanup
        assert!(!root.join("rustc").exists(), "rustc/ should be removed");
        assert!(!root.join("cargo").exists(), "cargo/ should be removed");
        assert!(!root.join("rust-std-x86_64-unknown-linux-gnu").exists());
        assert!(!root.join("install.sh").exists());
        assert!(!root.join("components").exists());
        assert!(!root.join("rust-installer-version").exists());

        // No manifest.in files should remain
        let has_manifest_in = walkdir(&root)
            .iter()
            .any(|p| p.file_name().and_then(|n| n.to_str()) == Some("manifest.in"));
        assert!(!has_manifest_in, "no manifest.in files should remain");
    }

    /// Recursively list all files under a directory.
    fn walkdir(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    files.extend(walkdir(&path));
                } else {
                    files.push(path);
                }
            }
        }
        files
    }
}
