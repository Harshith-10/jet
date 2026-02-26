use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use apalis::prelude::TaskSink;
use apalis_redis::RedisStorage;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use deadpool_redis::{Pool, redis::AsyncCommands};
use jet_core::models::JobRequest;
use jet_pack::{RedisVersionStore, VersionResolver, manifest::RuntimeManifest};
use serde::Serialize;
use tower_governor::{
    GovernorLayer, governor::GovernorConfigBuilder, key_extractor::SmartIpKeyExtractor,
};
use tracing::{info, warn};
use uuid::Uuid;

use crate::queue::{JobStateRecord, QueuedJob, job_state_key};

pub type SharedResolver = Arc<VersionResolver<RedisVersionStore>>;
pub type SharedStorage = Arc<tokio::sync::Mutex<RedisStorage<QueuedJob>>>;
pub type SharedManifests = Arc<HashMap<String, RuntimeManifest>>;

#[derive(Clone)]
pub struct ApiState {
    pub resolver: SharedResolver,
    pub manifests: SharedManifests,
    pub storage: SharedStorage,
    pub redis_pool: Pool,
    pub job_state_prefix: String,
    pub start_time: Instant,
    pub jobs_submitted: Arc<AtomicU64>,
    pub jobs_completed: Arc<AtomicU64>,
    pub jobs_failed: Arc<AtomicU64>,
    pub worker_concurrency: usize,
    pub jobs_in_flight: Arc<AtomicU64>,
    pub max_queue_depth: u64,
    /// Per-category in-flight counters for MLP stats.
    pub compile_in_flight: Arc<AtomicU64>,
    pub execute_in_flight: Arc<AtomicU64>,
    /// Concurrency limits for stats reporting.
    pub compile_concurrency: usize,
    pub execute_concurrency: usize,
    /// Max queue wait time before shedding (seconds).
    pub max_queue_wait_secs: u64,
}

const MAX_FILES: usize = 10;
const MAX_TESTCASES: usize = 1000;
const MAX_TOTAL_FILE_SIZE: usize = 5 * 1024 * 1024; // 5 MB

/// TTL for job state keys set from the API side.
const JOB_STATE_TTL_SECS: i64 = 3600; // 1 hour

fn validate_job_request(req: &JobRequest) -> Result<(), (StatusCode, String)> {
    if req.files.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "at least one file is required".to_string(),
        ));
    }

    if req.files.len() > MAX_FILES {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("too many files: {} (max: {})", req.files.len(), MAX_FILES),
        ));
    }

    let total_size: usize = req.files.iter().map(|f| f.content.len()).sum();
    if total_size > MAX_TOTAL_FILE_SIZE {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "total file size too large: {} bytes (max: {})",
                total_size, MAX_TOTAL_FILE_SIZE
            ),
        ));
    }

    if let Some(testcases) = &req.testcases {
        if testcases.len() > MAX_TESTCASES {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "too many testcases: {} (max: {})",
                    testcases.len(),
                    MAX_TESTCASES
                ),
            ));
        }

        for (i, tc) in testcases.iter().enumerate() {
            if tc.input.len() > MAX_TOTAL_FILE_SIZE / 10 {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("testcase {} input too large", i),
                ));
            }
        }
    }

    Ok(())
}

#[derive(Serialize)]
pub struct SubmitJobResponse {
    pub job_id: String,
    pub status: String,
    pub resolved_version: String,
}

#[derive(Serialize)]
pub struct RuntimeInfo {
    pub version: String,
    pub aliases: Vec<String>,
    pub architectures: Vec<String>,
    pub compiled: bool,
}

#[derive(Serialize)]
pub struct RuntimesResponse {
    pub total: usize,
    pub languages: HashMap<String, Vec<RuntimeInfo>>,
}

#[derive(Serialize)]
pub struct StatsResponse {
    pub uptime_seconds: u64,
    pub jobs_submitted: u64,
    pub jobs_completed: u64,
    pub jobs_failed: u64,
    pub jobs_in_flight: u64,
    pub compile_in_flight: u64,
    pub execute_in_flight: u64,
    pub max_queue_depth: u64,
    pub installed_runtimes: usize,
    pub supported_languages: Vec<String>,
    pub worker_concurrency: usize,
    pub compile_concurrency: usize,
    pub execute_concurrency: usize,
    pub max_queue_wait_secs: u64,
    pub host_arch: String,
}

pub fn router(state: ApiState) -> Router {
    // Strict Rate limiting: 1 request per second per IP, burst of 3.
    // Uses SmartIpKeyExtractor to respect X-Forwarded-For / X-Real-IP
    // headers from reverse proxies.
    //
    // NOTE: GovernorConfigBuilder::per_second(n) means "replenish 1 token
    // every n seconds", NOT "n tokens per second".
    let strict_conf = GovernorConfigBuilder::default()
        .per_second(1)
        .burst_size(3)
        .key_extractor(SmartIpKeyExtractor)
        .finish()
        .unwrap();

    // Background cleanup of expired rate-limit entries.
    let strict_limiter = strict_conf.limiter().clone();
    let interval = Duration::from_secs(60);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(interval);
            strict_limiter.retain_recent();
        }
    });

    // General Rate limiting: 5 requests per second per IP, burst of 10.
    // per_millisecond(200) = 1 token every 200ms = 5 tokens/second.
    let general_conf = GovernorConfigBuilder::default()
        .per_millisecond(200)
        .burst_size(10)
        .key_extractor(SmartIpKeyExtractor)
        .finish()
        .unwrap();

    // Background cleanup of expired rate-limit entries.
    let general_limiter = general_conf.limiter().clone();
    let interval = Duration::from_secs(60);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(interval);
            general_limiter.retain_recent();
        }
    });

    // Define the "General" routes
    let general_routes = Router::new()
        .route("/health", get(health))
        .route("/jobs/{id}", get(get_job))
        .route("/runtimes", get(list_runtimes))
        .route("/runtimes/{language}", get(get_language_runtimes))
        .route("/stats", get(get_stats))
        .layer(GovernorLayer::new(general_conf));

    // Define the "Strict" routes
    let strict_routes = Router::new()
        .route("/jobs", post(submit_job))
        .layer(GovernorLayer::new(strict_conf));

    // Merge them together
    Router::new()
        .merge(general_routes)
        .merge(strict_routes)
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn submit_job(
    State(state): State<ApiState>,
    Json(mut request): Json<JobRequest>,
) -> Result<(StatusCode, Json<SubmitJobResponse>), (StatusCode, String)> {
    // Backpressure: reject if too many jobs are already in-flight.
    let in_flight = state.jobs_in_flight.load(Ordering::Relaxed);
    if in_flight >= state.max_queue_depth {
        warn!(
            in_flight = in_flight,
            max = state.max_queue_depth,
            "rejecting job: queue full"
        );
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "server is overloaded: {} jobs in flight (max: {})",
                in_flight, state.max_queue_depth
            ),
        ));
    }

    validate_job_request(&request).map_err(|e| {
        warn!(language = %request.language, reason = %e.1, "job submission rejected: validation");
        e
    })?;

    let requested = request
        .version
        .clone()
        .ok_or((StatusCode::BAD_REQUEST, "version is required".to_string()))?;

    let resolved = state
        .resolver
        .resolve(&request.language, &requested)
        .map_err(internal_error)?
        .ok_or((
            StatusCode::BAD_REQUEST,
            format!(
                "runtime not installed or unsupported: {}:{}",
                request.language, requested
            ),
        ))?;

    let manifest_key = format!("{}:{}", request.language, resolved);
    let manifest = state.manifests.get(&manifest_key).ok_or((
        StatusCode::BAD_REQUEST,
        format!("runtime is not installed: {}", manifest_key),
    ))?;

    // Determine job type based on whether the runtime has a compile step.
    let job_type = if manifest.compile.is_some() {
        crate::queue::JobType::Compile
    } else {
        crate::queue::JobType::Execute
    };

    let job_id = request
        .job_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let language = request.language.clone();
    request.job_id = Some(job_id.clone());
    request.version = Some(resolved.clone());

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let job = QueuedJob {
        id: job_id.clone(),
        language: language.clone(),
        version: resolved.clone(),
        request,
        enqueued_at: now_ms,
        job_type,
    };

    state
        .storage
        .lock()
        .await
        .push(job)
        .await
        .map_err(internal_error)?;

    // Track in-flight job count for backpressure.
    state.jobs_in_flight.fetch_add(1, Ordering::Relaxed);

    let queued_state = JobStateRecord {
        job_id: job_id.clone(),
        status: "queued".to_string(),
        language,
        version: resolved.clone(),
        result: None,
        error: None,
        queue_wait_ms: None,
    };

    let state_key = job_state_key(&state.job_state_prefix, &job_id);
    let payload = serde_json::to_string(&queued_state).map_err(internal_error)?;
    {
        let mut conn = state.redis_pool.get().await.map_err(internal_error)?;
        let _: () = conn
            .set::<_, _, ()>(&state_key, &payload)
            .await
            .map_err(internal_error)?;
        let _: () = conn
            .expire::<_, ()>(&state_key, JOB_STATE_TTL_SECS)
            .await
            .map_err(internal_error)?;
    }

    state.jobs_submitted.fetch_add(1, Ordering::Relaxed);

    info!(
        job_id = %job_id,
        language = %queued_state.language,
        version = %resolved,
        "job submitted"
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(SubmitJobResponse {
            job_id,
            status: "queued".to_string(),
            resolved_version: resolved,
        }),
    ))
}

async fn get_job(
    Path(id): Path<String>,
    State(state): State<ApiState>,
) -> Result<(StatusCode, Json<JobStateRecord>), (StatusCode, String)> {
    let key = job_state_key(&state.job_state_prefix, &id);
    let mut conn = state.redis_pool.get().await.map_err(internal_error)?;
    let raw: Option<String> = conn
        .get::<_, Option<String>>(&key)
        .await
        .map_err(internal_error)?;

    let Some(raw) = raw else {
        return Err((StatusCode::NOT_FOUND, format!("job not found: {}", id)));
    };

    let parsed: JobStateRecord = serde_json::from_str(&raw).map_err(internal_error)?;
    Ok((StatusCode::OK, Json(parsed)))
}

async fn list_runtimes(State(state): State<ApiState>) -> Json<RuntimesResponse> {
    let languages = build_runtime_map(&state.manifests);
    let total: usize = languages.values().map(|v| v.len()).sum();
    Json(RuntimesResponse { total, languages })
}

async fn get_language_runtimes(
    Path(language): Path<String>,
    State(state): State<ApiState>,
) -> Result<Json<RuntimesResponse>, (StatusCode, String)> {
    let all = build_runtime_map(&state.manifests);
    let filtered: HashMap<String, Vec<RuntimeInfo>> = all
        .into_iter()
        .filter(|(lang, _)| lang == &language)
        .collect();

    if filtered.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no installed runtimes for language: {}", language),
        ));
    }

    let total: usize = filtered.values().map(|v| v.len()).sum();
    Ok(Json(RuntimesResponse {
        total,
        languages: filtered,
    }))
}

async fn get_stats(State(state): State<ApiState>) -> Json<StatsResponse> {
    let uptime = state.start_time.elapsed();
    let mut langs: Vec<String> = state
        .manifests
        .values()
        .map(|m| m.language.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    langs.sort();

    Json(StatsResponse {
        uptime_seconds: uptime.as_secs(),
        jobs_submitted: state.jobs_submitted.load(Ordering::Relaxed),
        jobs_completed: state.jobs_completed.load(Ordering::Relaxed),
        jobs_failed: state.jobs_failed.load(Ordering::Relaxed),
        jobs_in_flight: state.jobs_in_flight.load(Ordering::Relaxed),
        compile_in_flight: state.compile_in_flight.load(Ordering::Relaxed),
        execute_in_flight: state.execute_in_flight.load(Ordering::Relaxed),
        max_queue_depth: state.max_queue_depth,
        installed_runtimes: state.manifests.len(),
        supported_languages: langs,
        worker_concurrency: state.worker_concurrency,
        compile_concurrency: state.compile_concurrency,
        execute_concurrency: state.execute_concurrency,
        max_queue_wait_secs: state.max_queue_wait_secs,
        host_arch: std::env::consts::ARCH.to_string(),
    })
}

fn build_runtime_map(manifests: &SharedManifests) -> HashMap<String, Vec<RuntimeInfo>> {
    let mut map: HashMap<String, Vec<RuntimeInfo>> = HashMap::new();
    for manifest in manifests.values() {
        let mut archs: Vec<String> = manifest.runtimes.keys().cloned().collect();
        archs.sort();
        let info = RuntimeInfo {
            version: manifest.version.clone(),
            aliases: manifest.aliases.clone(),
            architectures: archs,
            compiled: manifest.compile.is_some(),
        };
        map.entry(manifest.language.clone()).or_default().push(info);
    }

    // Sort versions within each language for consistent output
    for versions in map.values_mut() {
        versions.sort_by(|a, b| a.version.cmp(&b.version));
    }

    map
}

fn internal_error<E: std::fmt::Display>(err: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc, sync::atomic::AtomicU64, time::Instant};

    use apalis_redis::RedisStorage;
    use axum::{extract::Path, extract::State, http::StatusCode};
    use deadpool_redis::{Config as PoolConfig, Runtime as PoolRuntime, redis::AsyncCommands};
    use mini_redis::server;
    use tokio::{
        net::TcpListener,
        sync::{Mutex, oneshot},
    };

    use super::{ApiState, get_job};
    use crate::queue::{JobStateRecord, QueuedJob, job_state_key};
    use jet_pack::{RedisVersionStore, VersionResolver};

    async fn setup_state() -> (ApiState, oneshot::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test redis listener");
        let addr = listener.local_addr().expect("local addr");
        let redis_url = format!("redis://{}/", addr);

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = server::run(listener, async move {
                let _ = shutdown_rx.await;
            })
            .await;
        });

        let resolver = VersionResolver::new(
            RedisVersionStore::new(&redis_url, "test:versions").expect("resolver store"),
        );
        let queue_conn = apalis_redis::connect(redis_url.clone())
            .await
            .expect("queue conn");

        // Create a deadpool-redis connection pool for tests.
        let pool_cfg = PoolConfig::from_url(&redis_url);
        let pool = pool_cfg
            .create_pool(Some(PoolRuntime::Tokio1))
            .expect("pool creation");

        let state = ApiState {
            resolver: Arc::new(resolver),
            manifests: Arc::new(HashMap::new()),
            storage: Arc::new(Mutex::new(RedisStorage::<QueuedJob>::new(queue_conn))),
            redis_pool: pool,
            job_state_prefix: "jet:test:jobs".to_string(),
            start_time: Instant::now(),
            jobs_submitted: Arc::new(AtomicU64::new(0)),
            jobs_completed: Arc::new(AtomicU64::new(0)),
            jobs_failed: Arc::new(AtomicU64::new(0)),
            worker_concurrency: 1,
            jobs_in_flight: Arc::new(AtomicU64::new(0)),
            max_queue_depth: 100,
            compile_in_flight: Arc::new(AtomicU64::new(0)),
            execute_in_flight: Arc::new(AtomicU64::new(0)),
            compile_concurrency: 1,
            execute_concurrency: 1,
            max_queue_wait_secs: 30,
        };

        (state, shutdown_tx)
    }

    #[tokio::test]
    async fn get_job_returns_saved_state() {
        let (state, shutdown_tx) = setup_state().await;

        let record = JobStateRecord {
            job_id: "job-1".to_string(),
            status: "completed".to_string(),
            language: "python".to_string(),
            version: "3.14.3".to_string(),
            result: None,
            error: None,
            queue_wait_ms: None,
        };
        let payload = serde_json::to_string(&record).expect("serialize record");
        let key = job_state_key(&state.job_state_prefix, &record.job_id);
        {
            let mut conn = state.redis_pool.get().await.expect("pool conn");
            let _: () = conn
                .set::<_, _, ()>(&key, &payload)
                .await
                .expect("seed redis");
        }

        let (status, body) = get_job(Path("job-1".to_string()), State(state.clone()))
            .await
            .expect("get job should succeed");

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.job_id, "job-1");
        assert_eq!(body.status, "completed");

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn get_job_returns_not_found_for_unknown_id() {
        let (state, shutdown_tx) = setup_state().await;

        let err = get_job(Path("missing-job".to_string()), State(state))
            .await
            .expect_err("missing job should fail");

        assert_eq!(err.0, StatusCode::NOT_FOUND);

        let _ = shutdown_tx.send(());
    }
}
