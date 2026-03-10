use std::sync::atomic::{AtomicU64, Ordering};
use std::{collections::HashMap, path::PathBuf, sync::Arc};

use apalis::prelude::*;
use apalis_redis::RedisStorage;
use deadpool_redis::{Pool, redis::AsyncCommands};
use jet_core::models::ExecutionLimits;
use jet_core::models::{JobResult, StageStatus};
use jet_pack::manifest::RuntimeManifest;
use tokio::fs;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

use crate::{
    counters::saturating_decrement,
    path_safety::build_job_workspace_path,
    queue::{JobStateRecord, JobType, QueuedJob, job_state_key},
    worker::supervisor::{ChildEvalPayload, run_supervised_job},
};

#[derive(Clone)]
pub struct WorkerContext {
    pub manifests: Arc<HashMap<String, RuntimeManifest>>,
    pub runtime_install_dir: PathBuf,
    pub redis_pool: Pool,
    pub job_state_prefix: String,
    pub jobs_completed: Arc<AtomicU64>,
    pub jobs_failed: Arc<AtomicU64>,
    pub jobs_in_flight: Arc<AtomicU64>,
    /// Semaphore that gates compilation jobs (heavy, multi-threaded).
    pub compile_semaphore: Arc<Semaphore>,
    /// Semaphore that gates execution jobs (lightweight).
    pub execute_semaphore: Arc<Semaphore>,
    /// Per-category in-flight counters for observability.
    pub compile_in_flight: Arc<AtomicU64>,
    pub execute_in_flight: Arc<AtomicU64>,
    /// Maximum time (ms) a job may wait in the queue before being shed.
    pub max_queue_wait_ms: u64,
}

pub async fn run_worker(
    name: String,
    redis_url: String,
    context: WorkerContext,
    concurrency: usize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let conn = apalis_redis::connect(redis_url).await?;
    let storage = RedisStorage::<QueuedJob>::new(conn);

    let worker = WorkerBuilder::new(&name)
        .backend(storage)
        .concurrency(concurrency)
        .data(context)
        .build(handle_job);

    worker.run().await?;
    Ok(())
}

async fn handle_job(job: QueuedJob, data: Data<WorkerContext>) -> Result<(), std::io::Error> {
    // ── Queue-time shedding ───────────────────────────────────────
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let queue_wait_ms = now_ms.saturating_sub(job.enqueued_at);

    if queue_wait_ms > data.max_queue_wait_ms {
        warn!(
            job_id = %job.id,
            language = %job.language,
            queue_wait_ms = queue_wait_ms,
            max_wait_ms = data.max_queue_wait_ms,
            "shedding stale job: exceeded max queue wait time"
        );
        data.jobs_failed.fetch_add(1, Ordering::Relaxed);
        saturating_decrement(&data.jobs_in_flight);
        write_job_state(
            &data.redis_pool,
            &data.job_state_prefix,
            JobStateRecord {
                job_id: job.id.clone(),
                status: "queue_timeout".to_string(),
                language: job.language.clone(),
                version: job.version.clone(),
                result: None,
                error: Some(format!(
                    "job waited {}ms in queue (max: {}ms)",
                    queue_wait_ms, data.max_queue_wait_ms
                )),
                terminal_reason: Some("queue_timeout".to_string()),
                queue_wait_ms: Some(queue_wait_ms),
            },
        )
        .await?;
        return Ok(());
    }

    // ── Acquire category semaphore ────────────────────────────────
    let (semaphore, category_counter, category_name) = match job.job_type {
        JobType::Compile => (
            data.compile_semaphore.clone(),
            data.compile_in_flight.clone(),
            "compile",
        ),
        JobType::Execute => (
            data.execute_semaphore.clone(),
            data.execute_in_flight.clone(),
            "execute",
        ),
    };

    let _permit = semaphore
        .acquire()
        .await
        .map_err(|e| std::io::Error::other(format!("semaphore closed for {category_name}: {e}")))?;
    category_counter.fetch_add(1, Ordering::Relaxed);

    info!(
        job_id = %job.id,
        language = %job.language,
        version = %job.version,
        job_type = category_name,
        queue_wait_ms = queue_wait_ms,
        "job starting"
    );

    write_job_state(
        &data.redis_pool,
        &data.job_state_prefix,
        JobStateRecord {
            job_id: job.id.clone(),
            status: "running".to_string(),
            language: job.language.clone(),
            version: job.version.clone(),
            result: None,
            error: None,
            terminal_reason: None,
            queue_wait_ms: Some(queue_wait_ms),
        },
    )
    .await?;

    let start = std::time::Instant::now();
    let result = process_job(&job, &data).await;
    let elapsed = start.elapsed();

    // Release category counter (semaphore permit drops automatically).
    saturating_decrement(&category_counter);

    match result {
        Ok(job_result) => {
            info!(
                job_id = %job.id,
                language = %job.language,
                job_type = category_name,
                duration_ms = elapsed.as_millis() as u64,
                "job completed successfully"
            );
            data.jobs_completed.fetch_add(1, Ordering::Relaxed);
            saturating_decrement(&data.jobs_in_flight);
            write_job_state(
                &data.redis_pool,
                &data.job_state_prefix,
                JobStateRecord {
                    job_id: job.id.clone(),
                    status: "completed".to_string(),
                    language: job.language.clone(),
                    version: job.version.clone(),
                    result: Some(job_result.clone()),
                    error: None,
                    terminal_reason: Some(terminal_reason_from_result(&job_result).to_string()),
                    queue_wait_ms: Some(queue_wait_ms),
                },
            )
            .await?;
        }
        Err(source) => {
            error!(
                job_id = %job.id,
                language = %job.language,
                job_type = category_name,
                error = %source,
                duration_ms = elapsed.as_millis() as u64,
                "job failed"
            );
            data.jobs_failed.fetch_add(1, Ordering::Relaxed);
            saturating_decrement(&data.jobs_in_flight);
            write_job_state(
                &data.redis_pool,
                &data.job_state_prefix,
                JobStateRecord {
                    job_id: job.id.clone(),
                    status: "failed".to_string(),
                    language: job.language.clone(),
                    version: job.version.clone(),
                    result: None,
                    error: Some(source.to_string()),
                    terminal_reason: Some(classify_failure_reason(&source).to_string()),
                    queue_wait_ms: Some(queue_wait_ms),
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

    // Zig-based runtimes (c, cpp, zig) may have a pre-warmed global cache
    // that avoids the ~10 s header-decompression penalty inside the sandbox.
    let zig_cache_dir = jet_pack::manager::zig_cache_dir_for(&runtime_root_dir);

    let jobs_root = data.runtime_install_dir.join("jobs");
    let workspace_dir = build_job_workspace_path(&jobs_root, &job.id)
        .map_err(|source| std::io::Error::other(source.to_string()))?;
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

    // ── Wall-clock safety net ─────────────────────────────────────
    //
    // Compute a generous upper bound: compile + (run × testcases) + buffer.
    // This prevents a misbehaving sandbox from blocking a worker thread
    // indefinitely (which would eventually starve the Redis consumer).
    let num_testcases = job
        .request
        .testcases
        .as_ref()
        .map(|t| t.len() as u64)
        .unwrap_or(1);
    let compile_ms = job.request.compile_timeout.unwrap_or(30_000);
    let run_ms = job.request.run_timeout.unwrap_or(limits.timeout_ms);
    let wall_clock_limit =
        std::time::Duration::from_millis(compile_ms + run_ms * num_testcases + 60_000);

    // Run each job in a supervised child process so wall-clock timeouts can
    // terminate the actual process tree, not just the async future.
    let result = run_supervised_job(
        ChildEvalPayload {
            request: job.request.clone(),
            workspace_dir: workspace_dir.clone(),
            runtime_root_dir,
            zig_cache_dir,
            manifest,
            limits,
        },
        wall_clock_limit,
    )
    .await
    .map_err(|source| {
        if source.kind() == std::io::ErrorKind::TimedOut {
            warn!(
                job_id = %job.id,
                wall_clock_limit_ms = wall_clock_limit.as_millis() as u64,
                error = %source,
                "job exceeded wall-clock limit and child process group was killed"
            );
        }
        source
    });

    // Always clean up workspace, regardless of success or failure.
    cleanup_workspace(&jobs_root, &workspace_dir, &job.id).await;

    result
}

async fn cleanup_workspace(
    jobs_root: &std::path::Path,
    workspace_dir: &std::path::Path,
    job_id: &str,
) {
    let canonical_jobs_root = match tokio::fs::canonicalize(jobs_root).await {
        Ok(path) => path,
        Err(e) => {
            warn!(job_id = %job_id, error = %e, "failed to resolve jobs root for cleanup");
            return;
        }
    };

    let canonical_workspace = match tokio::fs::canonicalize(workspace_dir).await {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            warn!(job_id = %job_id, error = %e, "failed to resolve workspace for cleanup");
            return;
        }
    };

    if !canonical_workspace.starts_with(&canonical_jobs_root) {
        warn!(
            job_id = %job_id,
            workspace = %canonical_workspace.display(),
            jobs_root = %canonical_jobs_root.display(),
            "refusing to cleanup workspace outside configured jobs root"
        );
        return;
    }

    if let Err(e) = fs::remove_dir_all(&canonical_workspace).await {
        // Ignore NotFound — the workspace may never have been created
        // (e.g. manifest validation failed before create_dir_all).
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(
                job_id = %job_id,
                error = %e,
                "failed to cleanup workspace"
            );
        }
    }
}

fn normalize_arch(arch: &str) -> &str {
    match arch {
        "amd64" => "x86_64",
        "arm64" => "aarch64",
        _ => arch,
    }
}

fn classify_failure_reason(error: &std::io::Error) -> &'static str {
    if error.kind() == std::io::ErrorKind::TimedOut {
        return "timeout";
    }

    if error.kind() == std::io::ErrorKind::BrokenPipe
        || error.kind() == std::io::ErrorKind::Interrupted
    {
        return "worker_lost";
    }

    "execution_error"
}

fn terminal_reason_from_result(result: &JobResult) -> &'static str {
    if is_stage_status(&result.compile, StageStatus::TimeLimitExceeded) {
        return "compile_timeout";
    }
    if is_stage_status(&result.run, StageStatus::TimeLimitExceeded)
        || has_testcase_stage_status(result, StageStatus::TimeLimitExceeded)
    {
        return "run_timeout";
    }

    if is_stage_status(&result.compile, StageStatus::MemoryLimitExceeded) {
        return "compile_oom";
    }
    if is_stage_status(&result.run, StageStatus::MemoryLimitExceeded)
        || has_testcase_stage_status(result, StageStatus::MemoryLimitExceeded)
    {
        return "run_oom";
    }

    if is_stage_status(&result.compile, StageStatus::OutputLimitExceeded) {
        return "compile_output_limit";
    }
    if is_stage_status(&result.run, StageStatus::OutputLimitExceeded)
        || has_testcase_stage_status(result, StageStatus::OutputLimitExceeded)
    {
        return "run_output_limit";
    }

    if is_stage_status(&result.compile, StageStatus::CompilationError) {
        return "compilation_error";
    }
    if is_stage_status(&result.run, StageStatus::RuntimeError)
        || has_testcase_stage_status(result, StageStatus::RuntimeError)
    {
        return "runtime_error";
    }

    if let Some(testcases) = &result.testcases {
        if testcases.iter().any(|tc| !tc.passed) {
            return "wrong_answer";
        }
    }

    "success"
}

fn is_stage_status(stage: &Option<jet_core::models::StageResult>, status: StageStatus) -> bool {
    stage.as_ref().map(|s| s.status == status).unwrap_or(false)
}

fn has_testcase_stage_status(result: &JobResult, status: StageStatus) -> bool {
    result
        .testcases
        .as_ref()
        .map(|cases| cases.iter().any(|tc| tc.run_details.status == status))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use jet_core::models::{JobResult, StageResult, StageStatus, TestcaseResult};

    use super::{classify_failure_reason, normalize_arch};
    use crate::counters::saturating_decrement;

    #[test]
    fn normalizes_common_arch_aliases() {
        assert_eq!(normalize_arch("amd64"), "x86_64");
        assert_eq!(normalize_arch("arm64"), "aarch64");
        assert_eq!(normalize_arch("x86_64"), "x86_64");
    }

    #[test]
    fn saturating_decrement_never_underflows() {
        let counter = AtomicU64::new(0);

        saturating_decrement(&counter);
        assert_eq!(counter.load(Ordering::Relaxed), 0);

        counter.store(2, Ordering::Relaxed);
        saturating_decrement(&counter);
        saturating_decrement(&counter);
        saturating_decrement(&counter);

        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn classifies_timeout_failures() {
        let err = std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out");
        assert_eq!(classify_failure_reason(&err), "timeout");
    }

    #[test]
    fn classifies_broken_pipe_as_worker_lost() {
        let err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "child output missing");
        assert_eq!(classify_failure_reason(&err), "worker_lost");
    }

    #[test]
    fn classifies_interrupted_as_worker_lost() {
        let err = std::io::Error::new(std::io::ErrorKind::Interrupted, "worker cancelled");
        assert_eq!(classify_failure_reason(&err), "worker_lost");
    }

    #[test]
    fn derives_compile_timeout_terminal_reason() {
        let result = JobResult {
            language: "rust".to_string(),
            version: "1.76.0".to_string(),
            run: None,
            compile: Some(stage(StageStatus::TimeLimitExceeded)),
            testcases: None,
        };

        assert_eq!(super::terminal_reason_from_result(&result), "compile_timeout");
    }

    #[test]
    fn derives_run_timeout_terminal_reason_from_testcases() {
        let result = JobResult {
            language: "python".to_string(),
            version: "3.14.3".to_string(),
            run: None,
            compile: Some(stage(StageStatus::Success)),
            testcases: Some(vec![TestcaseResult {
                id: "tc-1".to_string(),
                passed: false,
                actual_output: String::new(),
                run_details: stage(StageStatus::TimeLimitExceeded),
            }]),
        };

        assert_eq!(super::terminal_reason_from_result(&result), "run_timeout");
    }

    fn stage(status: StageStatus) -> StageResult {
        StageResult {
            status,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            signal: None,
            memory_usage: None,
            cpu_time: None,
            execution_time: None,
        }
    }
}

/// TTL for terminal job states (completed / failed) so they don't
/// accumulate in Redis forever.
const JOB_STATE_TTL_SECS: u64 = 3600; // 1 hour

async fn write_job_state(
    pool: &Pool,
    prefix: &str,
    state: JobStateRecord,
) -> Result<(), std::io::Error> {
    let key = job_state_key(prefix, &state.job_id);
    let payload = serde_json::to_string(&state)
        .map_err(|source| std::io::Error::other(source.to_string()))?;
    let is_terminal =
        state.status == "completed" || state.status == "failed" || state.status == "queue_timeout";
    let mut conn = pool
        .get()
        .await
        .map_err(|source| std::io::Error::other(source.to_string()))?;
    let _: () = conn
        .set::<_, _, ()>(&key, &payload)
        .await
        .map_err(|source| std::io::Error::other(source.to_string()))?;
    if is_terminal {
        let _: () = conn
            .expire::<_, ()>(&key, JOB_STATE_TTL_SECS as i64)
            .await
            .map_err(|source| std::io::Error::other(source.to_string()))?;
    }
    Ok(())
}
