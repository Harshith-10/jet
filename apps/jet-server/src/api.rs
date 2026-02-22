use std::{collections::HashMap, sync::Arc};

use apalis::prelude::Storage;
use apalis_redis::RedisStorage;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use jet_core::models::JobRequest;
use jet_pack::{RedisVersionStore, VersionResolver, manifest::RuntimeManifest};
use redis::{AsyncCommands, aio::ConnectionManager};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::queue::{JobStateRecord, QueuedJob, job_state_key};

pub type SharedResolver = Arc<VersionResolver<RedisVersionStore>>;
pub type SharedStorage = Arc<Mutex<RedisStorage<QueuedJob>>>;
pub type SharedManifests = Arc<HashMap<String, RuntimeManifest>>;
pub type SharedRedisConn = Arc<Mutex<ConnectionManager>>;

#[derive(Clone)]
pub struct ApiState {
    pub resolver: SharedResolver,
    pub manifests: SharedManifests,
    pub storage: SharedStorage,
    pub redis_conn: SharedRedisConn,
    pub job_state_prefix: String,
}

#[derive(serde::Serialize)]
pub struct SubmitJobResponse {
    pub job_id: String,
    pub status: String,
    pub resolved_version: String,
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/jobs", post(submit_job))
        .route("/jobs/{id}", get(get_job))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn submit_job(
    State(state): State<ApiState>,
    Json(mut request): Json<JobRequest>,
) -> Result<(StatusCode, Json<SubmitJobResponse>), (StatusCode, String)> {
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
                "unsupported language/version fragment: {}:{}",
                request.language, requested
            ),
        ))?;

    let manifest_key = format!("{}:{}", request.language, resolved);
    if !state.manifests.contains_key(&manifest_key) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("manifest not found for {}", manifest_key),
        ));
    }

    let job_id = request
        .job_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let language = request.language.clone();
    request.job_id = Some(job_id.clone());
    request.version = Some(resolved.clone());

    let job = QueuedJob {
        id: job_id.clone(),
        language: language.clone(),
        version: resolved.clone(),
        request,
    };

    state
        .storage
        .lock()
        .await
        .push(job)
        .await
        .map_err(internal_error)?;

    let queued_state = JobStateRecord {
        job_id: job_id.clone(),
        status: "queued".to_string(),
        language,
        version: resolved.clone(),
        result: None,
        error: None,
    };

    let state_key = job_state_key(&state.job_state_prefix, &job_id);
    let payload = serde_json::to_string(&queued_state).map_err(internal_error)?;
    let _: () = state
        .redis_conn
        .lock()
        .await
        .set::<String, String, ()>(state_key, payload)
        .await
        .map_err(internal_error)?;

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
    let raw: Option<String> = state
        .redis_conn
        .lock()
        .await
        .get::<String, Option<String>>(key)
        .await
        .map_err(internal_error)?;

    let Some(raw) = raw else {
        return Err((StatusCode::NOT_FOUND, format!("job not found: {}", id)));
    };

    let parsed: JobStateRecord = serde_json::from_str(&raw).map_err(internal_error)?;
    Ok((StatusCode::OK, Json(parsed)))
}

fn internal_error<E: std::fmt::Display>(err: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use apalis_redis::RedisStorage;
    use axum::{extract::Path, extract::State, http::StatusCode};
    use mini_redis::server;
    use redis::AsyncCommands;
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
        let status_conn = apalis_redis::connect(redis_url).await.expect("status conn");

        let state = ApiState {
            resolver: Arc::new(resolver),
            manifests: Arc::new(HashMap::new()),
            storage: Arc::new(Mutex::new(RedisStorage::<QueuedJob>::new(queue_conn))),
            redis_conn: Arc::new(Mutex::new(status_conn)),
            job_state_prefix: "jet:test:jobs".to_string(),
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
        };
        let payload = serde_json::to_string(&record).expect("serialize record");
        let key = job_state_key(&state.job_state_prefix, &record.job_id);
        let _: () = state
            .redis_conn
            .lock()
            .await
            .set::<String, String, ()>(key, payload)
            .await
            .expect("seed redis");

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
