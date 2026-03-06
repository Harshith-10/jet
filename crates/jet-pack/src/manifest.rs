use std::{collections::HashMap, fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::error::{JetPackError, JetPackResult};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeArchive {
    pub url: String,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionTemplate {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jvm_flags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeManifest {
    pub language: String,
    pub version: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub runtimes: HashMap<String, RuntimeArchive>,
    pub execute: ExecutionTemplate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compile: Option<ExecutionTemplate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starter_code: Option<String>,
}

pub fn parse_manifest_yaml(yaml: &str) -> JetPackResult<RuntimeManifest> {
    serde_yaml::from_str(yaml).map_err(|source| JetPackError::ManifestParse {
        path: Path::new("<inline>").to_path_buf(),
        source,
    })
}

pub fn parse_manifest_file(path: &Path) -> JetPackResult<RuntimeManifest> {
    let raw = fs::read_to_string(path).map_err(|source| JetPackError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    serde_yaml::from_str(&raw).map_err(|source| JetPackError::ManifestParse {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_manifest_yaml() {
        let yaml = r#"
language: python
version: 3.14.3
aliases: ["3", "3.14"]
runtimes:
  x86_64:
    url: file:///tmp/python-3.14.3.tar.gz
    sha256: null
execute:
  command: python
  args: ["main.py"]
compile: null
"#;

        let manifest = parse_manifest_yaml(yaml).expect("yaml should parse");
        assert_eq!(manifest.language, "python");
        assert_eq!(manifest.version, "3.14.3");
        assert_eq!(manifest.aliases, vec!["3", "3.14"]);
        assert_eq!(manifest.starter_code, None);
    }

    #[test]
    fn parses_manifest_with_starter_code() {
        let yaml = r#"
language: python
version: 3.14.3
aliases: ["3", "3.14"]
runtimes:
  x86_64:
    url: file:///tmp/python-3.14.3.tar.gz
    sha256: null
execute:
  command: python
  args: ["main.py"]
compile: null
starter_code: |
  a = int(input())
  b = int(input())
  print("The sum is: " + str(a + b))
"#;

        let manifest = parse_manifest_yaml(yaml).expect("yaml should parse");
        assert!(manifest.starter_code.is_some());
        assert!(manifest.starter_code.unwrap().contains("The sum is:"));
    }

    #[test]
    fn fails_for_invalid_yaml() {
        let yaml = "language: python\nversion: [bad";
        let err = parse_manifest_yaml(yaml);
        assert!(matches!(err, Err(JetPackError::ManifestParse { .. })));
    }
}
