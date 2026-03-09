use std::collections::HashMap;

use regex::Regex;
use serde_json::Value;

use crate::{
    error::{JetPackError, JetPackResult},
    manifest::{ExecutionTemplate, RuntimeArchive, RuntimeManifest},
};

use super::{RuntimeUpdater, UpdatedManifest, fetch_json};

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
            let release_json = fetch_json(&url)?;
            let manifest = parse_corretto_release_manifest(major, &release_json)?;

            updated.push(UpdatedManifest {
                file_name: format!("java-{major}.yaml"),
                manifest,
            });
        }

        Ok(updated)
    }
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
            args: Some(vec!["-cp".to_string(), ".".to_string(), "{class}".to_string()]),
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
        starter_code: Some(
            "import java.util.*;\n\npublic class Main {\n    public static void main(String[] args) {\n        Scanner sc = new Scanner(System.in);\n        int a = sc.nextInt();\n        int b = sc.nextInt();\n        System.out.println(\"The sum is: \" + (a + b));\n    }\n}\n"
                .to_string(),
        ),
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
}
