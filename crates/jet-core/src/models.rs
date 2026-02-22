use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRequest {
    pub job_id: Option<String>,
    pub language: String,
    pub version: Option<String>,
    pub files: Vec<FileRequest>,
    pub testcases: Option<Vec<Testcase>>,
    pub args: Option<Vec<String>>,
    pub stdin: Option<String>,
    pub run_timeout: Option<u64>,
    pub compile_timeout: Option<u64>,
    pub run_memory_limit: Option<u64>,
    pub compile_memory_limit: Option<u64>,
    pub run_output_limit: Option<u64>,
    pub compile_output_limit: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub request: JobRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRequest {
    pub name: Option<String>,
    pub content: String,
    pub encoding: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Testcase {
    pub id: String,
    pub input: String,
    pub expected_output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    pub language: String,
    pub version: String,
    pub run: Option<StageResult>,
    pub compile: Option<StageResult>,
    pub testcases: Option<Vec<TestcaseResult>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionLimits {
    pub memory_limit_bytes: u64,
    pub pid_limit: u64,
    pub file_limit: u64,
    pub timeout_ms: u64,
    pub output_limit_bytes: u64,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            memory_limit_bytes: 512 * 1024 * 1024,
            pid_limit: 256,
            file_limit: 2048,
            timeout_ms: 3000,
            output_limit_bytes: 1024 * 1024,
            uid: None,
            gid: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StageStatus {
    Pending,
    Running,
    Success,
    RuntimeError,
    CompilationError,
    TimeLimitExceeded,
    MemoryLimitExceeded,
    OutputLimitExceeded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageResult {
    pub status: StageStatus,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub memory_usage: Option<u64>,
    pub cpu_time: Option<u64>,
    pub execution_time: Option<u64>,
}

impl std::fmt::Display for StageResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Status: {:?}", self.status)?;
        if let Some(code) = self.exit_code {
            writeln!(f, "Exit Code: {code}")?;
        }
        if let Some(signal) = &self.signal {
            writeln!(f, "Signal: {signal}")?;
        }

        if let Some(mem) = self.memory_usage {
            let (value, unit) = if mem > 1024 * 1024 * 1024 {
                (mem as f64 / 1024.0 / 1024.0 / 1024.0, "GB")
            } else if mem > 1024 * 1024 {
                (mem as f64 / 1024.0 / 1024.0, "MB")
            } else if mem > 1024 {
                (mem as f64 / 1024.0, "KB")
            } else {
                (mem as f64, "B")
            };
            writeln!(f, "Memory Usage: {value:.2} {unit}")?;
        }

        if let Some(cpu) = self.cpu_time {
            let (value, unit) = if cpu > 1_000_000 {
                (cpu as f64 / 1_000_000.0, "s")
            } else if cpu > 1_000 {
                (cpu as f64 / 1_000.0, "ms")
            } else {
                (cpu as f64, "µs")
            };
            writeln!(f, "CPU Time: {value:.2} {unit}")?;
        }

        if let Some(execution_time) = self.execution_time {
            let (value, unit) = if execution_time > 1_000 {
                (execution_time as f64 / 1_000.0, "s")
            } else {
                (execution_time as f64, "ms")
            };
            writeln!(f, "Execution Time: {value:.2} {unit}")?;
        }

        if !self.stdout.is_empty() {
            writeln!(f, "Stdout:\n{}", self.stdout)?;
        }
        if !self.stderr.is_empty() {
            writeln!(f, "Stderr:\n{}", self.stderr)?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestcaseResult {
    pub id: String,
    pub passed: bool,
    pub actual_output: String,
    pub run_details: StageResult,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_limits_default_matches_phase_one_requirements() {
        let limits = ExecutionLimits::default();

        assert_eq!(limits.memory_limit_bytes, 512 * 1024 * 1024);
        assert_eq!(limits.timeout_ms, 3000);
        assert_eq!(limits.output_limit_bytes, 1024 * 1024);
    }

    #[test]
    fn stage_status_serializes_to_screaming_snake_case() {
        let status = StageStatus::TimeLimitExceeded;
        let json = serde_json::to_string(&status).expect("status should serialize");

        assert_eq!(json, "\"TIME_LIMIT_EXCEEDED\"");
    }

    #[test]
    fn stage_status_rejects_unknown_values() {
        let parsed = serde_json::from_str::<StageStatus>("\"SOME_NEW_STATUS\"");
        assert!(parsed.is_err());
    }
}
