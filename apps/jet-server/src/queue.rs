use jet_core::models::{JobRequest, JobResult};

/// Whether a job requires compilation before execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum JobType {
    /// Language has a compile step (e.g. C++, Java, Rust).
    Compile,
    /// Interpreted-only language (e.g. Python, JavaScript).
    Execute,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QueuedJob {
    pub id: String,
    pub language: String,
    pub version: String,
    pub request: JobRequest,
    /// When the job was enqueued (Unix timestamp in milliseconds).
    pub enqueued_at: u64,
    /// Whether this job requires compilation.
    pub job_type: JobType,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JobStateRecord {
    pub job_id: String,
    pub status: String,
    pub language: String,
    pub version: String,
    pub result: Option<JobResult>,
    pub error: Option<String>,
    /// How long the job waited in the queue before a worker picked it up (ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_wait_ms: Option<u64>,
}

pub fn job_state_key(prefix: &str, job_id: &str) -> String {
    format!("{}:{}", prefix, job_id)
}

#[cfg(test)]
mod tests {
    use super::job_state_key;

    #[test]
    fn builds_job_state_key_with_prefix() {
        let key = job_state_key("jet:jobs", "job-123");
        assert_eq!(key, "jet:jobs:job-123");
    }
}
