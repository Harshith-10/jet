use std::fs;
use std::path::Path;

use jet_core::models::{ExecutionLimits, FileRequest};
use jet_pack::RuntimeManifest;

use super::traits::{LanguageBackend, WriteResult};
use super::{
    DEFAULT_JAVA_COMPILE_JVM_FLAGS, DEFAULT_JAVA_RUN_JVM_FLAGS, DEFAULT_JVM_COMPILE_MEMORY_BYTES,
    DEFAULT_JVM_RUN_MEMORY_BYTES,
};

/// Java-specific backend that extracts the public class name from
/// the submitted source code so the file is saved as `<ClassName>.java`
/// and the `java` command receives the correct class to run.
pub struct JavaBackend;

// ---------------------------------------------------------------------------
// Class-name extraction
// ---------------------------------------------------------------------------

/// Extract the name of the `public class` (or `public enum`, etc.) that
/// contains `public static void main` from a Java source string.
///
/// Strategy:
/// 1. Find all top-level `public class <Name>` declarations.
/// 2. If exactly one exists, use it (Java mandates at most one public
///    top-level type per compilation unit).
/// 3. Otherwise fall back to whichever class contains
///    `public static void main`.
/// 4. If nothing matches, return `None`.
fn extract_public_class_name(source: &str) -> Option<String> {
    // Simple but effective: we scan for `public class <Ident>` at the
    // top level.  A full Java parser is overkill – the regex below
    // handles the overwhelmingly common case.
    let public_class_re =
        regex::Regex::new(r"(?m)^\s*public\s+(?:(?:abstract|final|strictfp)\s+)*class\s+(\w+)")
            .unwrap();

    let captures: Vec<String> = public_class_re
        .captures_iter(source)
        .map(|c| c[1].to_string())
        .collect();

    if captures.len() == 1 {
        return Some(captures.into_iter().next().unwrap());
    }

    // Multiple (or zero) public classes – look for the one with `main`.
    // We do a crude search: for each candidate, check whether `main`
    // appears in the source after its declaration.
    if captures.is_empty() {
        // No *public* class – look for any class with main.
        return extract_class_with_main(source);
    }

    for name in &captures {
        // Find the class body and look for a main method signature.
        if class_body_has_main(source, name) {
            return Some(name.clone());
        }
    }

    // Last resort: first public class.
    captures.into_iter().next()
}

/// Find any class (public or not) that contains a `main` method.
fn extract_class_with_main(source: &str) -> Option<String> {
    let class_re =
        regex::Regex::new(r"(?m)^\s*(?:(?:public|abstract|final|strictfp)\s+)*class\s+(\w+)")
            .unwrap();

    for cap in class_re.captures_iter(source) {
        let name = cap[1].to_string();
        if class_body_has_main(source, &name) {
            return Some(name);
        }
    }
    None
}

/// Heuristic: does the source region following `class <name>` contain
/// `public static void main`?
fn class_body_has_main(source: &str, class_name: &str) -> bool {
    // Find the position of `class <name>` and search the subsequent text
    // up to the next `\nclass ` (crude boundary) for the main signature.
    let pattern = format!("class {class_name}");
    if let Some(start) = source.find(&pattern) {
        let rest = &source[start + pattern.len()..];
        // Limit the search to the "next class" boundary (best effort).
        let end = rest
            .find("\nclass ")
            .or_else(|| rest.find("\npublic class "))
            .unwrap_or(rest.len());
        let body = &rest[..end];
        // Match `public static void main` with optional whitespace.
        let main_re = regex::Regex::new(r"public\s+static\s+void\s+main\s*\(").unwrap();
        return main_re.is_match(body);
    }
    false
}

// ---------------------------------------------------------------------------
// LanguageBackend implementation
// ---------------------------------------------------------------------------

impl LanguageBackend for JavaBackend {
    fn write_files(&self, workspace: &Path, files: &[FileRequest]) -> std::io::Result<WriteResult> {
        let mut primary_file = "Main.java".to_string();
        let mut class_name: Option<String> = None;

        for (i, file) in files.iter().enumerate() {
            // For the first (primary) file we attempt class-name extraction.
            if i == 0 {
                let extracted = extract_public_class_name(&file.content);
                let target_class = extracted.unwrap_or_else(|| "Main".to_string());
                let target_file = format!("{target_class}.java");

                let path = workspace.join(&target_file);
                fs::write(&path, &file.content)?;

                primary_file = target_file;
                class_name = Some(target_class);
            } else {
                // Secondary files: save under their original name, or try
                // to extract the class name for proper naming.
                let target = if let Some(ref name) = file.name {
                    name.clone()
                } else {
                    let extracted = extract_public_class_name(&file.content);
                    let cls = extracted.unwrap_or_else(|| format!("File{i}"));
                    format!("{cls}.java")
                };
                let path = workspace.join(&target);
                fs::write(&path, &file.content)?;
            }
        }

        Ok(WriteResult {
            primary_file,
            class_name,
        })
    }

    fn adjust_compile_limits(&self, limits: &mut ExecutionLimits, _manifest: &RuntimeManifest) {
        limits.memory_limit_bytes = limits
            .memory_limit_bytes
            .max(DEFAULT_JVM_COMPILE_MEMORY_BYTES);
    }

    fn adjust_run_limits(&self, limits: &mut ExecutionLimits, _manifest: &RuntimeManifest) {
        limits.memory_limit_bytes = limits.memory_limit_bytes.max(DEFAULT_JVM_RUN_MEMORY_BYTES);
    }

    fn build_compile_args(
        &self,
        template_args: Vec<String>,
        manifest: &RuntimeManifest,
    ) -> Vec<String> {
        let compile_template = match &manifest.compile {
            Some(t) => t,
            None => return template_args,
        };

        let flags: Vec<&str> = compile_template
            .jvm_flags
            .as_ref()
            .map(|v| v.iter().map(|s| s.as_str()).collect())
            .unwrap_or_else(|| DEFAULT_JAVA_COMPILE_JVM_FLAGS.to_vec());

        let mut args: Vec<String> = flags.iter().map(|f| format!("-J{f}")).collect();
        args.extend(template_args);
        args
    }

    fn build_run_args(
        &self,
        template_args: Vec<String>,
        manifest: &RuntimeManifest,
    ) -> Vec<String> {
        let jvm_flags: Vec<String> =
            manifest
                .execute
                .jvm_flags
                .as_ref()
                .cloned()
                .unwrap_or_else(|| {
                    DEFAULT_JAVA_RUN_JVM_FLAGS
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
                });

        let mut args = jvm_flags;
        args.extend(template_args);
        args
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_simple_public_class() {
        let src = r#"
public class HelloWorld {
    public static void main(String[] args) {
        System.out.println("Hello");
    }
}
"#;
        assert_eq!(
            extract_public_class_name(src),
            Some("HelloWorld".to_string())
        );
    }

    #[test]
    fn extracts_class_with_abstract_modifier() {
        // Even though abstract classes can't be instantiated, we still
        // pick up the name if it's the only public class.
        let src = r#"
public abstract class Base {
    public static void main(String[] args) {}
}
"#;
        assert_eq!(extract_public_class_name(src), Some("Base".to_string()));
    }

    #[test]
    fn picks_class_with_main_among_multiple() {
        let src = r#"
class Helper {
    static int add(int a, int b) { return a + b; }
}

public class Solution {
    public static void main(String[] args) {
        System.out.println(Helper.add(1, 2));
    }
}
"#;
        assert_eq!(extract_public_class_name(src), Some("Solution".to_string()));
    }

    #[test]
    fn no_public_class_falls_back_to_main() {
        let src = r#"
class Foo {
    void doStuff() {}
}

class Bar {
    public static void main(String[] args) {
        System.out.println("bar");
    }
}
"#;
        assert_eq!(extract_public_class_name(src), Some("Bar".to_string()));
    }

    #[test]
    fn returns_none_when_no_class() {
        let src = "// empty file";
        assert_eq!(extract_public_class_name(src), None);
    }

    #[test]
    fn single_file_renaming() {
        let backend = JavaBackend;
        let tmp = tempfile::tempdir().unwrap();
        let files = vec![jet_core::models::FileRequest {
            name: Some("HelloWorld.java".to_string()),
            content: r#"
public class HelloWorld {
    public static void main(String[] args) {
        System.out.println("Hello from Java!");
    }
}
"#
            .to_string(),
            encoding: None,
        }];

        let result = backend.write_files(tmp.path(), &files).unwrap();
        assert_eq!(result.primary_file, "HelloWorld.java");
        assert_eq!(result.class_name, Some("HelloWorld".to_string()));
        assert!(tmp.path().join("HelloWorld.java").exists());
    }

    #[test]
    fn renames_file_when_class_name_differs() {
        let backend = JavaBackend;
        let tmp = tempfile::tempdir().unwrap();
        let files = vec![jet_core::models::FileRequest {
            name: Some("main".to_string()), // user didn't supply correct name
            content: r#"
public class Calculator {
    public static void main(String[] args) {
        System.out.println(1 + 1);
    }
}
"#
            .to_string(),
            encoding: None,
        }];

        let result = backend.write_files(tmp.path(), &files).unwrap();
        assert_eq!(result.primary_file, "Calculator.java");
        assert_eq!(result.class_name, Some("Calculator".to_string()));
        assert!(tmp.path().join("Calculator.java").exists());
    }
}
