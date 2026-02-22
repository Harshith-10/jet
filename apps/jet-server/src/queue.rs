use jet_core::models::{JobRequest, JobResult};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QueuedJob {
    pub id: String,
    pub language: String,
    pub version: String,
    pub request: JobRequest,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JobStateRecord {
    pub job_id: String,
    pub status: String,
    pub language: String,
    pub version: String,
    pub result: Option<JobResult>,
    pub error: Option<String>,
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
