use std::fs;
use std::path::PathBuf;

use jet_core::models::{ExecutionLimits, JobRequest, JobResult, StageStatus, TestcaseResult};
use jet_pack::RuntimeManifest;

use crate::sandbox::{Sandbox, SandboxProfile, SandboxResult};

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
        // 1. Write files to workspace
        for file in &request.files {
            let name = file.name.as_deref().unwrap_or("main");
            let path = self.workspace_dir.join(name);
            fs::write(&path, &file.content)?;
        }

        // 2. Compile if necessary
        let mut compile_result = None;
        if let Some(compile_template) = &self.manifest.compile {
            let mut sandbox = Sandbox::new(
                &self.limits,
                &self.workspace_dir,
                self.runtime_dir.as_deref(),
                &SandboxProfile::strict(),
            )?;
            
            let cmd = &compile_template.command;
            let args: Vec<&str> = compile_template
                .args
                .as_ref()
                .map(|a| a.iter().map(|s| s.as_str()).collect())
                .unwrap_or_default();

            let envs = vec![("PATH", "/opt/runtime/bin:/usr/bin:/bin")];
            
            let timeout = request.compile_timeout.unwrap_or(self.limits.timeout_ms);
            
            let result = sandbox.run(cmd, &args, Some(&envs), None, timeout)?;
            
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

        let cmd = &self.manifest.execute.command;
        let args: Vec<&str> = self.manifest.execute
            .args
            .as_ref()
            .map(|a| a.iter().map(|s| s.as_str()).collect())
            .unwrap_or_default();
            
        let envs = vec![("PATH", "/opt/runtime/bin:/usr/bin:/bin")];
        let timeout = request.run_timeout.unwrap_or(self.limits.timeout_ms);

        if let Some(testcases) = &request.testcases {
            let mut results = Vec::new();
            for tc in testcases {
                let mut sandbox = Sandbox::new(
                    &self.limits,
                    &self.workspace_dir,
                    self.runtime_dir.as_deref(),
                    &SandboxProfile::strict(),
                )?;
                let result = sandbox.run(cmd, &args, Some(&envs), Some(&tc.input), timeout)?;
                
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
                &self.limits,
                &self.workspace_dir,
                self.runtime_dir.as_deref(),
                &SandboxProfile::strict(),
            )?;
            let result = sandbox.run(cmd, &args, Some(&envs), request.stdin.as_deref(), timeout)?;
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
