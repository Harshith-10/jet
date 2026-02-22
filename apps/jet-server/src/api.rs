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
    .route("/jobs/:id", get(get_job))
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
