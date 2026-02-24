use std::collections::HashMap;

use regex::Regex;
use serde_json::Value;

use crate::{
    error::{JetPackError, JetPackResult},
    manifest::{ExecutionTemplate, RuntimeArchive, RuntimeManifest},
};

#[derive(Debug, Clone)]
pub struct UpdatedManifest {
    pub file_name: String,
    pub manifest: RuntimeManifest,
}

pub trait RuntimeUpdater {
    fn language(&self) -> &'static str;
    fn fetch_updated_manifests(&self) -> JetPackResult<Vec<UpdatedManifest>>;
}

#[derive(Debug, Clone)]
pub struct JavaCorrettoUpdater {
    pub majors: Vec<String>,
}

impl Default for JavaCorrettoUpdater {
    fn default() -> Self {
        Self {
            majors: vec!["8", "11", "17", "21", "25"]
                .into_iter()
                .map(ToOwned::to_owned)
                .collect(),
        }
    }
}

impl RuntimeUpdater for JavaCorrettoUpdater {
    fn language(&self) -> &'static str {
        "java"
    }

    fn fetch_updated_manifests(&self) -> JetPackResult<Vec<UpdatedManifest>> {
        let mut updated = Vec::new();

        for major in &self.majors {
            let url =
                format!("https://api.github.com/repos/corretto/corretto-{major}/releases/latest");
            let release_json = github_get_json(&url)?;
            let manifest = parse_corretto_release_manifest(major, &release_json)?;

            updated.push(UpdatedManifest {
                file_name: format!("java-{major}.yaml"),
                manifest,
            });
        }

        Ok(updated)
    }
}

#[derive(Debug, Clone)]
pub struct PythonStandaloneUpdater;

impl RuntimeUpdater for PythonStandaloneUpdater {
    fn language(&self) -> &'static str {
        "python"
    }

    fn fetch_updated_manifests(&self) -> JetPackResult<Vec<UpdatedManifest>> {
        let url = "https://api.github.com/repos/astral-sh/python-build-standalone/releases/latest";
        let release_json = github_get_json(url)?;
        parse_python_release_manifests(&release_json)
    }
}

fn github_get_json(url: &str) -> JetPackResult<Value> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("jet-pack-updater")
        .build()
        .map_err(|source| JetPackError::Http {
            url: url.to_string(),
            source,
        })?;

    let response = client
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|source| JetPackError::Http {
            url: url.to_string(),
            source,
        })?;

    let text = response.text().map_err(|source| JetPackError::Http {
        url: url.to_string(),
        source,
    })?;

    serde_json::from_str(&text).map_err(|error| JetPackError::Serialization {
        message: format!("failed to parse github response JSON: {error}"),
    })
}

pub fn parse_corretto_release_manifest(
    major: &str,
    release: &Value,
) -> JetPackResult<RuntimeManifest> {
    let tag_name = release
        .get("tag_name")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let version_regex =
        Regex::new(r"(\d+\.[\d.]+)").map_err(|error| JetPackError::Serialization {
            message: error.to_string(),
        })?;

    let Some(version_capture) = version_regex.captures(tag_name) else {
        return Err(JetPackError::InvalidVersion {
            value: tag_name.to_string(),
        });
    };

    let Some(full_version) = version_capture.get(1).map(|m| m.as_str().to_string()) else {
        return Err(JetPackError::InvalidVersion {
            value: tag_name.to_string(),
        });
    };

    let body = release
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let url_regex = Regex::new(
        r"https://corretto\.aws/downloads/resources/[^/]+/amazon-corretto-[^/]+-linux-([^/]+)\.tar\.gz",
    )
    .map_err(|error| JetPackError::Serialization {
        message: error.to_string(),
    })?;

    let mut x86_64_url = None;
    let mut aarch64_url = None;

    for cap in url_regex.captures_iter(body) {
        let full_url = cap.get(0).map(|m| m.as_str()).unwrap_or_default();
        let arch = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
        if full_url.contains("alpine") {
            continue;
        }

        match arch {
            "x64" => x86_64_url = Some(full_url.to_string()),
            "aarch64" => aarch64_url = Some(full_url.to_string()),
            _ => {}
        }
    }

    let mut runtimes = HashMap::new();
    if let Some(url) = x86_64_url {
        runtimes.insert("x86_64".to_string(), RuntimeArchive { url, sha256: None });
    }
    if let Some(url) = aarch64_url {
        runtimes.insert("aarch64".to_string(), RuntimeArchive { url, sha256: None });
    }

    if !runtimes.contains_key("x86_64") || !runtimes.contains_key("aarch64") {
        return Err(JetPackError::MissingArchive {
            language: "java".to_string(),
            version: full_version,
            arch: "x86_64/aarch64".to_string(),
        });
    }

    Ok(RuntimeManifest {
        language: "java".to_string(),
        version: full_version.clone(),
        aliases: vec![
            major.to_string(),
            format!("java{major}"),
            format!("jdk{major}"),
        ],
        runtimes,
        compile: Some(ExecutionTemplate {
            command: "/opt/runtime/bin/javac".to_string(),
            args: Some(vec!["{file}".to_string()]),
            jvm_flags: Some(vec![
                "-Xms16m".to_string(),
                "-Xmx256m".to_string(),
                "-XX:MaxMetaspaceSize=64m".to_string(),
                "-XX:CompressedClassSpaceSize=32m".to_string(),
                "-XX:ReservedCodeCacheSize=32m".to_string(),
                "-XX:+UseSerialGC".to_string(),
                "-Xss256k".to_string(),
            ]),
        }),
        execute: ExecutionTemplate {
            command: "/opt/runtime/bin/java".to_string(),
            args: Some(vec!["-cp".to_string(), ".".to_string(), "Main".to_string()]),
            jvm_flags: Some(vec![
                "-Xms8m".to_string(),
                "-Xmx64m".to_string(),
                "-XX:MaxMetaspaceSize=32m".to_string(),
                "-XX:CompressedClassSpaceSize=16m".to_string(),
                "-XX:ReservedCodeCacheSize=16m".to_string(),
                "-XX:+UseSerialGC".to_string(),
                "-Xss256k".to_string(),
            ]),
        },
    })
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

    let mut grouped: HashMap<String, HashMap<String, String>> = HashMap::new();

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
            .insert(arch_key.to_string(), url);
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
        let Some(runtime_urls) = grouped.get(version) else {
            continue;
        };

        let Some(x64_url) = runtime_urls.get("x86_64") else {
            continue;
        };
        let Some(aarch64_url) = runtime_urls.get("aarch64") else {
            continue;
        };

        let mut runtimes = HashMap::new();
        runtimes.insert(
            "x86_64".to_string(),
            RuntimeArchive {
                url: x64_url.clone(),
                sha256: None,
            },
        );
        runtimes.insert(
            "aarch64".to_string(),
            RuntimeArchive {
                url: aarch64_url.clone(),
                sha256: None,
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
    fn parses_corretto_release_into_runtime_manifest() {
        let json = serde_json::json!({
            "tag_name": "21.0.6.7.1",
            "body": "https://corretto.aws/downloads/resources/21.0.6.7.1/amazon-corretto-21.0.6.7.1-linux-x64.tar.gz\nhttps://corretto.aws/downloads/resources/21.0.6.7.1/amazon-corretto-21.0.6.7.1-linux-aarch64.tar.gz"
        });

        let manifest = parse_corretto_release_manifest("21", &json).expect("manifest should parse");
        assert_eq!(manifest.language, "java");
        assert_eq!(manifest.version, "21.0.6.7.1");
        assert!(manifest.runtimes.contains_key("x86_64"));
        assert!(manifest.runtimes.contains_key("aarch64"));
    }

    #[test]
    fn fails_corretto_parse_when_archives_missing() {
        let json = serde_json::json!({
            "tag_name": "21.0.6.7.1",
            "body": "https://corretto.aws/downloads/resources/21.0.6.7.1/amazon-corretto-21.0.6.7.1-linux-x64.tar.gz"
        });

        let result = parse_corretto_release_manifest("21", &json);
        assert!(matches!(result, Err(JetPackError::MissingArchive { .. })));
    }

    #[test]
    fn parses_python_release_and_picks_latest_patch_per_major_minor() {
        let release = serde_json::json!({
            "assets": [
                {
                    "name": "cpython-3.14.2+20260211-x86_64_v3-unknown-linux-gnu-install_only.tar.gz",
                    "browser_download_url": "https://example/3.14.2-x64.tar.gz"
                },
                {
                    "name": "cpython-3.14.2+20260211-aarch64-unknown-linux-gnu-install_only.tar.gz",
                    "browser_download_url": "https://example/3.14.2-arm.tar.gz"
                },
                {
                    "name": "cpython-3.14.3+20260211-x86_64_v3-unknown-linux-gnu-install_only.tar.gz",
                    "browser_download_url": "https://example/3.14.3-x64.tar.gz"
                },
                {
                    "name": "cpython-3.14.3+20260211-aarch64-unknown-linux-gnu-install_only.tar.gz",
                    "browser_download_url": "https://example/3.14.3-arm.tar.gz"
                },
                {
                    "name": "cpython-3.15.0a6+20260211-x86_64_v3-unknown-linux-gnu-install_only.tar.gz",
                    "browser_download_url": "https://example/3.15.0a6-x64.tar.gz"
                },
                {
                    "name": "cpython-3.15.0a6+20260211-aarch64-unknown-linux-gnu-install_only.tar.gz",
                    "browser_download_url": "https://example/3.15.0a6-arm.tar.gz"
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
    }
}
