use std::fs;
use std::path::PathBuf;

use jet_core::models::{ExecutionLimits, JobRequest, JobResult, StageStatus, TestcaseResult};
use jet_pack::RuntimeManifest;

use crate::sandbox::{Sandbox, SandboxProfile, SandboxResult};

const DEFAULT_COMPILE_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;
const DEFAULT_JVM_COMPILE_MEMORY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_JVM_RUN_MEMORY_BYTES: u64 = 512 * 1024 * 1024;

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

pub struct Evaluator {
    workspace_dir: PathBuf,
    runtime_dir: Option<PathBuf>,
    manifest: RuntimeManifest,
    limits: ExecutionLimits,
}

impl Evaluator {
    pub fn new(
        workspace_dir: PathBuf,
        runtime_dir: Option<PathBuf>,
        manifest: RuntimeManifest,
        limits: ExecutionLimits,
    ) -> Self {
        Self {
            workspace_dir,
            runtime_dir,
            manifest,
            limits,
        }
    }

    pub fn evaluate(&self, request: &JobRequest) -> SandboxResult<JobResult> {
        let primary_file = request
            .files
            .first()
            .and_then(|f| f.name.clone())
            .unwrap_or_else(|| "main".to_string());

        // 1. Write files to workspace
        for file in &request.files {
            let name = file.name.as_deref().unwrap_or("main");
            let path = self.workspace_dir.join(name);
            fs::write(&path, &file.content)?;
        }

        // 2. Compile if necessary
        let mut compile_result = None;
        if let Some(compile_template) = &self.manifest.compile {
            let mut compile_limits = self.limits.clone();
            let default_compile_mem = if self.manifest.language == "java" {
                DEFAULT_JVM_COMPILE_MEMORY_BYTES
            } else {
                DEFAULT_COMPILE_MEMORY_BYTES
            };
            compile_limits.memory_limit_bytes = request
                .compile_memory_limit
                .unwrap_or_else(|| compile_limits.memory_limit_bytes.max(default_compile_mem));
            if let Some(output_limit) = request.compile_output_limit {
                compile_limits.output_limit_bytes = output_limit;
            }
            if let Some(timeout) = request.compile_timeout {
                compile_limits.timeout_ms = timeout;
            }

            let mut sandbox = Sandbox::new(
                &compile_limits,
                &self.workspace_dir,
                self.runtime_dir.as_deref(),
                &SandboxProfile::strict(),
            )?;

            let cmd = &compile_template.command;
            let compile_args_owned: Vec<String> = compile_template
                .args
                .as_ref()
                .map(|a| {
                    a.iter()
                        .map(|s| s.replace("{file}", &primary_file))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let compile_args: Vec<&str> = compile_args_owned.iter().map(|s| s.as_str()).collect();

            // For javac, JVM flags must be prefixed with -J to avoid the
            // "Picked up JAVA_TOOL_OPTIONS" stderr noise.
            let java_compile_flags: Vec<String> = if self.manifest.language == "java" {
                let flags: Vec<&str> = compile_template
                    .jvm_flags
                    .as_ref()
                    .map(|v| v.iter().map(|s| s.as_str()).collect())
                    .unwrap_or_else(|| DEFAULT_JAVA_COMPILE_JVM_FLAGS.to_vec());
                flags.iter().map(|f| format!("-J{f}")).collect()
            } else {
                Vec::new()
            };
            let mut full_compile_args: Vec<&str> =
                java_compile_flags.iter().map(|s| s.as_str()).collect();
            full_compile_args.extend_from_slice(&compile_args);

            let envs = vec![("PATH", "/opt/runtime/bin:/usr/bin:/bin")];

            let timeout = compile_limits.timeout_ms;

            let result = sandbox.run(cmd, &full_compile_args, Some(&envs), None, timeout)?;

            if result.status != StageStatus::Success {
                return Ok(JobResult {
                    language: request.language.clone(),
                    version: request.version.clone().unwrap_or_default(),
                    run: None,
                    compile: Some(result),
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

        // JVM needs at least ~512 MB of virtual address space to start.
        if self.manifest.language == "java" {
            run_limits.memory_limit_bytes = run_limits
                .memory_limit_bytes
                .max(DEFAULT_JVM_RUN_MEMORY_BYTES);
        }

        let cmd = &self.manifest.execute.command;
        let run_args_owned: Vec<String> = self
            .manifest
            .execute
            .args
            .as_ref()
            .map(|a| {
                a.iter()
                    .map(|s| s.replace("{file}", &primary_file))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let run_args: Vec<&str> = run_args_owned.iter().map(|s| s.as_str()).collect();

        // For java, pass JVM flags as direct CLI args (before the class name)
        // to avoid the "Picked up JAVA_TOOL_OPTIONS" stderr noise.
        let run_jvm_flags_owned: Vec<String> = if self.manifest.language == "java" {
            self.manifest
                .execute
                .jvm_flags
                .as_ref()
                .cloned()
                .unwrap_or_else(|| {
                    DEFAULT_JAVA_RUN_JVM_FLAGS
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
                })
        } else {
            Vec::new()
        };
        let mut full_run_args: Vec<&str> = run_jvm_flags_owned.iter().map(|s| s.as_str()).collect();
        full_run_args.extend_from_slice(&run_args);

        let envs = vec![("PATH", "/opt/runtime/bin:/usr/bin:/bin")];
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
