use std::collections::HashMap;

use regex::Regex;
use serde_json::Value;

use crate::{
    error::{JetPackError, JetPackResult},
    manifest::{ExecutionTemplate, RuntimeArchive, RuntimeManifest},
};

use super::{RuntimeUpdater, UpdatedManifest, fetch_json};

#[derive(Debug, Clone)]
pub struct PythonStandaloneUpdater;

impl RuntimeUpdater for PythonStandaloneUpdater {
    fn language(&self) -> &'static str {
        "python"
    }

    fn fetch_updated_manifests(&self) -> JetPackResult<Vec<UpdatedManifest>> {
        let url = "https://api.github.com/repos/astral-sh/python-build-standalone/releases/latest";
        let release_json = fetch_json(url)?;
        parse_python_release_manifests(&release_json)
    }
}

pub fn parse_python_release_manifests(release: &Value) -> JetPackResult<Vec<UpdatedManifest>> {
    let assets = release
        .get("assets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let version_regex = Regex::new(
        r"cpython-(\d+\.\d+\.\d+(?:[ab]\d+|rc\d+)?)\+(\d{8})-(x86_64_v3|aarch64)-unknown-linux-gnu-install_only\.tar\.gz",
    )
    .map_err(|error| JetPackError::Serialization {
        message: error.to_string(),
    })?;

    let mut grouped: HashMap<String, HashMap<String, (String, Option<String>)>> = HashMap::new();

    for asset in assets {
        let Some(name) = asset.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(url) = asset
            .get("browser_download_url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
        else {
            continue;
        };

        let sha256 = asset
            .get("digest")
            .and_then(Value::as_str)
            .and_then(|d| d.strip_prefix("sha256:"))
            .map(ToOwned::to_owned);

        let Some(caps) = version_regex.captures(name) else {
            continue;
        };

        let version = caps
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let arch_suffix = caps.get(3).map(|m| m.as_str()).unwrap_or_default();
        let arch_key = match arch_suffix {
            "x86_64_v3" => "x86_64",
            "aarch64" => "aarch64",
            _ => continue,
        };

        grouped
            .entry(version)
            .or_default()
            .insert(arch_key.to_string(), (url, sha256));
    }

    let mut latest_by_major_minor: HashMap<(u64, u64), String> = HashMap::new();

    for version in grouped.keys() {
        let parsed = parse_python_version(version)?;
        let key = (parsed.major, parsed.minor);

        match latest_by_major_minor.get(&key) {
            Some(existing) => {
                let existing_parsed = parse_python_version(existing)?;
                if parsed > existing_parsed {
                    latest_by_major_minor.insert(key, version.clone());
                }
            }
            None => {
                latest_by_major_minor.insert(key, version.clone());
            }
        }
    }

    let mut updated = Vec::new();

    for version in latest_by_major_minor.values() {
        let Some(runtime_entries) = grouped.get(version) else {
            continue;
        };

        let Some((x64_url, x64_sha256)) = runtime_entries.get("x86_64") else {
            continue;
        };
        let Some((aarch64_url, aarch64_sha256)) = runtime_entries.get("aarch64") else {
            continue;
        };

        let mut runtimes = HashMap::new();
        runtimes.insert(
            "x86_64".to_string(),
            RuntimeArchive {
                url: x64_url.clone(),
                sha256: x64_sha256.clone(),
            },
        );
        runtimes.insert(
            "aarch64".to_string(),
            RuntimeArchive {
                url: aarch64_url.clone(),
                sha256: aarch64_sha256.clone(),
            },
        );

        let manifest = RuntimeManifest {
            language: "python".to_string(),
            version: version.clone(),
            aliases: vec![
                format!(
                    "{}.{}",
                    parse_python_version(version)?.major,
                    parse_python_version(version)?.minor
                ),
                "python3".to_string(),
                "py".to_string(),
            ],
            runtimes,
            compile: None,
            execute: ExecutionTemplate {
                command: "/opt/runtime/bin/python3".to_string(),
                args: Some(vec!["{file}".to_string()]),
                jvm_flags: None,
            },
            starter_code: Some(
                "a = int(input())\nb = int(input())\nprint(\"The sum is: \" + str(a + b))\n"
                    .to_string(),
            ),
        };

        updated.push(UpdatedManifest {
            file_name: format!("python-{version}.yaml"),
            manifest,
        });
    }

    updated.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    Ok(updated)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
struct PythonVersion {
    major: u64,
    minor: u64,
    patch: u64,
    pre_rank: u8,
    pre_num: u64,
}

fn parse_python_version(value: &str) -> JetPackResult<PythonVersion> {
    let re = Regex::new(r"^(\d+)\.(\d+)\.(\d+)(?:(a|b|rc)(\d+))?$").map_err(|error| {
        JetPackError::Serialization {
            message: error.to_string(),
        }
    })?;

    let Some(caps) = re.captures(value) else {
        return Err(JetPackError::InvalidVersion {
            value: value.to_string(),
        });
    };

    let major = caps
        .get(1)
        .and_then(|m| m.as_str().parse::<u64>().ok())
        .ok_or_else(|| JetPackError::InvalidVersion {
            value: value.to_string(),
        })?;
    let minor = caps
        .get(2)
        .and_then(|m| m.as_str().parse::<u64>().ok())
        .ok_or_else(|| JetPackError::InvalidVersion {
            value: value.to_string(),
        })?;
    let patch = caps
        .get(3)
        .and_then(|m| m.as_str().parse::<u64>().ok())
        .ok_or_else(|| JetPackError::InvalidVersion {
            value: value.to_string(),
        })?;

    let (pre_rank, pre_num) = match caps.get(4).map(|m| m.as_str()) {
        None => (3, 0),
        Some("rc") => (
            2,
            caps.get(5)
                .and_then(|m| m.as_str().parse::<u64>().ok())
                .unwrap_or(0),
        ),
        Some("b") => (
            1,
            caps.get(5)
                .and_then(|m| m.as_str().parse::<u64>().ok())
                .unwrap_or(0),
        ),
        Some("a") => (
            0,
            caps.get(5)
                .and_then(|m| m.as_str().parse::<u64>().ok())
                .unwrap_or(0),
        ),
        Some(_) => {
            return Err(JetPackError::InvalidVersion {
                value: value.to_string(),
            });
        }
    };

    Ok(PythonVersion {
        major,
        minor,
        patch,
        pre_rank,
        pre_num,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_python_release_and_picks_latest_patch_per_major_minor() {
        let release = serde_json::json!({
            "assets": [
                {
                    "name": "cpython-3.14.2+20260211-x86_64_v3-unknown-linux-gnu-install_only.tar.gz",
                    "browser_download_url": "https://example/3.14.2-x64.tar.gz",
                    "digest": "sha256:aaa142x64"
                },
                {
                    "name": "cpython-3.14.2+20260211-aarch64-unknown-linux-gnu-install_only.tar.gz",
                    "browser_download_url": "https://example/3.14.2-arm.tar.gz",
                    "digest": "sha256:aaa142arm"
                },
                {
                    "name": "cpython-3.14.3+20260211-x86_64_v3-unknown-linux-gnu-install_only.tar.gz",
                    "browser_download_url": "https://example/3.14.3-x64.tar.gz",
                    "digest": "sha256:aaa143x64"
                },
                {
                    "name": "cpython-3.14.3+20260211-aarch64-unknown-linux-gnu-install_only.tar.gz",
                    "browser_download_url": "https://example/3.14.3-arm.tar.gz",
                    "digest": "sha256:aaa143arm"
                },
                {
                    "name": "cpython-3.15.0a6+20260211-x86_64_v3-unknown-linux-gnu-install_only.tar.gz",
                    "browser_download_url": "https://example/3.15.0a6-x64.tar.gz",
                    "digest": "sha256:bbb150a6x64"
                },
                {
                    "name": "cpython-3.15.0a6+20260211-aarch64-unknown-linux-gnu-install_only.tar.gz",
                    "browser_download_url": "https://example/3.15.0a6-arm.tar.gz",
                    "digest": "sha256:bbb150a6arm"
                }
            ]
        });

        let manifests =
            parse_python_release_manifests(&release).expect("python manifests should parse");

        assert_eq!(manifests.len(), 2);
        assert!(
            manifests
                .iter()
                .any(|m| m.file_name == "python-3.14.3.yaml")
        );
        assert!(
            manifests
                .iter()
                .any(|m| m.file_name == "python-3.15.0a6.yaml")
        );

        let m314 = manifests
            .iter()
            .find(|m| m.file_name == "python-3.14.3.yaml")
            .unwrap();
        assert_eq!(
            m314.manifest
                .runtimes
                .get("x86_64")
                .unwrap()
                .sha256
                .as_deref(),
            Some("aaa143x64")
        );
        assert_eq!(
            m314.manifest
                .runtimes
                .get("aarch64")
                .unwrap()
                .sha256
                .as_deref(),
            Some("aaa143arm")
        );

        let m315 = manifests
            .iter()
            .find(|m| m.file_name == "python-3.15.0a6.yaml")
            .unwrap();
        assert_eq!(
            m315.manifest
                .runtimes
                .get("x86_64")
                .unwrap()
                .sha256
                .as_deref(),
            Some("bbb150a6x64")
        );
        assert_eq!(
            m315.manifest
                .runtimes
                .get("aarch64")
                .unwrap()
                .sha256
                .as_deref(),
            Some("bbb150a6arm")
        );
    }
}
