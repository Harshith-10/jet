use std::{collections::HashMap, path::PathBuf, sync::Arc};

use apalis::prelude::*;
use apalis_redis::RedisStorage;
use jet_core::models::ExecutionLimits;
use jet_pack::manifest::RuntimeManifest;
use redis::{AsyncCommands, aio::ConnectionManager};
use tokio::fs;
use tokio::sync::Mutex;

use crate::{
    queue::{JobStateRecord, QueuedJob, job_state_key},
    worker::evaluator::Evaluator,
};

#[derive(Clone)]
pub struct WorkerContext {
    pub manifests: Arc<HashMap<String, RuntimeManifest>>,
    pub runtime_install_dir: PathBuf,
    pub redis_conn: Arc<Mutex<ConnectionManager>>,
    pub job_state_prefix: String,
}

pub async fn run_worker(
    redis_url: String,
    context: WorkerContext,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let conn = apalis_redis::connect(redis_url).await?;
    let storage = RedisStorage::<QueuedJob>::new(conn);

    let worker = WorkerBuilder::new("jet-worker")
        .data(context)
        .backend(storage)
        .build_fn(handle_job);

    worker.run().await;
    Ok(())
}

async fn handle_job(job: QueuedJob, data: Data<WorkerContext>) -> Result<(), std::io::Error> {
    write_job_state(
        &data.redis_conn,
        &data.job_state_prefix,
        JobStateRecord {
            job_id: job.id.clone(),
            status: "running".to_string(),
            language: job.language.clone(),
            version: job.version.clone(),
            result: None,
            error: None,
        },
    )
    .await?;

    let result = process_job(&job, &data).await;
    match result {
        Ok(job_result) => {
            write_job_state(
                &data.redis_conn,
                &data.job_state_prefix,
                JobStateRecord {
                    job_id: job.id.clone(),
                    status: "completed".to_string(),
                    language: job.language.clone(),
                    version: job.version.clone(),
                    result: Some(job_result.clone()),
                    error: None,
                },
            )
            .await?;
        }
        Err(source) => {
            write_job_state(
                &data.redis_conn,
                &data.job_state_prefix,
                JobStateRecord {
                    job_id: job.id.clone(),
                    status: "failed".to_string(),
                    language: job.language.clone(),
                    version: job.version.clone(),
                    result: None,
                    error: Some(source.to_string()),
                },
            )
            .await?;
            return Err(source);
        }
    }

    Ok(())
}

async fn process_job(
    job: &QueuedJob,
    data: &WorkerContext,
) -> Result<jet_core::models::JobResult, std::io::Error> {
    let key = format!("{}:{}", job.language, job.version);
    let manifest = data
        .manifests
        .get(&key)
        .cloned()
        .ok_or_else(|| std::io::Error::other("manifest missing"))?;

    let host_arch = normalize_arch(std::env::consts::ARCH);
    if !manifest.runtimes.contains_key(host_arch) {
        return Err(std::io::Error::other(format!(
            "runtime archive for architecture '{}' is missing in manifest",
            host_arch
        )));
    }

    let runtime_root_dir = data
        .runtime_install_dir
        .join(&job.language)
        .join(&job.version)
        .join("root");
    if !runtime_root_dir.exists() {
        return Err(std::io::Error::other(format!(
            "runtime is not installed at {}",
            runtime_root_dir.display()
        )));
    }

    let workspace_dir = data.runtime_install_dir.join("jobs").join(&job.id);
    fs::create_dir_all(&workspace_dir)
        .await
        .map_err(|source| std::io::Error::other(source.to_string()))?;

    let mut limits = ExecutionLimits::default();
    if let Some(memory) = job.request.run_memory_limit {
        limits.memory_limit_bytes = memory;
    }
    if let Some(timeout) = job.request.run_timeout {
        limits.timeout_ms = timeout;
    }
    if let Some(output_limit) = job.request.run_output_limit {
        limits.output_limit_bytes = output_limit;
    }

    let evaluator = Evaluator::new(workspace_dir, Some(runtime_root_dir), manifest, limits);
    evaluator
        .evaluate(&job.request)
        .map_err(|source| std::io::Error::other(source.to_string()))
}

fn normalize_arch(arch: &str) -> &str {
    match arch {
        "amd64" => "x86_64",
        "arm64" => "aarch64",
        _ => arch,
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_arch;

    #[test]
    fn normalizes_common_arch_aliases() {
        assert_eq!(normalize_arch("amd64"), "x86_64");
        assert_eq!(normalize_arch("arm64"), "aarch64");
        assert_eq!(normalize_arch("x86_64"), "x86_64");
    }
}


async fn write_job_state(
    redis_conn: &Arc<Mutex<ConnectionManager>>,
    prefix: &str,
    state: JobStateRecord,
) -> Result<(), std::io::Error> {
    let key = job_state_key(prefix, &state.job_id);
    let payload = serde_json::to_string(&state)
        .map_err(|source| std::io::Error::other(source.to_string()))?;
    let _: () = redis_conn
        .lock()
        .await
        .set::<String, String, ()>(key, payload)
        .await
        .map_err(|source| std::io::Error::other(source.to_string()))?;
    Ok(())
}
