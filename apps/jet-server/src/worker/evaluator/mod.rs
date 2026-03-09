pub mod generic;
pub mod java;
pub mod traits;

use std::path::{Path, PathBuf};

use jet_core::models::{
    ExecutionLimits, JobRequest, JobResult, StageResult, StageStatus, TestcaseResult,
};
use jet_pack::RuntimeManifest;
use tracing::{info, warn};

use crate::sandbox::{Sandbox, SandboxProfile, SandboxResult};

use self::generic::GenericBackend;
use self::java::JavaBackend;
use self::traits::LanguageBackend;

const DEFAULT_COMPILE_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;
const DEFAULT_JVM_COMPILE_MEMORY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_JVM_RUN_MEMORY_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_COMPILE_OUTPUT_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_COMPILE_TIMEOUT_MS: u64 = 30_000;

/// Minimal C++ source used to warm up Zig's global cache inside a sandbox.
///
/// `#include <iostream>` forces Zig to decompress the bundled libc++
/// headers, which is the expensive part (~10 s on first run).
const ZIG_WARMUP_CPP: &str = r#"#include <iostream>
int main() { std::cout << "ok" << std::endl; return 0; }
"#;

/// Fallback JVM flags used when the manifest does not specify `jvm_flags`.
const DEFAULT_JAVA_COMPILE_JVM_FLAGS: &[&str] = &[
    "-Xms16m",
    "-Xmx256m",
    "-XX:MaxMetaspaceSize=64m",
    "-XX:CompressedClassSpaceSize=32m",
    "-XX:ReservedCodeCacheSize=32m",
    "-XX:+UseSerialGC",
    "-Xss256k",
];

/// Fallback JVM flags used when the manifest does not specify `jvm_flags`.
const DEFAULT_JAVA_RUN_JVM_FLAGS: &[&str] = &[
    "-Xms8m",
    "-Xmx64m",
    "-XX:MaxMetaspaceSize=32m",
    "-XX:CompressedClassSpaceSize=16m",
    "-XX:ReservedCodeCacheSize=16m",
    "-XX:+UseSerialGC",
    "-Xss256k",
];

/// Select the appropriate [`LanguageBackend`] for a given language.
fn backend_for(language: &str) -> Box<dyn LanguageBackend> {
    match language {
        "java" => Box::new(JavaBackend),
        _ => Box::new(GenericBackend),
    }
}

pub struct Evaluator {
    workspace_dir: PathBuf,
    runtime_dir: Option<PathBuf>,
    /// Pre-warmed Zig global cache directory (if applicable).
    cache_dir: Option<PathBuf>,
    manifest: RuntimeManifest,
    limits: ExecutionLimits,
}

impl Evaluator {
    pub fn new(
        workspace_dir: PathBuf,
        runtime_dir: Option<PathBuf>,
        cache_dir: Option<PathBuf>,
        manifest: RuntimeManifest,
        limits: ExecutionLimits,
    ) -> Self {
        Self {
            workspace_dir,
            runtime_dir,
            cache_dir,
            manifest,
            limits,
        }
    }

    /// Warm up the Zig global cache by compiling a trivial C++ program
    /// inside a real sandbox.  Only runs once per cache directory (tracked
    /// by a `.warmed` sentinel file).
    fn ensure_zig_cache_warm(cache_dir: &Path, runtime_dir: Option<&Path>) -> SandboxResult<()> {
        let sentinel = cache_dir.join(".warmed");
        if sentinel.exists() {
            return Ok(());
        }

        info!("warming up zig cache (first use)");

        // Build a temporary workspace for the warm-up compilation.
        let warmup_dir =
            std::env::temp_dir().join(format!("jet-zig-warmup-{}", std::process::id()));
        std::fs::create_dir_all(&warmup_dir)?;
        std::fs::write(warmup_dir.join("warmup.cpp"), ZIG_WARMUP_CPP)?;

        let limits = ExecutionLimits {
            memory_limit_bytes: DEFAULT_COMPILE_MEMORY_BYTES,
            output_limit_bytes: DEFAULT_COMPILE_OUTPUT_LIMIT_BYTES,
            timeout_ms: 120_000, // generous: first-ever decompress can be slow
            ..ExecutionLimits::default()
        };

        let mut sandbox = Sandbox::with_cache(
            &limits,
            &warmup_dir,
            runtime_dir,
            Some(cache_dir),
            &SandboxProfile::strict(),
        )?;

        let envs = vec![
            ("PATH", "/opt/runtime/bin:/usr/bin:/bin"),
            ("HOME", "/tmp"),
            ("ZIG_GLOBAL_CACHE_DIR", "/opt/zig-cache"),
            ("ZIG_LOCAL_CACHE_DIR", "/tmp/zig-local-cache"),
        ];

        let result = sandbox.run(
            "/opt/runtime/zig",
            &["c++", "warmup.cpp", "-o", "warmup_bin", "-O3"],
            Some(&envs),
            None,
            120_000,
        )?;

        // Clean up temp workspace.
        let _ = std::fs::remove_dir_all(&warmup_dir);

        if result.status == StageStatus::Success {
            // Mark cache as warm so subsequent calls skip the warm-up.
            let _ = std::fs::write(&sentinel, "warmed");
            info!(
                execution_time_ms = result.execution_time,
                "zig cache warm-up complete"
            );
        } else {
            warn!(
                stderr = %result.stderr.trim(),
                "zig cache warm-up failed (non-fatal)"
            );
        }

        Ok(())
    }

    pub fn evaluate(&self, request: &JobRequest) -> SandboxResult<JobResult> {
        let backend = backend_for(&self.manifest.language);

        // 1. Write files to workspace (language-specific)
        let write_result = backend.write_files(&self.workspace_dir, &request.files)?;
        let primary_file = &write_result.primary_file;
        let class_name = write_result
            .class_name
            .as_deref()
            .unwrap_or(primary_file.as_str());

        // Helper closure: substitute placeholders in template args.
        let substitute = |args: &[String]| -> Vec<String> {
            args.iter()
                .map(|s| {
                    s.replace("{file}", primary_file)
                        .replace("{class}", class_name)
                })
                .collect()
        };

        // 2. Compile if necessary
        let mut compile_result = None;
        if let Some(compile_template) = &self.manifest.compile {
            // Ensure the Zig global cache is warm before the first real
            // compilation so users don't pay the ~10 s decompression cost.
            if let Some(cache_dir) = &self.cache_dir {
                let _ = Self::ensure_zig_cache_warm(cache_dir, self.runtime_dir.as_deref());
            }

            let mut compile_limits = self.limits.clone();

            // Base compile-limit defaults.
            compile_limits.memory_limit_bytes = request.compile_memory_limit.unwrap_or_else(|| {
                compile_limits
                    .memory_limit_bytes
                    .max(DEFAULT_COMPILE_MEMORY_BYTES)
            });
            compile_limits.output_limit_bytes = request.compile_output_limit.unwrap_or_else(|| {
                compile_limits
                    .output_limit_bytes
                    .max(DEFAULT_COMPILE_OUTPUT_LIMIT_BYTES)
            });
            compile_limits.timeout_ms = request
                .compile_timeout
                .unwrap_or_else(|| compile_limits.timeout_ms.max(DEFAULT_COMPILE_TIMEOUT_MS));

            // Language-specific adjustments (e.g. JVM memory).
            backend.adjust_compile_limits(&mut compile_limits, &self.manifest);

            let mut sandbox = Sandbox::with_cache(
                &compile_limits,
                &self.workspace_dir,
                self.runtime_dir.as_deref(),
                self.cache_dir.as_deref(),
                &SandboxProfile::strict(),
            )?;

            let cmd = &compile_template.command;
            let template_args = compile_template
                .args
                .as_ref()
                .map(|a| substitute(a))
                .unwrap_or_default();

            let full_compile_args_owned = backend.build_compile_args(template_args, &self.manifest);
            let full_compile_args: Vec<&str> =
                full_compile_args_owned.iter().map(|s| s.as_str()).collect();

            let mut envs = vec![("PATH", "/opt/runtime/bin:/usr/bin:/bin"), ("HOME", "/tmp")];

            // Point Zig at the pre-warmed global cache (read-write inside
            // the sandbox) so it skips the ~10 s header decompression.
            // Only set the env var when a cache was actually bind-mounted.
            if self.cache_dir.is_some() {
                envs.push(("ZIG_GLOBAL_CACHE_DIR", "/opt/zig-cache"));
                envs.push(("ZIG_LOCAL_CACHE_DIR", "/tmp/zig-local-cache"));
            }

            let timeout = compile_limits.timeout_ms;

            let result = sandbox.run(cmd, &full_compile_args, Some(&envs), None, timeout)?;

            if result.status != StageStatus::Success {
                // The sandbox classifies all non-zero exits generically.
                // For the compile stage, any non-success status (that
                // isn't TLE/MLE/OLE) is a compilation error.
                let compile_status = match result.status {
                    StageStatus::RuntimeError => StageStatus::CompilationError,
                    other => other,
                };
                return Ok(JobResult {
                    language: request.language.clone(),
                    version: request.version.clone().unwrap_or_default(),
                    run: None,
                    compile: Some(StageResult {
                        status: compile_status,
                        ..result
                    }),
                    testcases: None,
                });
            }

            compile_result = Some(result);
        }

        // 3. Run testcases or single run
        let mut run_result = None;
        let mut testcase_results = None;

        let mut run_limits = self.limits.clone();
        if let Some(memory) = request.run_memory_limit {
            run_limits.memory_limit_bytes = memory;
        }
        if let Some(output_limit) = request.run_output_limit {
            run_limits.output_limit_bytes = output_limit;
        }
        if let Some(timeout) = request.run_timeout {
            run_limits.timeout_ms = timeout;
        }

        // Language-specific run-limit adjustments.
        backend.adjust_run_limits(&mut run_limits, &self.manifest);

        let cmd = &self.manifest.execute.command;
        let template_args = self
            .manifest
            .execute
            .args
            .as_ref()
            .map(|a| substitute(a))
            .unwrap_or_default();

        let full_run_args_owned = backend.build_run_args(template_args, &self.manifest);
        let full_run_args: Vec<&str> = full_run_args_owned.iter().map(|s| s.as_str()).collect();

        let envs = vec![("PATH", "/opt/runtime/bin:/usr/bin:/bin"), ("HOME", "/tmp")];
        let timeout = run_limits.timeout_ms;

        if let Some(testcases) = &request.testcases {
            let mut results = Vec::new();
            for tc in testcases {
                let mut sandbox = Sandbox::new(
                    &run_limits,
                    &self.workspace_dir,
                    self.runtime_dir.as_deref(),
                    &SandboxProfile::strict(),
                )?;
                let result =
                    sandbox.run(cmd, &full_run_args, Some(&envs), Some(&tc.input), timeout)?;

                let passed = if result.status == StageStatus::Success {
                    if let Some(expected) = &tc.expected_output {
                        result.stdout.trim() == expected.trim()
                    } else {
                        true
                    }
                } else {
                    false
                };

                results.push(TestcaseResult {
                    id: tc.id.clone(),
                    passed,
                    actual_output: result.stdout.clone(),
                    run_details: result,
                });
            }
            testcase_results = Some(results);
        } else {
            let mut sandbox = Sandbox::new(
                &run_limits,
                &self.workspace_dir,
                self.runtime_dir.as_deref(),
                &SandboxProfile::strict(),
            )?;
            let result = sandbox.run(
                cmd,
                &full_run_args,
                Some(&envs),
                request.stdin.as_deref(),
                timeout,
            )?;
            run_result = Some(result);
        }

        Ok(JobResult {
            language: request.language.clone(),
            version: request.version.clone().unwrap_or_default(),
            run: run_result,
            compile: compile_result,
            testcases: testcase_results,
        })
    }
}
