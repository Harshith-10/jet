use std::collections::HashMap;

use crate::{
    error::{JetPackError, JetPackResult},
    manifest::{ExecutionTemplate, RuntimeArchive, RuntimeManifest},
};

use super::{RuntimeUpdater, UpdatedManifest, fetch_text};

/// Updater for Rust (stable channel).
///
/// Fetches the Rust release manifest from `static.rust-lang.org` in TOML
/// format — the same source of truth that `rustup` uses.  The unified `rust`
/// package URL is selected because it bundles `rustc`, `rust-std`, and
/// `cargo` in a single tarball.  The companion [`RustInstaller`] post-install
/// hook then merges the components into a portable sysroot.
///
/// Only the latest stable release is tracked (a single manifest is emitted).
#[derive(Debug, Clone)]
pub struct RustUpdater;

impl RuntimeUpdater for RustUpdater {
    fn language(&self) -> &'static str {
        "rust"
    }

    fn fetch_updated_manifests(&self) -> JetPackResult<Vec<UpdatedManifest>> {
        let url = "https://static.rust-lang.org/dist/channel-rust-stable.toml";
        let toml_text = fetch_text(url)?;
        parse_rust_channel_manifest(&toml_text)
    }
}

/// Parses the Rust stable channel TOML and emits a single manifest for the
/// latest stable version.
pub fn parse_rust_channel_manifest(toml_text: &str) -> JetPackResult<Vec<UpdatedManifest>> {
    let root: toml::Value =
        toml::from_str(toml_text).map_err(|e| JetPackError::Serialization {
            message: format!("failed to parse Rust channel TOML: {e}"),
        })?;

    // pkg.rust.version = "1.76.0 (07dca489a 2024-02-04)"
    let version_raw = root
        .get("pkg")
        .and_then(|p| p.get("rust"))
        .and_then(|r| r.get("version"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| JetPackError::Serialization {
            message: "missing pkg.rust.version in Rust channel TOML".to_string(),
        })?;

    // Extract the semver portion: "1.76.0"
    let version = version_raw
        .split_whitespace()
        .next()
        .unwrap_or(version_raw)
        .to_string();

    let runtimes = extract_rust_runtimes(&root, &version)?;

    let manifest = RuntimeManifest {
        language: "rust".to_string(),
        version: version.clone(),
        aliases: vec!["rust".to_string(), "rs".to_string(), "rustc".to_string()],
        runtimes,
        compile: Some(ExecutionTemplate {
            command: "/opt/runtime/bin/rustc".to_string(),
            args: Some(vec![
                "{file}".to_string(),
                "-o".to_string(),
                "main".to_string(),
            ]),
            jvm_flags: None,
        }),
        execute: ExecutionTemplate {
            command: "./main".to_string(),
            args: None,
            jvm_flags: None,
        },
        starter_code: Some(
            "use std::io;\n\nfn main() {\n    let mut input = String::new();\n    io::stdin().read_line(&mut input).unwrap();\n    let nums: Vec<i64> = input.trim().split_whitespace()\n        .map(|x| x.parse().unwrap())\n        .collect();\n    println!(\"The sum is: {}\", nums[0] + nums[1]);\n}\n"
                .to_string(),
        ),
    };

    Ok(vec![UpdatedManifest {
        file_name: format!("rust-{version}.yaml"),
        manifest,
    }])
}

/// Extracts the unified `rust` package download URLs for x86_64 and aarch64
/// from the parsed TOML manifest.
fn extract_rust_runtimes(
    root: &toml::Value,
    version: &str,
) -> JetPackResult<HashMap<String, RuntimeArchive>> {
    let mut runtimes = HashMap::new();

    let pkg_rust_targets = root
        .get("pkg")
        .and_then(|p| p.get("rust"))
        .and_then(|r| r.get("target"))
        .ok_or_else(|| JetPackError::Serialization {
            message: "missing pkg.rust.target in Rust channel TOML".to_string(),
        })?;

    for (target_triple, arch_key) in [
        ("x86_64-unknown-linux-gnu", "x86_64"),
        ("aarch64-unknown-linux-gnu", "aarch64"),
    ] {
        let target =
            pkg_rust_targets
                .get(target_triple)
                .ok_or_else(|| JetPackError::MissingArchive {
                    language: "rust".to_string(),
                    version: version.to_string(),
                    arch: target_triple.to_string(),
                })?;

        let available = target
            .get("available")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !available {
            return Err(JetPackError::MissingArchive {
                language: "rust".to_string(),
                version: version.to_string(),
                arch: target_triple.to_string(),
            });
        }

        // Prefer xz over gz for smaller downloads
        let url = target
            .get("xz_url")
            .and_then(|v| v.as_str())
            .or_else(|| target.get("url").and_then(|v| v.as_str()))
            .ok_or_else(|| JetPackError::Serialization {
                message: format!("missing download URL for rust {version} {target_triple}"),
            })?;

        let sha256 = target
            .get("xz_hash")
            .and_then(|v| v.as_str())
            .or_else(|| target.get("hash").and_then(|v| v.as_str()))
            .map(ToOwned::to_owned);

        runtimes.insert(
            arch_key.to_string(),
            RuntimeArchive {
                url: url.to_string(),
                sha256,
            },
        );
    }

    Ok(runtimes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rust_channel_toml() {
        let toml_text = r#"
manifest-version = "2"
date = "2024-02-08"

[pkg.rust]
version = "1.76.0 (07dca489a 2024-02-04)"

  [pkg.rust.target.x86_64-unknown-linux-gnu]
  available = true
  url = "https://static.rust-lang.org/dist/2024-02-08/rust-1.76.0-x86_64-unknown-linux-gnu.tar.gz"
  hash = "abc123"
  xz_url = "https://static.rust-lang.org/dist/2024-02-08/rust-1.76.0-x86_64-unknown-linux-gnu.tar.xz"
  xz_hash = "def456"

  [pkg.rust.target.aarch64-unknown-linux-gnu]
  available = true
  url = "https://static.rust-lang.org/dist/2024-02-08/rust-1.76.0-aarch64-unknown-linux-gnu.tar.gz"
  hash = "ghi789"
  xz_url = "https://static.rust-lang.org/dist/2024-02-08/rust-1.76.0-aarch64-unknown-linux-gnu.tar.xz"
  xz_hash = "jkl012"
"#;

        let manifests = parse_rust_channel_manifest(toml_text).expect("should parse");
        assert_eq!(manifests.len(), 1);

        let m = &manifests[0].manifest;
        assert_eq!(m.language, "rust");
        assert_eq!(m.version, "1.76.0");
        assert!(m.aliases.contains(&"rs".to_string()));
        assert!(m.aliases.contains(&"rustc".to_string()));

        let x86 = m.runtimes.get("x86_64").expect("x86_64 runtime");
        assert!(x86.url.ends_with(".tar.xz"));
        assert_eq!(x86.sha256.as_deref(), Some("def456"));

        let aarch64 = m.runtimes.get("aarch64").expect("aarch64 runtime");
        assert!(aarch64.url.ends_with(".tar.xz"));
        assert_eq!(aarch64.sha256.as_deref(), Some("jkl012"));

        assert_eq!(manifests[0].file_name, "rust-1.76.0.yaml");
    }

    #[test]
    fn prefers_xz_over_gz() {
        let toml_text = r#"
manifest-version = "2"
date = "2024-02-08"

[pkg.rust]
version = "1.76.0 (07dca489a 2024-02-04)"

  [pkg.rust.target.x86_64-unknown-linux-gnu]
  available = true
  url = "https://example.com/rust.tar.gz"
  hash = "gz_hash"
  xz_url = "https://example.com/rust.tar.xz"
  xz_hash = "xz_hash"

  [pkg.rust.target.aarch64-unknown-linux-gnu]
  available = true
  url = "https://example.com/rust-aarch64.tar.gz"
  hash = "gz_hash_arm"
"#;

        let manifests = parse_rust_channel_manifest(toml_text).expect("should parse");
        let m = &manifests[0].manifest;

        // x86_64 should use xz
        let x86 = m.runtimes.get("x86_64").unwrap();
        assert!(x86.url.ends_with(".tar.xz"));
        assert_eq!(x86.sha256.as_deref(), Some("xz_hash"));

        // aarch64 has no xz, should fall back to gz
        let aarch64 = m.runtimes.get("aarch64").unwrap();
        assert!(aarch64.url.ends_with(".tar.gz"));
        assert_eq!(aarch64.sha256.as_deref(), Some("gz_hash_arm"));
    }

    #[test]
    fn rejects_unavailable_target() {
        let toml_text = r#"
manifest-version = "2"
date = "2024-02-08"

[pkg.rust]
version = "1.76.0 (07dca489a 2024-02-04)"

  [pkg.rust.target.x86_64-unknown-linux-gnu]
  available = false

  [pkg.rust.target.aarch64-unknown-linux-gnu]
  available = true
  url = "https://example.com/rust.tar.gz"
  hash = "abc"
"#;

        let result = parse_rust_channel_manifest(toml_text);
        assert!(result.is_err());
    }

    #[test]
    fn extracts_semver_from_version_string() {
        let toml_text = r#"
manifest-version = "2"

[pkg.rust]
version = "1.85.1 (4eb161250 2025-03-15)"

  [pkg.rust.target.x86_64-unknown-linux-gnu]
  available = true
  url = "https://example.com/rust.tar.gz"
  hash = "abc"

  [pkg.rust.target.aarch64-unknown-linux-gnu]
  available = true
  url = "https://example.com/rust-arm.tar.gz"
  hash = "def"
"#;

        let manifests = parse_rust_channel_manifest(toml_text).expect("should parse");
        assert_eq!(manifests[0].manifest.version, "1.85.1");
    }
}
