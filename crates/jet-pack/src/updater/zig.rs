use std::collections::HashMap;

use serde_json::Value;

use crate::{
    error::{JetPackError, JetPackResult},
    manifest::{ExecutionTemplate, RuntimeArchive, RuntimeManifest},
};

use super::{RuntimeUpdater, UpdatedManifest, fetch_json};

/// Updater that produces three manifests per Zig release: C, C++, and Zig.
///
/// Uses the Zig download index at <https://ziglang.org/download/index.json>.
/// Each stable version provides x86_64-linux and aarch64-linux tarballs that
/// bundle Clang + LLD + libc/libc++, making Zig a drop-in C/C++ compiler.
#[derive(Debug, Clone)]
pub struct ZigUpdater;

impl RuntimeUpdater for ZigUpdater {
    fn language(&self) -> &'static str {
        "zig"
    }

    fn fetch_updated_manifests(&self) -> JetPackResult<Vec<UpdatedManifest>> {
        let url = "https://ziglang.org/download/index.json";
        let index = fetch_json(url)?;
        parse_zig_index(&index)
    }
}

/// Parses the Zig download index JSON and produces manifests for the latest
/// stable version. Three manifests are emitted: C, C++, and Zig itself.
pub fn parse_zig_index(index: &Value) -> JetPackResult<Vec<UpdatedManifest>> {
    let obj = index
        .as_object()
        .ok_or_else(|| JetPackError::Serialization {
            message: "zig index is not a JSON object".to_string(),
        })?;

    // Find the latest stable version (skip "master" and any pre-release keys).
    let mut stable_versions: Vec<&str> = obj
        .keys()
        .filter(|k| *k != "master" && !k.contains('-'))
        .map(String::as_str)
        .collect();

    stable_versions.sort_by(|a, b| compare_zig_versions(a, b));

    let latest = stable_versions
        .last()
        .ok_or_else(|| JetPackError::Serialization {
            message: "no stable zig versions found in index".to_string(),
        })?;

    let release = &obj[*latest];

    let runtimes = extract_zig_runtimes(latest, release)?;

    let mut updated = Vec::new();

    // C manifest (zig cc)
    updated.push(UpdatedManifest {
        file_name: format!("c-{latest}.yaml"),
        manifest: RuntimeManifest {
            language: "c".to_string(),
            version: latest.to_string(),
            aliases: vec!["c".to_string(), "gcc".to_string()],
            runtimes: runtimes.clone(),
            compile: Some(ExecutionTemplate {
                command: "/opt/runtime/zig".to_string(),
                args: Some(vec![
                    "cc".to_string(),
                    "{file}".to_string(),
                    "-o".to_string(),
                    "main".to_string(),
                    "-O3".to_string(),
                ]),
                jvm_flags: None,
            }),
            execute: ExecutionTemplate {
                command: "./main".to_string(),
                args: None,
                jvm_flags: None,
            },
            starter_code: Some(
                "#include <stdio.h>\n\nint main() {\n    int a, b;\n    scanf(\"%d %d\", &a, &b);\n    printf(\"The sum is: %d\\n\", a + b);\n    return 0;\n}\n"
                    .to_string(),
            ),
        },
    });

    // C++ manifest (zig c++)
    updated.push(UpdatedManifest {
        file_name: format!("cpp-{latest}.yaml"),
        manifest: RuntimeManifest {
            language: "cpp".to_string(),
            version: latest.to_string(),
            aliases: vec![
                "cpp".to_string(),
                "c++".to_string(),
                "g++".to_string(),
                "cxx".to_string(),
            ],
            runtimes: runtimes.clone(),
            compile: Some(ExecutionTemplate {
                command: "/opt/runtime/zig".to_string(),
                args: Some(vec![
                    "c++".to_string(),
                    "{file}".to_string(),
                    "-o".to_string(),
                    "main".to_string(),
                    "-O3".to_string(),
                ]),
                jvm_flags: None,
            }),
            execute: ExecutionTemplate {
                command: "./main".to_string(),
                args: None,
                jvm_flags: None,
            },
            starter_code: Some(
                "#include <iostream>\nusing namespace std;\n\nint main() {\n    int a, b;\n    cin >> a >> b;\n    cout << \"The sum is: \" << (a + b) << endl;\n    return 0;\n}\n"
                    .to_string(),
            ),
        },
    });

    // Zig manifest
    updated.push(UpdatedManifest {
        file_name: format!("zig-{latest}.yaml"),
        manifest: RuntimeManifest {
            language: "zig".to_string(),
            version: latest.to_string(),
            aliases: vec!["zig".to_string()],
            runtimes,
            compile: Some(ExecutionTemplate {
                command: "/opt/runtime/zig".to_string(),
                args: Some(vec![
                    "build-exe".to_string(),
                    "{file}".to_string(),
                    "-femit-bin=main".to_string(),
                    "-OReleaseFast".to_string(),
                ]),
                jvm_flags: None,
            }),
            execute: ExecutionTemplate {
                command: "./main".to_string(),
                args: None,
                jvm_flags: None,
            },
            starter_code: Some(
                "const std = @import(\"std\");\n\npub fn main() !void {\n    const stdin = std.io.getStdIn().reader();\n    const stdout = std.io.getStdOut().writer();\n\n    var buf: [64]u8 = undefined;\n\n    const a_line = (try stdin.readUntilDelimiterOrEof(&buf, '\\n')) orelse return;\n    const a = try std.fmt.parseInt(i64, std.mem.trim(u8, a_line, &.{ '\\r', '\\n', ' ' }), 10);\n\n    const b_line = (try stdin.readUntilDelimiterOrEof(&buf, '\\n')) orelse return;\n    const b = try std.fmt.parseInt(i64, std.mem.trim(u8, b_line, &.{ '\\r', '\\n', ' ' }), 10);\n\n    try stdout.print(\"The sum is: {d}\\n\", .{a + b});\n}\n"
                    .to_string(),
            ),
        },
    });

    updated.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    Ok(updated)
}

fn extract_zig_runtimes(
    version: &str,
    release: &Value,
) -> JetPackResult<HashMap<String, RuntimeArchive>> {
    let mut runtimes = HashMap::new();

    for (json_key, arch_key) in [("x86_64-linux", "x86_64"), ("aarch64-linux", "aarch64")] {
        let entry = release
            .get(json_key)
            .ok_or_else(|| JetPackError::MissingArchive {
                language: "zig".to_string(),
                version: version.to_string(),
                arch: json_key.to_string(),
            })?;

        let url = entry
            .get("tarball")
            .and_then(Value::as_str)
            .ok_or_else(|| JetPackError::Serialization {
                message: format!("missing tarball URL for zig {version} {json_key}"),
            })?;

        let sha256 = entry
            .get("shasum")
            .and_then(Value::as_str)
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

/// Compares two dotted-numeric version strings (e.g. "0.13.0" vs "0.12.1").
fn compare_zig_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> Vec<u64> {
        s.split('.')
            .filter_map(|part| part.parse::<u64>().ok())
            .collect()
    };
    parse(a).cmp(&parse(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_index() -> Value {
        serde_json::json!({
            "master": {
                "x86_64-linux": {
                    "tarball": "https://ziglang.org/builds/zig-linux-x86_64-0.14.0-dev.tar.xz",
                    "shasum": "aaa"
                },
                "aarch64-linux": {
                    "tarball": "https://ziglang.org/builds/zig-linux-aarch64-0.14.0-dev.tar.xz",
                    "shasum": "bbb"
                }
            },
            "0.13.0": {
                "x86_64-linux": {
                    "tarball": "https://ziglang.org/download/0.13.0/zig-linux-x86_64-0.13.0.tar.xz",
                    "shasum": "deadbeef01"
                },
                "aarch64-linux": {
                    "tarball": "https://ziglang.org/download/0.13.0/zig-linux-aarch64-0.13.0.tar.xz",
                    "shasum": "deadbeef02"
                }
            },
            "0.12.1": {
                "x86_64-linux": {
                    "tarball": "https://ziglang.org/download/0.12.1/zig-linux-x86_64-0.12.1.tar.xz",
                    "shasum": "ccc"
                },
                "aarch64-linux": {
                    "tarball": "https://ziglang.org/download/0.12.1/zig-linux-aarch64-0.12.1.tar.xz",
                    "shasum": "ddd"
                }
            }
        })
    }

    #[test]
    fn picks_latest_stable_and_emits_three_manifests() {
        let manifests = parse_zig_index(&sample_index()).expect("should parse");

        assert_eq!(manifests.len(), 3);

        let c = manifests
            .iter()
            .find(|m| m.manifest.language == "c")
            .unwrap();
        let cpp = manifests
            .iter()
            .find(|m| m.manifest.language == "cpp")
            .unwrap();
        let zig = manifests
            .iter()
            .find(|m| m.manifest.language == "zig")
            .unwrap();

        // All should use version 0.13.0 (latest stable, skipping master)
        assert_eq!(c.manifest.version, "0.13.0");
        assert_eq!(cpp.manifest.version, "0.13.0");
        assert_eq!(zig.manifest.version, "0.13.0");

        // File names
        assert_eq!(c.file_name, "c-0.13.0.yaml");
        assert_eq!(cpp.file_name, "cpp-0.13.0.yaml");
        assert_eq!(zig.file_name, "zig-0.13.0.yaml");
    }

    #[test]
    fn c_manifest_has_correct_compile_and_execute() {
        let manifests = parse_zig_index(&sample_index()).expect("should parse");
        let c = manifests
            .iter()
            .find(|m| m.manifest.language == "c")
            .unwrap();

        let compile = c.manifest.compile.as_ref().expect("c should have compile");
        assert_eq!(compile.command, "/opt/runtime/zig");
        assert_eq!(
            compile.args.as_ref().unwrap(),
            &["cc", "{file}", "-o", "main", "-O3"]
        );
        assert!(compile.jvm_flags.is_none());

        assert_eq!(c.manifest.execute.command, "./main");
        assert!(c.manifest.execute.args.is_none());
    }

    #[test]
    fn cpp_manifest_has_correct_compile_and_execute() {
        let manifests = parse_zig_index(&sample_index()).expect("should parse");
        let cpp = manifests
            .iter()
            .find(|m| m.manifest.language == "cpp")
            .unwrap();

        let compile = cpp
            .manifest
            .compile
            .as_ref()
            .expect("cpp should have compile");
        assert_eq!(compile.command, "/opt/runtime/zig");
        assert_eq!(
            compile.args.as_ref().unwrap(),
            &["c++", "{file}", "-o", "main", "-O3"]
        );
    }

    #[test]
    fn zig_manifest_uses_build_exe_with_release_fast() {
        let manifests = parse_zig_index(&sample_index()).expect("should parse");
        let zig = manifests
            .iter()
            .find(|m| m.manifest.language == "zig")
            .unwrap();

        let compile = zig
            .manifest
            .compile
            .as_ref()
            .expect("zig should have compile");
        assert_eq!(compile.command, "/opt/runtime/zig");
        assert_eq!(
            compile.args.as_ref().unwrap(),
            &["build-exe", "{file}", "-femit-bin=main", "-OReleaseFast"]
        );
    }

    #[test]
    fn runtimes_have_sha256_from_index() {
        let manifests = parse_zig_index(&sample_index()).expect("should parse");
        let zig = manifests
            .iter()
            .find(|m| m.manifest.language == "zig")
            .unwrap();

        let x86 = zig.manifest.runtimes.get("x86_64").unwrap();
        assert_eq!(x86.sha256.as_deref(), Some("deadbeef01"));

        let arm = zig.manifest.runtimes.get("aarch64").unwrap();
        assert_eq!(arm.sha256.as_deref(), Some("deadbeef02"));
    }

    #[test]
    fn skips_master_and_prerelease() {
        let index = serde_json::json!({
            "master": {
                "x86_64-linux": { "tarball": "https://example/master-x64.tar.xz", "shasum": "a" },
                "aarch64-linux": { "tarball": "https://example/master-arm.tar.xz", "shasum": "b" }
            },
            "0.14.0-dev": {
                "x86_64-linux": { "tarball": "https://example/dev-x64.tar.xz", "shasum": "c" },
                "aarch64-linux": { "tarball": "https://example/dev-arm.tar.xz", "shasum": "d" }
            },
            "0.12.1": {
                "x86_64-linux": { "tarball": "https://example/0.12.1-x64.tar.xz", "shasum": "e" },
                "aarch64-linux": { "tarball": "https://example/0.12.1-arm.tar.xz", "shasum": "f" }
            }
        });

        let manifests = parse_zig_index(&index).expect("should parse");
        // Should pick 0.12.1, not master or 0.14.0-dev
        assert!(manifests.iter().all(|m| m.manifest.version == "0.12.1"));
    }
}
